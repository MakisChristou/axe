use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// How long to wait for an EVM tx receipt before giving up.
/// Fast chains confirm in ~8s; others typically <20s. 60s gives congested
/// networks enough room while still catching silently-dropped txs.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

use super::LoadTestArgs;
use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::submitter::TransactionSubmitter;
use crate::evm::{ContractCall, SenderReceiver};
use crate::types::Network;
use crate::ui;
use alloy::{
    primitives::{Address, Bytes, FixedBytes, keccak256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
    sol_types::{SolEvent, SolValue},
};
use eyre::eyre;
use rand::Rng;
use solana_sdk::pubkey::Pubkey;

/// Solana memo program address for the target network.
pub fn memo_program_id(network: Network) -> Pubkey {
    network.solana_memo_id()
}

/// Default gas value sent with sendPayload for cross-chain gas.
///
/// Strategy: ask Axelarscan's `estimateGasFee` for a relayer-aware quote on
/// the route (× 1.5 inside the helper) and fall back to a route-agnostic
/// constant if the API can't be reached. The constant fallback was tuned
/// for ETH-priced sources and silently underpays routes where source-native
/// is cheap (XRP) or destination-native is volatile (HYPE).
async fn default_gas_value_wei(args: &LoadTestArgs) -> u128 {
    if let Some(quoted) = quote_route_gas(args).await {
        return quoted;
    }
    fallback_gas_value_wei(args.network, &args.source_chain)
}

async fn quote_route_gas(args: &LoadTestArgs) -> Option<u128> {
    let cfg = crate::config::ChainsConfig::load(&args.config).ok()?;
    let symbol = cfg
        .chains
        .get(&args.source_chain)?
        .token_symbol
        .as_deref()?;
    super::gas_estimate::estimate_route_gas(
        args.network,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        symbol,
        super::gas_estimate::DEFAULT_DEST_GAS_LIMIT,
    )
    .await
}

fn fallback_gas_value_wei(network: Network, _source_chain: &str) -> u128 {
    match network {
        // devnet-amplifier relayer doesn't require gas payment
        Network::DevnetAmplifier => 0,
        _ => 20_000_000_000_000_000, // 0.02 ETH
    }
}

// Solana ExecutablePayload ABI types (matches axelar-amplifier-solana gateway)
sol! {
    struct SolanaAccountRepr {
        bytes32 pubkey;
        bool is_signer;
        bool is_writable;
    }

    struct SolanaGatewayPayload {
        bytes execute_payload;
        SolanaAccountRepr[] accounts;
    }
}

/// Build an ExecutablePayload in ABI encoding format for the memo program.
///
/// Format: [0x01 (ABI scheme)] [ABI-encoded SolanaGatewayPayload]
/// The memo program needs the counter PDA as a writable account.
pub fn make_executable_payload(custom: &Option<Vec<u8>>, counter_pda: &Pubkey) -> Vec<u8> {
    let memo_bytes: Vec<u8> = match custom {
        Some(p) => p.clone(),
        None => {
            let mut buf = [0u8; 16];
            rand::thread_rng().fill(&mut buf);
            format!("hello from axe load test {}", hex::encode(buf)).into_bytes()
        }
    };

    let gateway_payload = SolanaGatewayPayload {
        execute_payload: memo_bytes.into(),
        accounts: vec![SolanaAccountRepr {
            pubkey: FixedBytes::from(counter_pda.to_bytes()),
            is_signer: false,
            is_writable: true,
        }],
    };

    let encoded = gateway_payload.abi_encode_params();

    // Prepend ABI encoding scheme byte (0x01)
    let mut full_payload = Vec::with_capacity(1 + encoded.len());
    full_payload.push(0x01);
    full_payload.extend(encoded);
    full_payload
}

struct EvmSubmitter {
    rpc_url: reqwest::Url,
    sender_receiver: Address,
    destination_chain: String,
    destination_address: String,
    gas_value_wei: u128,
    fee_mode: super::gas_mode::EvmFeeMode,
}

struct EvmSubmitJob {
    signer: PrivateKeySigner,
    payload: Vec<u8>,
}

impl TransactionSubmitter for EvmSubmitter {
    type Job = EvmSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        let provider = ProviderBuilder::new()
            .wallet(job.signer)
            .connect_http(self.rpc_url.clone());
        super::retry::rate_limited(|| {
            execute_and_record_evm(
                &provider,
                self.sender_receiver,
                &self.destination_chain,
                &self.destination_address,
                &job.payload,
                self.gas_value_wei,
                EvmTxOpts {
                    nonce: None,
                    fee_mode: self.fee_mode,
                },
            )
        })
        .await
    }
}

