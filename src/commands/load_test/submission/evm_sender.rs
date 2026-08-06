//! EVM transaction submission.

use crate::config::ChainsConfig;
use alloy::hex;
use alloy::primitives::TxHash;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// How long to wait for an EVM tx receipt before giving up.
/// Fast chains confirm in ~8s; others typically <20s. 60s gives congested
/// networks enough room while still catching silently-dropped txs.
const EVM_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

use super::LoadTestArgs;
use super::gmp_payload::GmpPayloadEncoding;
use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity, TxMetrics};
use super::run_sizing::SustainedPlan;
use super::submitter::TransactionSubmitter;
use super::units::Wei;
use crate::config::AxelarChainContract;
use crate::evm::{ContractCall, EvmEndpoints, SenderReceiver, connect_evm_signed, send_tx_robust};
use crate::retry::retry_with_fallback_all;
use crate::types::Network;
use crate::ui;
use alloy::{
    primitives::{Address, Bytes, keccak256},
    providers::Provider,
    signers::local::PrivateKeySigner,
    sol_types::SolEvent,
};
use eyre::eyre;

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
    fallback_gas_value_wei(args.network)
}

async fn quote_route_gas(args: &LoadTestArgs) -> Option<u128> {
    let cfg = ChainsConfig::load(&args.config).await.ok()?;
    let symbol = cfg
        .chains
        .get(args.source_chain.as_ref())?
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

fn fallback_gas_value_wei(network: Network) -> u128 {
    match network {
        // devnet-amplifier relayer doesn't require gas payment
        Network::DevnetAmplifier => 0,
        _ => 20_000_000_000_000_000, // 0.02 ETH
    }
}

struct EvmSubmitter {
    /// Source RPCs in preference order, `[private, public]` — each submit
    /// builds an `EvmEndpoints` pool so a failing primary fails over.
    rpc_urls: Vec<String>,
    sender_receiver: Address,
    destination_chain: String,
    destination_address: String,
    gas_value: Wei,
    fee_mode: super::gas_mode::EvmFeeMode,
}

struct EvmSubmitJob {
    signer: PrivateKeySigner,
    payload: Vec<u8>,
}

impl TransactionSubmitter for EvmSubmitter {
    type Job = EvmSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        let submit_start = Instant::now();
        let endpoints = match EvmEndpoints::connect(&self.rpc_urls) {
            Ok(endpoints) => endpoints,
            Err(error) => return make_failure(submit_start, &error.to_string()),
        };
        super::retry::rate_limited(|| {
            execute_and_record_evm(
                &endpoints,
                &job.signer,
                SendPayloadRequest {
                    sender_receiver: self.sender_receiver,
                    destination_chain: self.destination_chain.clone(),
                    destination_address: self.destination_address.clone(),
                    payload: job.payload.clone(),
                    gas_value: self.gas_value,
                    nonce: None,
                    fee_mode: self.fee_mode,
                },
            )
        })
        .await
    }
}

struct EvmSustainedSubmitter {
    /// Source RPCs in preference order, `[private, public]` — each submit
    /// builds an `EvmEndpoints` pool so a failing primary fails over.
    rpc_urls: Vec<String>,
    sender_receiver: Address,
    destination_chain: String,
    destination_address: String,
    gas_value: Wei,
    fee_mode: super::gas_mode::EvmFeeMode,
}

struct EvmSustainedSubmitJob {
    signer: PrivateKeySigner,
    payload: Vec<u8>,
    nonce: Option<u64>,
}

impl TransactionSubmitter for EvmSustainedSubmitter {
    type Job = EvmSustainedSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        let submit_start = Instant::now();
        let endpoints = match EvmEndpoints::connect(&self.rpc_urls) {
            Ok(endpoints) => endpoints,
            Err(error) => return make_failure(submit_start, &error.to_string()),
        };
        execute_and_record_evm(
            &endpoints,
            &job.signer,
            SendPayloadRequest {
                sender_receiver: self.sender_receiver,
                destination_chain: self.destination_chain.clone(),
                destination_address: self.destination_address.clone(),
                payload: job.payload,
                gas_value: self.gas_value,
                nonce: job.nonce,
                fee_mode: self.fee_mode,
            },
        )
        .await
    }
}

