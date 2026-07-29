//! Stellar GMP sender. Mirrors the shape of `xrpl_sender.rs` for
//! Stellar-sourced `AxelarGateway.call_contract` invocations.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use super::metrics::{
    ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity, TxMetrics, TxOutcome,
};
use super::run_sizing::SustainedPlan;
use super::submitter::TransactionSubmitter;
use super::sustained;
use crate::stellar::{
    StellarClient, StellarWallet, scval_address_account, scval_bytes, scval_string, scval_token,
};
use crate::ui;
use alloy::primitives::keccak256;
use alloy::sol_types::SolValue;
use eyre::{Result, eyre};
use indicatif::ProgressBar;
use rand::Rng;

/// Stellar classic-op base fee in stroops, plus the simulated Soroban
/// resource fee added by the client. Set above the network's surge-pricing
/// floor so submissions don't get rejected as `txInsufficientFee` whenever
/// ledgers are >50% full. Stellar bills `min(submitted_fee, market_fee)`
/// so a generous bid only buys priority — it doesn't actually pay more
/// than the network is clearing at (current Horizon `/fee_stats` p99 is
/// ~18k stroops; 100k stroops = 0.01 XLM gives a comfortable cap).
pub const BASE_FEE: u32 = 100_000;

/// Default cross-chain gas payment, in stroops (1 XLM = 10^7 stroops).
/// `AxelarExample.send` forwards this to `AxelarGasService.pay_gas`.
/// Leaving 0 gives the "Insufficient Fee" error on Axelarscan; 10 XLM is
/// a comfortable default on testnet/devnet.
pub const DEFAULT_GAS_STROOPS: u64 = 100_000_000; // 10 XLM

pub fn parse_gas_stroops(value: Option<&str>) -> Result<u64> {
    match value {
        Some(value) => value
            .parse()
            .map_err(|error| eyre!("invalid --gas-value: {error}")),
        None => Ok(DEFAULT_GAS_STROOPS),
    }
}

/// Generate a default ABI-encoded payload compatible with EVM SenderReceiver.
pub fn make_payload(custom: &Option<Vec<u8>>) -> Vec<u8> {
    match custom {
        Some(p) => p.clone(),
        None => {
            let mut buf = [0u8; 16];
            rand::thread_rng().fill(&mut buf);
            let suffix = hex::encode(buf);
            let message = format!("hello from axe load test {suffix}");
            (message,).abi_encode_params()
        }
    }
}

/// Build and submit a single `AxelarExample.send(...)` invocation — the
/// high-level wrapper that internally pays gas via `AxelarGasService` and
/// emits the `ContractCall` event from `AxelarGateway`. Mirrors the reference
/// `axelar-contract-deployments/stellar/gmp.js` script.
struct SubmitRequest<'a> {
    client: &'a StellarClient,
    wallet: &'a StellarWallet,
    example_contract: &'a str,
    gateway_contract: &'a str,
    destination_chain: &'a str,
    destination_address: &'a str,
    payload: &'a [u8],
    gas_token_contract: &'a str,
    gas_amount_stroops: u64,
}

async fn submit_single(request: SubmitRequest<'_>) -> TxMetrics {
    let SubmitRequest {
        client,
        wallet,
        example_contract,
        gateway_contract,
        destination_chain,
        destination_address,
        payload,
        gas_token_contract,
        gas_amount_stroops,
    } = request;
    let submit_start = Instant::now();
    // The on-chain `ContractCall` event is emitted by the example contract
    // (which `AxelarExample.send` invokes internally), so the message's
    // `source_address` from the VotingVerifier's perspective is the example
    // contract's C-address — NOT the caller's G-address. Match what
    // Amplifier stored on-chain to make the voted-stage query succeed.
    let source_addr = example_contract.to_string();
    let _caller_addr = wallet.address();
    let payload_hash = hex::encode(keccak256(payload));

    let args = match build_send_args(
        wallet,
        destination_chain,
        destination_address,
        payload,
        gas_token_contract,
        gas_amount_stroops,
    ) {
        Ok(a) => a,
        Err(e) => return fail_metrics(submit_start, &source_addr, &format!("args: {e}")),
    };

    let gateway_filter = match crate::stellar::parse_contract_id(gateway_contract) {
        Ok(h) => Some(h),
        Err(e) => return fail_metrics(submit_start, &source_addr, &format!("gateway id: {e}")),
    };

    match client
        .invoke_contract(
            wallet,
            example_contract,
            "send",
            args,
            BASE_FEE,
            gateway_filter,
        )
        .await
    {
        Ok(invoked) => {
            let submit_time_ms = submit_start.elapsed().as_millis() as u64;
            // Stellar message IDs are `0x{lowercase_tx_hash}-{event_index}` per
            // the `hex_tx_hash_and_event_index` msg_id format. When
            // `AxelarExample.send` is the entrypoint, multiple events fire
            // (gas service first, then gateway emits `ContractCall`) — the
            // index is looked up dynamically from the validated tx response.
            let event_index = invoked.event_index.unwrap_or(0);
            let message_id = format!("0x{}-{event_index}", invoked.tx_hash_hex.to_lowercase());
            TxMetrics {
                signature: message_id,
                submit_time_ms,
                confirm_time_ms: Some(submit_time_ms),
                latency_ms: Some(submit_time_ms),
                compute_units: None,
                slot: None,
                outcome: TxOutcome::from_external(invoked.success, None, "tx failed on-chain"),
                payload: payload.to_vec(),
                payload_hash,
                source_address: source_addr,
                gmp_destination_chain: destination_chain.to_string(),
                gmp_destination_address: destination_address.to_string(),
                send_instant: Some(submit_start),
                amplifier_timing: None,
            }
        }
        Err(e) => fail_metrics(submit_start, &source_addr, &e.to_string()),
    }
}