/// Run EVM load test with parallel sends from derived wallets.
///
/// Derives N EVM signers from the main private key, funds them, then fires
/// all callContract() txs in parallel (one per derived wallet).
///
/// When `evm_destination` is true, payloads are ABI-encoded strings for EVM
/// `SenderReceiver._execute`. When false, payloads use the Solana gateway format.
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    sender_receiver_addr: Address,
    main_key: &[u8; 32],
    evm_rpc_url: &str,
    destination_address: &str,
    evm_destination: bool,
) -> eyre::Result<LoadTestReport> {
    let num_txs = args.num_txs.max(1) as usize;

    // Derive the memo program's counter PDA
    let memo_program_id = memo_program_id(args.network);
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };

    let main_signer = PrivateKeySigner::from_bytes(&(*main_key).into())
        .map_err(|e| eyre!("invalid main EVM key: {e}"))?;
    let funding_provider = ProviderBuilder::new()
        .wallet(main_signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    let gas_value_wei: u128 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value_wei(args).await,
    };

    // Hedera quirk: sending HBAR to a fresh EVM address auto-creates a Hedera
    // account, but the mirror node lags before the new account is visible to
    // the JSON-RPC relay — so a tx FROM a just-funded derived key reverts
    // with "Sender account not found" during simulation. The deployment
    // scripts in axelar-contract-deployments avoid this by using a single
    // pre-existing wallet; we mirror that here. Loses parallelism for
    // num_txs > 1 on Hedera (sequential nonce), acceptable for the smoke
    // fleet which uses num_txs = 1.
    // A single tx needs no parallelism, so skip key derivation and send from
    // the main wallet directly. This avoids funding a throwaway subwallet
    // (whose leftover + the gas refund — paid to `msg.sender` — would otherwise
    // sit parked there), and sidesteps the per-key `MIN_WEI_PER_KEY` funding
    // gate, so a thin-balance source chain (cheap L2s) works without topping up.
    // Hedera always uses the main wallet (a tx from a freshly-funded key reverts
    // with "Sender account not found" while the mirror node lags).
    let derived = if args.source_axelar_id == "hedera" || num_txs == 1 {
        if num_txs == 1 {
            ui::info("single tx: sending from main wallet directly (no subwallet)");
        } else {
            ui::info("Hedera source: using main wallet directly (no key derivation)");
        }
        vec![main_signer.clone(); num_txs]
    } else {
        let d = keypairs::derive_evm_signers(main_key, num_txs)?;
        ui::info(&format!("derived {} EVM signing keys", d.len()));
        keypairs::ensure_funded_evm_with_extra(&funding_provider, &main_signer, &d, gas_value_wei)
            .await?;
        d
    };

    // Legacy (pre-1559) source chains need type-0 txs; detect once and apply
    // to each send. No-op on 1559 chains.
    let fee_mode = super::gas_mode::EvmFeeMode::detect(&funding_provider).await?;

    // Each send does multiple RPC calls (estimate gas, nonce, send, receipt),
    // so cap concurrency to avoid overwhelming the RPC.
    const MAX_CONCURRENT_SENDS: usize = 100;
    let dest_chain = args.destination_chain.clone();
    let dest_addr = destination_address.to_string();
    let jobs = derived
        .into_iter()
        .map(|signer| EvmSubmitJob {
            signer,
            payload: if evm_destination {
                super::sol_sender::make_payload(&payload)
            } else {
                make_executable_payload(&payload, &counter_pda)
            },
        })
        .collect();
    let burst = super::submitter::run_burst(
        EvmSubmitter {
            rpc_url: evm_rpc_url.parse()?,
            sender_receiver: sender_receiver_addr,
            destination_chain: dest_chain.clone(),
            destination_address: dest_addr.clone(),
            gas_value_wei,
            fee_mode,
        },
        jobs,
        MAX_CONCURRENT_SENDS,
    )
    .await?;
    let metrics = burst.metrics;
    let total_failed = metrics.iter().filter(|m| !m.is_success()).count() as u64;

    // Show error breakdown if there were failures
    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.is_success()) {
            let reason = m
                .error()
                .unwrap_or("unknown")
                .chars()
                .take(120)
                .collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    let report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: args.source_chain.clone(),
            destination_chain: args.destination_chain.clone(),
            destination_address: dest_addr,
            num_txs: args.num_txs,
            num_keys: num_txs,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    Ok(report)
}