/// Run EVM load test with parallel sends from derived wallets.
///
/// Derives N EVM signers from the main private key, funds them, then fires
/// all callContract() txs in parallel (one per derived wallet).
///
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    num_txs: u64,
    sender_receiver_addr: Address,
    main_key: &[u8; 32],
    rpc_urls: &[String],
    destination_address: &str,
    payload_encoding: GmpPayloadEncoding,
) -> eyre::Result<LoadTestReport> {
    let num_txs = usize::try_from(num_txs)?;
    let payload_encoder = payload_encoding.prepare(args.network);

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };

    let main_signer = PrivateKeySigner::from_bytes(&(*main_key).into())
        .map_err(|e| eyre!("invalid main EVM key: {e}"))?;
    // Multi-transport provider: funding reads/sends fail over to the public
    // RPC on transport-level errors.
    let funding_provider = connect_evm_signed(rpc_urls, main_signer.clone())?;
    let gas_value_wei: u128 = match &args.gas_value {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value_wei(args).await,
    };
    let gas_value = Wei::from_u128(gas_value_wei);

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
        keypairs::ensure_funded_evm_with_extra(&funding_provider, &main_signer, &d, gas_value)
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
            payload: payload_encoder.encode(&payload),
        })
        .collect();
    let burst = super::submitter::run_burst(
        EvmSubmitter {
            rpc_urls: rpc_urls.to_vec(),
            sender_receiver: sender_receiver_addr,
            destination_chain: dest_chain.to_string(),
            destination_address: dest_addr.clone(),
            gas_value,
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
        let mut error_counts: HashMap<String, u64> = HashMap::new();
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
            run: RunIdentity::burst(args),
            destination_address: dest_addr,
            num_txs: num_txs as u64,
            num_keys: num_txs,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    Ok(report)
}

/// Per-tx request for a single callContract via SenderReceiver.sendPayload().
/// Gas payment + callContract happen atomically in the SenderReceiver contract.
struct SendPayloadRequest {
    sender_receiver: Address,
    destination_chain: String,
    destination_address: String,
    payload: Vec<u8>,
    gas_value: Wei,
    /// When `Some`, sets the nonce directly on the tx instead of letting the
    /// fill fetch it from the RPC. Required for sustained mode because many RPC
    /// nodes (including QuikNode) do not reliably return pending-mempool txs in
    /// `eth_getTransactionCount(addr, "pending")`, causing nonce collisions when
    /// the same key fires again within 3s before its previous tx confirms.
    nonce: Option<u64>,
    /// Legacy vs 1559 fee mode for the source chain.
    fee_mode: super::gas_mode::EvmFeeMode,
}

async fn execute_and_record_evm(
    endpoints: &EvmEndpoints,
    signer: &PrivateKeySigner,
    request: SendPayloadRequest,
) -> TxMetrics {
    let SendPayloadRequest {
        sender_receiver: sender_receiver_addr,
        destination_chain: dest_chain,
        destination_address: dest_addr,
        payload,
        gas_value,
        nonce,
        fee_mode,
    } = request;
    let submit_start = Instant::now();
    let payload_hash = hex::encode(keccak256(&payload));

    // Build the calldata once, then hand the whole fill→sign→broadcast→confirm
    // sequence to `send_tx_robust`: retry + private→public fallback at every
    // phase, idempotent same-bytes re-broadcast, and lost-response landings
    // recovered by the pre-known tx hash.
    let sr = SenderReceiver::new(sender_receiver_addr, endpoints.primary().clone());
    let mut tx = sr
        .sendPayload(dest_chain, dest_addr, Bytes::from(payload.clone()))
        .value(gas_value.as_u256())
        .into_transaction_request();
    tx.from = Some(signer.address());
    tx.nonce = nonce;
    // Legacy (pre-1559) chains: send a type-0 tx with an explicit gas_price.
    if let Some(gp) = fee_mode.legacy_gas_price() {
        tx.gas_price = Some(gp);
    }

    match send_tx_robust(
        endpoints,
        signer,
        tx,
        "source sendPayload",
        EVM_RECEIPT_TIMEOUT,
    )
    .await
    {
        Ok(receipt) => {
            let tx_hash = receipt.transaction_hash;
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
                confirm_time_ms: Some(latency_ms),
                latency_ms: Some(latency_ms),
                compute_units: Some(receipt.gas_used),
                slot: receipt.block_number,
                payload,
                payload_hash,
                source_address: format!("{sender_receiver_addr}"),
                send_instant: Some(submit_start),
                ..TxMetrics::succeeded(message_id, 0)
            }
        }
        Err(e) => make_failure(submit_start, &e.to_string()),
    }
}