struct StellarSubmitter {
    client: StellarClient,
    example_contract: String,
    gateway_contract: String,
    destination_chain: String,
    destination_address: String,
    gas_token_contract: String,
    gas_amount_stroops: u64,
}

struct StellarSubmitJob {
    wallet: StellarWallet,
    payload: Vec<u8>,
}

impl TransactionSubmitter for StellarSubmitter {
    type Job = StellarSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        submit_single(SubmitRequest {
            client: &self.client,
            wallet: &job.wallet,
            example_contract: &self.example_contract,
            gateway_contract: &self.gateway_contract,
            destination_chain: &self.destination_chain,
            destination_address: &self.destination_address,
            payload: &job.payload,
            gas_token_contract: &self.gas_token_contract,
            gas_amount_stroops: self.gas_amount_stroops,
        })
        .await
    }
}

fn build_send_args(
    wallet: &StellarWallet,
    destination_chain: &str,
    destination_address: &str,
    payload: &[u8],
    gas_token_contract: &str,
    gas_amount_stroops: u64,
) -> Result<Vec<stellar_xdr::curr::ScVal>> {
    Ok(vec![
        scval_address_account(&wallet.public_key_bytes),
        scval_string(destination_chain)?,
        scval_string(destination_address)?,
        scval_bytes(payload)?,
        scval_token(gas_token_contract, gas_amount_stroops)?,
    ])
}