/// Send a single callContract tx via SenderReceiver.sendPayload() and return metrics.
/// Gas payment + callContract happen atomically in the SenderReceiver contract.
///
/// `explicit_nonce`: when `Some`, sets the nonce directly on the tx instead of letting
/// alloy fetch it from the RPC. Required for sustained mode because many RPC nodes
/// (including QuikNode) do not reliably return pending-mempool txs in
/// `eth_getTransactionCount(addr, "pending")`, causing nonce collisions when the same
/// key fires again within 3s before its previous tx confirms.
/// Per-tx options for an EVM GMP send.
#[derive(Clone, Copy)]
struct EvmTxOpts {
    /// When `Some`, sets the nonce directly (sustained mode — see below).
    nonce: Option<u64>,
    /// Legacy vs 1559 fee mode for the source chain.
    fee_mode: super::gas_mode::EvmFeeMode,
}

async fn execute_and_record_evm<P: Provider>(
    provider: &P,
    sender_receiver_addr: Address,
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
    gas_value_wei: u128,
    opts: EvmTxOpts,
) -> TxMetrics {
    let submit_start = Instant::now();
    let payload_hash = alloy::hex::encode(keccak256(payload));

    let sr = SenderReceiver::new(sender_receiver_addr, provider);
    let gas_value = alloy::primitives::U256::from(gas_value_wei);
    let mut base_call = sr
        .sendPayload(
            dest_chain.to_string(),
            dest_addr.to_string(),
            Bytes::from(payload.to_vec()),
        )
        .value(gas_value);
    // Legacy (pre-1559) chains: send a type-0 tx with an explicit gas_price.
    if let Some(gp) = opts.fee_mode.legacy_gas_price() {
        base_call = base_call.gas_price(gp);
    }
    let call = match opts.nonce {
        Some(n) => base_call.nonce(n),
        None => base_call,
    };

    match call.send().await {
        Ok(pending) => {
            let tx_hash = *pending.tx_hash();
            match tokio::time::timeout(EVM_RECEIPT_TIMEOUT, pending.get_receipt()).await {
                Ok(Ok(receipt)) => {
                    let latency_ms = submit_start.elapsed().as_millis() as u64;

                    // Extract ContractCall event index
                    let event_index = receipt
                        .inner
                        .logs()
                        .iter()
                        .enumerate()
                        .find_map(|(i, log)| {
                            if log.topics().first() == Some(&ContractCall::SIGNATURE_HASH) {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);

                    // message_id format matching Axelar convention
                    let message_id = format!("{tx_hash:#x}-{event_index}");

                    TxMetrics {
                        signature: message_id,
                        submit_time_ms: 0,
                        confirm_time_ms: Some(latency_ms),
                        latency_ms: Some(latency_ms),
                        compute_units: Some(receipt.gas_used),
                        slot: receipt.block_number,
                        outcome: TxMetrics::succeeded_outcome(),
                        payload: payload.to_vec(),
                        payload_hash,
                        source_address: format!("{sender_receiver_addr}"),
                        gmp_destination_chain: String::new(),
                        gmp_destination_address: String::new(),
                        send_instant: Some(submit_start),
                        amplifier_timing: None,
                    }
                }
                Ok(Err(e)) => make_failure_with_hash(submit_start, &e.to_string(), Some(tx_hash)),
                Err(_) => make_failure_with_hash(submit_start, "tx timed out", Some(tx_hash)),
            }
        }
        Err(e) => make_failure(submit_start, &e.to_string()),
    }
}

/// Run EVM->Sol sustained load test at a controlled TPS rate.
///
/// Uses a rotating pool of `tps * key_cycle` derived wallets, cycling keys
/// every `key_cycle` seconds. Sends `tps` txs per second for `duration_secs`.
pub(super) struct SustainedLoadRequest<'a> {
    pub args: &'a LoadTestArgs,
    pub sender_receiver: Address,
    pub main_key: &'a [u8; 32],
    pub evm_rpc_url: &'a str,
    pub destination_address: &'a str,
    pub verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    pub send_done: Option<Arc<AtomicBool>>,
    pub verify_spinner_tx: tokio::sync::oneshot::Sender<indicatif::ProgressBar>,
    pub evm_destination: bool,
}

pub(super) async fn run_sustained_load_test_with_metrics(
    request: SustainedLoadRequest<'_>,
) -> eyre::Result<LoadTestReport> {
    let SustainedLoadRequest {
        args,
        sender_receiver: sender_receiver_addr,
        main_key,
        evm_rpc_url,
        destination_address,
        verify_tx,
        send_done,
        verify_spinner_tx,
        evm_destination,
    } = request;
    // `run_sustained` is only called from sustained-mode dispatch, where
    // both `tps` and `duration_secs` have already been validated as `Some`.
    let tps = args
        .tps
        .expect("run_sustained called outside sustained mode") as usize;
    let duration_secs = args
        .duration_secs
        .expect("run_sustained called outside sustained mode");
    let key_cycle = args.key_cycle as usize;
    let pool_size = tps * key_cycle;
    let total_expected = tps as u64 * duration_secs;

    let memo_program_id = memo_program_id(args.network);
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };
    let gas_value_wei: u128 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value_wei(args).await,
    };

    // Derive pool
    let derived = keypairs::derive_evm_signers(main_key, pool_size)?;
    ui::info(&format!(
        "derived {} EVM signing keys (pool: {} tx/s × {}s cycle)",
        pool_size, tps, key_cycle
    ));

    // Fund pool keys.
    let main_signer = PrivateKeySigner::from_bytes(&(*main_key).into())
        .map_err(|e| eyre!("invalid main EVM key: {e}"))?;
    let funding_provider = ProviderBuilder::new()
        .wallet(main_signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    let rounds_per_key = duration_secs.div_ceil(key_cycle as u64);
    let buffered_rounds = rounds_per_key + rounds_per_key / 5 + 1;
    let gas_total_per_key = gas_value_wei.saturating_mul(buffered_rounds as u128);
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        &main_signer,
        &derived,
        gas_total_per_key,
    )
    .await?;

    // Legacy (pre-1559) source chains need type-0 txs; detect once and apply to
    // each streamed send. No-op on 1559 chains.
    let fee_mode = super::gas_mode::EvmFeeMode::detect(&funding_provider).await?;

    // Pre-fetch initial nonces.
    let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(pool_size);
    for signer in &derived {
        let n = nonce_provider
            .get_transaction_count(signer.address())
            .await?;
        nonces.push(n);
    }

    // Create MultiProgress AFTER funding so spinners don't flicker during setup.
    let multi = indicatif::MultiProgress::new();
    let spinner_style = indicatif::ProgressStyle::with_template("  {spinner:.cyan} {msg}")
        .unwrap()
        .tick_strings(&["|", "/", "-", "\\", ""]);
    let spinner = multi.add(indicatif::ProgressBar::new_spinner());
    spinner.set_style(spinner_style.clone());
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));
    spinner.set_message(format!("[0/{duration_secs}s] starting sustained send..."));

    let verify_spinner = multi.add(indicatif::ProgressBar::new_spinner());
    verify_spinner.set_style(spinner_style);
    verify_spinner.enable_steady_tick(std::time::Duration::from_millis(100));
    verify_spinner.set_message("pipeline: waiting for src-confirmed txs...");
    let _ = verify_spinner_tx.send(verify_spinner);

    let dest_chain = args.destination_chain.clone();
    let dest_addr = destination_address.to_string();
    let rpc_url_str = evm_rpc_url.to_string();
    let cfg = crate::config::ChainsConfig::load(&args.config)?;
    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_chain)
        .is_ok();
    let source_chain = args.source_axelar_id.clone();
    let network = args.network;
    let (src_legacy, dst_legacy) =
        super::verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    let legacy_route = src_legacy || dst_legacy;

    let make_task: super::sustained::MakeTask =
        Box::new(move |key_idx: usize, nonce: Option<u64>| {
            let tx_payload = if evm_destination {
                super::sol_sender::make_payload(&payload)
            } else {
                make_executable_payload(&payload, &counter_pda)
            };
            let dc = dest_chain.clone();
            let da = dest_addr.clone();
            let sr = sender_receiver_addr;
            let gv = gas_value_wei;
            let url = rpc_url_str.clone();
            let vtx = verify_tx.clone();
            let sc = source_chain.clone();
            let has_vv = has_voting_verifier;
            let legacy = legacy_route;
            let fm = fee_mode;

            let provider = ProviderBuilder::new()
                .wallet(derived[key_idx].clone())
                .connect_http(url.parse().expect("invalid RPC URL"));

            Box::pin(async move {
                let mut result = execute_and_record_evm(
                    &provider,
                    sr,
                    &dc,
                    &da,
                    &tx_payload,
                    gv,
                    EvmTxOpts {
                        nonce,
                        fee_mode: fm,
                    },
                )
                .await;
                // Stream successful txs to the concurrent verification pipeline.
                if result.is_success()
                    && let Some(ref tx_sender) = vtx
                {
                    // Use signature length as a proxy for idx — the verify task
                    // will overwrite idx from the timings vec anyway.
                    match super::verify::tx_to_pending_solana(
                        &result,
                        0,
                        &sc,
                        has_vv,
                        super::verify::SourceChainType::Evm,
                        network,
                        legacy,
                    ) {
                        Ok(pending) => {
                            if tx_sender.send(pending).is_err() {
                                eprintln!(
                                    "warning: verification channel closed, tx won't be verified"
                                );
                            }
                        }
                        Err(e) => {
                            result.mark_failed(format!("failed to build verification state: {e}"));
                        }
                    }
                }
                result
            })
        });

    let result = super::sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        Some(nonces),
        make_task,
        send_done,
        spinner,
    )
    .await?;

    Ok(super::sustained::build_sustained_report(
        result,
        &args.source_chain,
        &args.destination_chain,
        destination_address,
        total_expected,
        pool_size,
    ))
}

fn make_failure(submit_start: Instant, error: &str) -> TxMetrics {
    make_failure_with_hash(submit_start, error, None)
}

fn make_failure_with_hash(
    submit_start: Instant,
    error: &str,
    tx_hash: Option<alloy::primitives::TxHash>,
) -> TxMetrics {
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics {
        signature: tx_hash.map_or_else(String::new, |h| format!("{h:#x}")),
        submit_time_ms: elapsed_ms,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        outcome: TxMetrics::failed_outcome(error.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}