/// Run EVM->Sol sustained load test at a controlled TPS rate.
///
/// Uses a rotating pool of `tps * key_cycle` derived wallets, cycling keys
/// every `key_cycle` seconds. Sends `tps` txs per second for `duration_secs`.
pub(super) struct SustainedLoadRequest {
    pub args: LoadTestArgs,
    pub plan: SustainedPlan,
    pub sender_receiver: Address,
    pub main_key: [u8; 32],
    /// Source RPCs in preference order, `[private, public]`.
    pub rpc_urls: Vec<String>,
    pub destination_address: String,
    pub verify_tx: Option<mpsc::UnboundedSender<super::verify::PendingTx>>,
    pub send_done: Option<Arc<AtomicBool>>,
    pub verify_spinner_tx: oneshot::Sender<indicatif::ProgressBar>,
    pub payload_encoding: GmpPayloadEncoding,
}

struct EvmSustainedSetup {
    payload: Option<Vec<u8>>,
    gas_value: Wei,
    derived: Vec<PrivateKeySigner>,
    fee_mode: super::gas_mode::EvmFeeMode,
    nonces: Vec<u64>,
}

async fn prepare_evm_sustained(
    args: &LoadTestArgs,
    plan: SustainedPlan,
    main_key: [u8; 32],
    rpc_urls: &[String],
) -> eyre::Result<EvmSustainedSetup> {
    let payload = args
        .payload
        .as_deref()
        .map(|value| hex::decode(value.strip_prefix("0x").unwrap_or(value)))
        .transpose()?;
    let gas_value_wei = match &args.gas_value {
        Some(value) => value
            .parse()
            .map_err(|error| eyre!("invalid --gas-value: {error}"))?,
        None => default_gas_value_wei(args).await,
    };
    let gas_value = Wei::from_u128(gas_value_wei);
    let pool_size = plan.tps * plan.key_cycle;
    let derived = keypairs::derive_evm_signers(&main_key, pool_size)?;
    ui::info(&format!(
        "derived {} EVM signing keys (pool: {} tx/s × {}s cycle)",
        pool_size, plan.tps, plan.key_cycle
    ));
    let main_signer = PrivateKeySigner::from_bytes(&main_key.into())
        .map_err(|error| eyre!("invalid main EVM key: {error}"))?;
    // Multi-transport provider: funding reads/sends fail over to the public
    // RPC on transport-level errors.
    let funding_provider = connect_evm_signed(rpc_urls, main_signer.clone())?;
    let rounds = plan.duration_secs.div_ceil(plan.key_cycle as u64);
    let buffered_rounds = rounds + rounds / 5 + 1;
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        &main_signer,
        &derived,
        gas_value.saturating_mul(buffered_rounds),
    )
    .await?;
    let fee_mode = super::gas_mode::EvmFeeMode::detect(&funding_provider).await?;
    let nonce_endpoints = EvmEndpoints::connect(rpc_urls)?;
    let mut nonces = Vec::with_capacity(pool_size);
    for signer in &derived {
        let address = signer.address();
        let n = retry_with_fallback_all(
            "starting nonce",
            nonce_endpoints.providers(),
            |p| async move { p.get_transaction_count(address).await },
        )
        .await?;
        nonces.push(n);
    }
    Ok(EvmSustainedSetup {
        payload,
        gas_value,
        derived,
        fee_mode,
        nonces,
    })
}