fn fail_metrics(submit_start: Instant, source: &str, err: &str) -> TxMetrics {
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    TxMetrics {
        signature: String::new(),
        submit_time_ms: elapsed_ms,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        outcome: TxOutcome::failed(err.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: source.to_string(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}

// ---------------------------------------------------------------------------
// Burst mode
// ---------------------------------------------------------------------------

pub(super) struct BurstRequest<'a> {
    pub client: &'a StellarClient,
    pub wallets: &'a [StellarWallet],
    pub example_contract: String,
    pub gateway_contract: String,
    pub destination_chain: &'a str,
    pub destination_address: &'a str,
    pub payload_override: Option<Vec<u8>>,
    pub run: RunIdentity,
    pub gas_token_contract: String,
    pub gas_amount_stroops: u64,
}

pub async fn run_burst(request: BurstRequest<'_>) -> Result<LoadTestReport> {
    let BurstRequest {
        client,
        wallets,
        example_contract,
        gateway_contract,
        destination_chain,
        destination_address,
        payload_override,
        run,
        gas_token_contract,
        gas_amount_stroops,
    } = request;
    let key_count = wallets.len();
    let jobs = wallets
        .iter()
        .cloned()
        .map(|wallet| StellarSubmitJob {
            wallet,
            payload: make_payload(&payload_override),
        })
        .collect();
    let burst = super::submitter::run_burst(
        StellarSubmitter {
            client: client.clone(),
            example_contract,
            gateway_contract,
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            gas_token_contract,
            gas_amount_stroops,
        },
        jobs,
        key_count,
    )
    .await?;
    Ok(build_burst_report(
        burst.metrics,
        run,
        destination_address,
        burst.total_submitted,
        burst.test_duration_secs,
        key_count,
    ))
}

fn build_burst_report(
    metrics: Vec<TxMetrics>,
    run: RunIdentity,
    destination_address: &str,
    total_submitted: u64,
    test_duration: f64,
    key_count: usize,
) -> LoadTestReport {
    LoadTestReport::from_transactions(
        ReportInput {
            run,
            destination_address: destination_address.to_string(),
            num_txs: total_submitted,
            num_keys: key_count,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    )
}

// ---------------------------------------------------------------------------
// Sustained mode
// ---------------------------------------------------------------------------

pub(super) struct SustainedRequest {
    pub client: StellarClient,
    pub wallets: Vec<StellarWallet>,
    pub example_contract: String,
    pub gateway_contract: String,
    pub destination_chain: String,
    pub destination_address: String,
    pub payload_override: Option<Vec<u8>>,
    pub tps: usize,
    pub duration_secs: u64,
    pub key_cycle: usize,
    pub verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    pub send_done: Option<Arc<AtomicBool>>,
    pub spinner: ProgressBar,
    pub has_voting_verifier: bool,
    pub destination_contract_addr: alloy::primitives::Address,
    pub gas_token_contract: String,
    pub gas_amount_stroops: u64,
}

pub(super) async fn run_sustained(request: SustainedRequest) -> Result<sustained::SustainedResult> {
    let SustainedRequest {
        client,
        wallets,
        example_contract,
        gateway_contract,
        destination_chain,
        destination_address,
        payload_override,
        tps,
        duration_secs,
        key_cycle,
        verify_tx,
        send_done,
        spinner,
        has_voting_verifier,
        destination_contract_addr,
        gas_token_contract,
        gas_amount_stroops,
    } = request;
    let client = Arc::new(client.clone());
    let wallets = Arc::new(wallets);

    let make_task: sustained::MakeTask = Box::new(move |key_idx: usize, _nonce: Option<u64>| {
        let c = Arc::clone(&client);
        let ws = Arc::clone(&wallets);
        let ex = example_contract.clone();
        let gw = gateway_contract.clone();
        let dc = destination_chain.clone();
        let da = destination_address.clone();
        let payload = make_payload(&payload_override);
        let gas_token = gas_token_contract.clone();
        let gas = gas_amount_stroops;
        let vtx = verify_tx.clone();
        let has_vv = has_voting_verifier;
        let contract_addr = destination_contract_addr;

        Box::pin(async move {
            let wallet = &ws[key_idx % ws.len()];
            let mut m = submit_single(SubmitRequest {
                client: &c,
                wallet,
                example_contract: &ex,
                gateway_contract: &gw,
                destination_chain: &dc,
                destination_address: &da,
                payload: &payload,
                gas_token_contract: &gas_token,
                gas_amount_stroops: gas,
            })
            .await;
            if m.is_success()
                && let Some(ref tx_sender) = vtx
            {
                match super::verify::tx_to_pending_stellar(&m, has_vv, contract_addr) {
                    Ok(pending) => {
                        let _ = tx_sender.send(pending);
                    }
                    Err(e) => {
                        m.mark_failed(format!("failed to build verification state: {e}"));
                    }
                }
            }
            m
        })
    });

    sustained::run_sustained_loop(
        SustainedPlan {
            tps,
            duration_secs,
            key_cycle,
        },
        None,
        make_task,
        send_done,
        spinner,
    )
    .await
}

/// Compute the per-derived-key starting balance for mainnet bootstrap, based
/// on the run's planned gas spend. Each derived key must hold:
///   * the 1 XLM base reserve (account stays alive),
///   * the cross-chain gas value it forwards to GasService for each tx,
///   * a 3 XLM Soroban-fee buffer for the interchain_transfer call(s).
///
/// Callers pass `txs_per_key = 1` for burst mode or `key_cycle` for sustained.
#[must_use]
pub fn mainnet_per_key_balance_stroops(gas_stroops_per_tx: u64, txs_per_key: u64) -> i64 {
    const BASE_RESERVE: i64 = 10_000_000; // 1 XLM
    const SOROBAN_BUFFER: i64 = 30_000_000; // 3 XLM
    let gas: i64 = gas_stroops_per_tx
        .saturating_mul(txs_per_key)
        .try_into()
        .unwrap_or(i64::MAX);
    BASE_RESERVE
        .saturating_add(SOROBAN_BUFFER)
        .saturating_add(gas)
}

/// Derive deterministic Stellar wallets from a 32-byte main seed.
/// Uses the same `keccak256(main_seed || index)` pattern as the rest of axe
/// (Solana/EVM/XRPL) so re-runs recover the same ephemeral accounts.
pub fn derive_wallets(main_seed: &[u8; 32], count: usize) -> Result<Vec<StellarWallet>> {
    // Single sender → main wallet directly (no ephemeral account to fund or to
    // leave its ~1 XLM base reserve parked in).
    if count <= 1 {
        return Ok((0..count)
            .map(|_| StellarWallet::from_seed(main_seed))
            .collect());
    }
    (0..count)
        .map(|i| {
            let mut seed_input = Vec::with_capacity(40);
            seed_input.extend_from_slice(main_seed);
            seed_input.extend_from_slice(&(i as u64).to_le_bytes());
            let hash = keccak256(&seed_input);
            Ok(StellarWallet::from_seed(hash.as_ref()))
        })
        .collect()
}

/// Ensure all derived wallets are activated (have a minimum XLM balance).
/// Testnet/futurenet: use Friendbot. Mainnet: classic `CreateAccount` op
/// signed by `main_wallet` for each missing key, funded with
/// `mainnet_starting_balance_stroops` so the derived key can pay the
/// cross-chain gas value + Soroban resource fees the run plans to spend.
pub async fn ensure_funded(
    client: &StellarClient,
    derived: &[StellarWallet],
    use_friendbot: bool,
    main_wallet: &StellarWallet,
    mainnet_starting_balance_stroops: i64,
) -> Result<()> {
    let check_pb = indicatif::ProgressBar::new(derived.len() as u64);
    check_pb.set_style(
        indicatif::ProgressStyle::with_template(
            "  {bar:40.cyan/dim} {pos}/{len} Stellar keys checked",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    // missing: account doesn't exist yet (needs CreateAccount).
    // underfunded: account exists but balance < required (needs Payment top-up).
    // Only the mainnet branch consumes `underfunded`; testnet/futurenet
    // Friendbot grants 10,000 XLM per call so any existing account is always
    // over-budget for our tests.
    let mut missing: Vec<usize> = Vec::new();
    let mut underfunded: Vec<(usize, i64)> = Vec::new();
    for (i, w) in derived.iter().enumerate() {
        match client.native_balance_stroops(&w.address()).await? {
            None => missing.push(i),
            Some(bal) if !use_friendbot && bal < mainnet_starting_balance_stroops => {
                underfunded.push((i, mainnet_starting_balance_stroops - bal));
            }
            Some(_) => {}
        }
        check_pb.inc(1);
    }
    check_pb.finish_and_clear();

    if missing.is_empty() && underfunded.is_empty() {
        ui::success(&format!(
            "all {} derived Stellar keys are activated and funded",
            derived.len()
        ));
        return Ok(());
    }

    if use_friendbot {
        ui::info(&format!(
            "funding {}/{} Stellar keys via Friendbot...",
            missing.len(),
            derived.len()
        ));
        let pb = indicatif::ProgressBar::new(missing.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::with_template(
                "  {bar:40.cyan/dim} {pos}/{len} Stellar keys funded",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        for &i in &missing {
            client
                .friendbot_fund(&derived[i].address())
                .await
                .map_err(|e| eyre!("friendbot fund failed for key {i}: {e}"))?;
            pb.inc(1);
        }
        pb.finish_and_clear();
        ui::success(&format!(
            "funded {} Stellar keys via Friendbot",
            missing.len()
        ));
        return Ok(());
    }

    // Mainnet path: main wallet bootstraps each missing derived key via a
    // classic CreateAccount op + tops up under-funded ones via Payment.
    // Sequenced one at a time because each op increments the funder's
    // sequence number.
    if !missing.is_empty() {
        ui::info(&format!(
            "creating {}/{} Stellar derived accounts from main wallet ({} XLM each)...",
            missing.len(),
            derived.len(),
            mainnet_starting_balance_stroops / 10_000_000,
        ));
        let pb = indicatif::ProgressBar::new(missing.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::with_template(
                "  {bar:40.cyan/dim} {pos}/{len} Stellar accounts created",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        for &i in &missing {
            client
                .create_account_classic(
                    main_wallet,
                    &derived[i].public_key_bytes,
                    mainnet_starting_balance_stroops,
                )
                .await
                .map_err(|e| eyre!("create_account failed for derived key {i}: {e}"))?;
            pb.inc(1);
        }
        pb.finish_and_clear();
        ui::success(&format!(
            "created {} Stellar derived accounts from main wallet",
            missing.len()
        ));
    }
    if !underfunded.is_empty() {
        ui::info(&format!(
            "topping up {} under-funded Stellar derived account(s) from main wallet...",
            underfunded.len()
        ));
        let pb = indicatif::ProgressBar::new(underfunded.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::with_template(
                "  {bar:40.cyan/dim} {pos}/{len} Stellar accounts topped up",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        for &(i, top_up) in &underfunded {
            client
                .pay_native_classic(main_wallet, &derived[i].public_key_bytes, top_up)
                .await
                .map_err(|e| eyre!("top-up payment failed for derived key {i}: {e}"))?;
            pb.inc(1);
        }
        pb.finish_and_clear();
        ui::success(&format!(
            "topped up {} Stellar derived account(s) from main wallet",
            underfunded.len()
        ));
    }
    Ok(())
}