fn sustained_spinners(
    duration_secs: u64,
    sender: oneshot::Sender<indicatif::ProgressBar>,
) -> indicatif::ProgressBar {
    let multi = indicatif::MultiProgress::new();
    let style = ui::progress_spinner_style("  {spinner:.cyan} {msg}")
        .tick_strings(&["|", "/", "-", "\\", ""]);
    let send_spinner = multi.add(indicatif::ProgressBar::new_spinner());
    send_spinner.set_style(style.clone());
    send_spinner.enable_steady_tick(Duration::from_millis(100));
    send_spinner.set_message(format!("[0/{duration_secs}s] starting sustained send..."));
    let verify_spinner = multi.add(indicatif::ProgressBar::new_spinner());
    verify_spinner.set_style(style);
    verify_spinner.enable_steady_tick(Duration::from_millis(100));
    verify_spinner.set_message("pipeline: waiting for src-confirmed txs...");
    let _ = sender.send(verify_spinner);
    send_spinner
}

pub(super) async fn run_sustained_load_test_with_metrics(
    request: SustainedLoadRequest,
) -> eyre::Result<LoadTestReport> {
    let SustainedLoadRequest {
        args,
        plan:
            plan @ SustainedPlan {
                tps,
                duration_secs,
                key_cycle,
            },
        sender_receiver: sender_receiver_addr,
        main_key,
        rpc_urls,
        destination_address,
        verify_tx,
        send_done,
        verify_spinner_tx,
        payload_encoding,
    } = request;
    let pool_size = tps * key_cycle;
    let total_expected = plan.total_transactions();

    let setup = prepare_evm_sustained(&args, plan, main_key, &rpc_urls).await?;
    let EvmSustainedSetup {
        payload,
        gas_value,
        derived,
        fee_mode,
        nonces,
    } = setup;
    let payload_encoder = payload_encoding.prepare(args.network);

    // Create MultiProgress AFTER funding so spinners don't flicker during setup.
    let spinner = sustained_spinners(duration_secs, verify_spinner_tx);

    let dest_chain = args.destination_chain.clone();
    let dest_addr = destination_address.to_string();
    let cfg = ChainsConfig::load(&args.config).await?;
    let has_voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, &args.source_chain)
        .is_ok();
    let source_chain = args.source_axelar_id.to_string();
    let network = args.network;
    let (src_legacy, dst_legacy) =
        super::verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    let legacy_route = src_legacy || dst_legacy;

    let make_task = super::sustained::submission_tasks(
        EvmSustainedSubmitter {
            rpc_urls,
            sender_receiver: sender_receiver_addr,
            destination_chain: dest_chain.to_string(),
            destination_address: dest_addr,
            gas_value,
            fee_mode,
        },
        move |key_index, nonce| EvmSustainedSubmitJob {
            signer: derived[key_index].clone(),
            payload: payload_encoder.encode(&payload),
            nonce,
        },
        verify_tx,
        super::sustained::GmpPendingTxAdapter {
            source_chain,
            has_voting_verifier,
            source_type: super::verify::SourceChainType::Evm,
            network,
            legacy: legacy_route,
        },
    );

    let result = super::sustained::run_sustained_loop(
        SustainedPlan {
            tps,
            duration_secs,
            key_cycle,
        },
        Some(nonces),
        make_task,
        send_done,
        spinner,
    )
    .await?;

    Ok(super::sustained::build_sustained_report(
        result,
        RunIdentity::sustained(&args, plan),
        &destination_address,
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
    tx_hash: Option<TxHash>,
) -> TxMetrics {
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics::failed(
        tx_hash.map_or_else(String::new, |h| format!("{h:#x}")),
        elapsed_ms,
        error,
    )
}
