//! XRPL ITS sender. Mirrors the shape of `evm_sender.rs` and `sol_sender.rs`
//! for XRPL-sourced ITS `interchain_transfer` Payment transactions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use eyre::{Result, eyre};
use futures::future::join_all;
use indicatif::ProgressBar;
use tokio::sync::Mutex;

use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::sustained;
use crate::ui;
use crate::xrpl::{LAST_LEDGER_SEQUENCE_BUMP, XrplClient, XrplWallet, build_its_transfer_memos};
use xrpl_api::SubmitRequest;
use xrpl_binary_codec::{serialize, sign::sign_transaction};
use xrpl_types::{AccountId, Amount, Blob, PaymentTransaction};

/// Net transfer amount (drops) that should arrive at the destination after
/// the relayer subtracts `gas_fee_amount` from the on-wire payment. The
/// on-wire `Payment.Amount` must therefore be `gas_fee_drops + NET_TRANSFER_DROPS`;
/// callers compute it dynamically because hub-routed ITS doubles the per-command
/// gas, and a fixed total would otherwise underflow.
pub const NET_TRANSFER_DROPS: u64 = 50_000; // 0.05 XRP

/// Gas fee in drops, memoed as `gas_fee_amount` and deducted by the relayer.
/// Observed on-chain spend ~0.043 XRP; leaving 0.1 XRP gives comfortable
/// headroom and the excess is auto-refunded to the source XRPL wallet.
pub const DEFAULT_GAS_FEE_DROPS: u64 = 100_000; // 0.1 XRP

/// Build, sign and submit a single `interchain_transfer` Payment.
/// Returns (tx_hash_uppercase, metrics).
struct TransferSubmission<'a> {
    client: &'a XrplClient,
    wallet: &'a XrplWallet,
    destination_multisig: &'a AccountId,
    total_drops: u64,
    gas_fee_drops: u64,
    destination_chain: &'a str,
    destination_address_hex: &'a str,
    payload: Option<&'a [u8]>,
    payload_hash_hex: &'a str,
    gmp_dest_chain: &'a str,
    gmp_dest_address: &'a str,
}

async fn submit_single(submission: TransferSubmission<'_>) -> TxMetrics {
    let TransferSubmission {
        client,
        wallet,
        destination_multisig,
        total_drops,
        gas_fee_drops,
        destination_chain,
        destination_address_hex,
        payload,
        payload_hash_hex,
        gmp_dest_chain,
        gmp_dest_address,
    } = submission;
    let submit_start = Instant::now();
    let source_addr = wallet.address();

    // Build + sign the tx locally (so we can compute the deterministic hash
    // before the submit call, matching the behaviour of the XRPL relayer).
    let amount = match Amount::drops(total_drops) {
        Ok(a) => a,
        Err(e) => return fail_metrics(submit_start, &source_addr, &format!("amount: {e}")),
    };
    let mut tx = PaymentTransaction::new(wallet.account_id, amount, *destination_multisig);
    tx.common.memos = build_its_transfer_memos(
        destination_chain,
        destination_address_hex,
        gas_fee_drops,
        payload,
    )
    .into_iter()
    .map(|m| xrpl_types::Memo {
        memo_type: m.memo_type,
        memo_data: Blob(m.memo_data.0.clone()),
        memo_format: m.memo_format,
    })
    .collect();

    if let Err(e) = client.inner().prepare_transaction(&mut tx.common).await {
        return fail_metrics(submit_start, &source_addr, &format!("prepare: {e}"));
    }

    // The SDK's `prepare_transaction` sets `LastLedgerSequence = validated + 4`,
    // which is only ~16 seconds and frequently expires before inclusion under
    // any congestion or a one-ledger delay. Extend the window via the shared
    // `LAST_LEDGER_SEQUENCE_BUMP` (xrpl.js autofill defaults to +20; we use
    // +26 to be safe at load-test volumes).
    if let Some(lls) = tx.common.last_ledger_sequence {
        tx.common.last_ledger_sequence = Some(lls.saturating_add(LAST_LEDGER_SEQUENCE_BUMP));
    }

    if let Err(e) = sign_transaction(&mut tx, &wallet.public_key, &wallet.secret_key) {
        return fail_metrics(submit_start, &source_addr, &format!("sign: {e:?}"));
    }

    let tx_bytes = match serialize::serialize(&tx) {
        Ok(b) => b,
        Err(e) => return fail_metrics(submit_start, &source_addr, &format!("serialize: {e:?}")),
    };
    let tx_blob = hex::encode_upper(&tx_bytes);
    let tx_hash = {
        let h = xrpl_binary_codec::hash::hash(
            xrpl_binary_codec::hash::HASH_PREFIX_SIGNED_TRANSACTION,
            &tx_bytes,
        );
        h.to_hex()
    };

    let req = SubmitRequest::new(tx_blob).fail_hard(true);
    match client.inner().call(req).await {
        Ok(resp) => {
            let engine = format!("{:?}", resp.engine_result);
            if !engine.contains("tesSUCCESS") {
                return fail_metrics(
                    submit_start,
                    &source_addr,
                    &format!("submit rejected: {engine}: {}", resp.engine_result_message),
                );
            }
        }
        Err(e) => return fail_metrics(submit_start, &source_addr, &format!("submit: {e}")),
    }

    let submit_time_ms = submit_start.elapsed().as_millis() as u64;

    // XRPL message IDs are `0x{lowercase-hex-tx-hash}` per the
    // `hex_tx_hash` msg_id format of the XrplVotingVerifier.
    let message_id = format!("0x{}", tx_hash.to_lowercase());

    TxMetrics {
        signature: message_id,
        submit_time_ms,
        confirm_time_ms: Some(submit_time_ms),
        latency_ms: Some(submit_time_ms),
        compute_units: None,
        slot: None,
        success: true,
        error: None,
        payload: payload.map(|p| p.to_vec()).unwrap_or_default(),
        payload_hash: payload_hash_hex.to_string(),
        source_address: source_addr,
        gmp_destination_chain: gmp_dest_chain.to_string(),
        gmp_destination_address: gmp_dest_address.to_string(),
        send_instant: Some(submit_start),
        amplifier_timing: None,
    }
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
        success: false,
        error: Some(err.to_string()),
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

/// Run a burst-mode XRPL ITS load test: fires one Payment per derived
/// ephemeral wallet, in parallel.
pub(super) struct BurstRequest<'a> {
    pub client: &'a XrplClient,
    pub wallets: &'a [XrplWallet],
    pub destination_multisig: &'a AccountId,
    pub destination_chain: &'a str,
    pub destination_address_hex: &'a str,
    pub gas_fee_drops: u64,
    pub gmp_dest_chain: &'a str,
    pub gmp_dest_address: &'a str,
    pub source_chain: &'a str,
    pub destination_chain_label: &'a str,
}

pub async fn run_burst(request: BurstRequest<'_>) -> Result<LoadTestReport> {
    let BurstRequest {
        client,
        wallets,
        destination_multisig,
        destination_chain,
        destination_address_hex,
        gas_fee_drops,
        gmp_dest_chain,
        gmp_dest_address,
        source_chain,
        destination_chain_label,
    } = request;
    let key_count = wallets.len();
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{key_count} confirmed)..."));
    let test_start = Instant::now();

    let client = Arc::new(client.clone());
    let multisig = *destination_multisig;

    let mut tasks = Vec::with_capacity(key_count);
    for w in wallets {
        let c = Arc::clone(&client);
        let w = w.clone();
        let dc = destination_chain.to_string();
        let da = destination_address_hex.to_string();
        let gmp_c = gmp_dest_chain.to_string();
        let gmp_a = gmp_dest_address.to_string();
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed);
        let sp = spinner.clone();
        let total = key_count;
        let total_drops = gas_fee_drops.saturating_add(NET_TRANSFER_DROPS);
        let handle = tokio::spawn(async move {
            let m = submit_single(TransferSubmission {
                client: &c,
                wallet: &w,
                destination_multisig: &multisig,
                total_drops,
                gas_fee_drops,
                destination_chain: &dc,
                destination_address_hex: &da,
                payload: None,
                payload_hash_hex: "",
                gmp_dest_chain: &gmp_c,
                gmp_dest_address: &gmp_a,
            })
            .await;
            if m.success {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!("sending ({done}/{total} confirmed)..."));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    Ok(build_burst_report(
        metrics,
        source_chain,
        destination_chain_label,
        destination_address_hex,
        total_submitted,
        test_duration,
        key_count,
    ))
}

fn build_burst_report(
    metrics: Vec<TxMetrics>,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    total_submitted: u64,
    test_duration: f64,
    key_count: usize,
) -> LoadTestReport {
    LoadTestReport::from_transactions(
        ReportInput {
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
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

/// Run a sustained XRPL ITS load test. Uses the shared `sustained::run_sustained_loop`
/// and streams confirmed txs to a verification channel.
pub(super) struct SustainedRequest {
    pub client: XrplClient,
    pub wallets: Vec<XrplWallet>,
    pub destination_multisig: AccountId,
    pub destination_chain: String,
    pub destination_address_hex: String,
    pub gas_fee_drops: u64,
    pub gmp_dest_chain: String,
    pub gmp_dest_address: String,
    pub tps: usize,
    pub duration_secs: u64,
    pub key_cycle: usize,
    pub verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    pub send_done: Option<Arc<AtomicBool>>,
    pub spinner: ProgressBar,
    pub has_voting_verifier: bool,
}

pub(super) async fn run_sustained(request: SustainedRequest) -> sustained::SustainedResult {
    let SustainedRequest {
        client,
        wallets,
        destination_multisig,
        destination_chain,
        destination_address_hex,
        gas_fee_drops,
        gmp_dest_chain,
        gmp_dest_address,
        tps,
        duration_secs,
        key_cycle,
        verify_tx,
        send_done,
        spinner,
        has_voting_verifier,
    } = request;
    let client = Arc::new(client.clone());
    let wallets = Arc::new(wallets);

    let make_task: sustained::MakeTask = Box::new(move |key_idx: usize, _nonce: Option<u64>| {
        let c = Arc::clone(&client);
        let ws = Arc::clone(&wallets);
        let multisig = destination_multisig;
        let dc = destination_chain.clone();
        let da = destination_address_hex.clone();
        let gmp_c = gmp_dest_chain.clone();
        let gmp_a = gmp_dest_address.clone();
        let vtx = verify_tx.clone();
        let has_vv = has_voting_verifier;
        let gas = gas_fee_drops;

        Box::pin(async move {
            let wallet = &ws[key_idx % ws.len()];
            let total_drops = gas.saturating_add(NET_TRANSFER_DROPS);
            let mut m = submit_single(TransferSubmission {
                client: &c,
                wallet,
                destination_multisig: &multisig,
                total_drops,
                gas_fee_drops: gas,
                destination_chain: &dc,
                destination_address_hex: &da,
                payload: None,
                payload_hash_hex: "",
                gmp_dest_chain: &gmp_c,
                gmp_dest_address: &gmp_a,
            })
            .await;

            if m.success
                && let Some(ref tx_sender) = vtx
            {
                match super::verify::tx_to_pending_xrpl(&m, has_vv) {
                    Ok(pending) => {
                        let _ = tx_sender.send(pending);
                    }
                    Err(e) => {
                        m.success = false;
                        m.error = Some(format!("failed to build verification state: {e}"));
                    }
                }
            }
            m
        })
    });

    sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        None,
        make_task,
        send_done,
        spinner,
    )
    .await
}

/// Convenience wrapper: derive wallets, ensure funded, return ready-to-use
/// ephemeral wallets. `per_wallet_drops` is the funding target.
pub async fn prepare_wallets(
    client: &XrplClient,
    main_seed: &[u8; 32],
    main_wallet: Option<&XrplWallet>,
    count: usize,
    per_wallet_drops: u64,
    faucet_url: Option<&str>,
) -> Result<Vec<XrplWallet>> {
    if count == 0 {
        return Err(eyre!("XRPL load test requires at least 1 ephemeral wallet"));
    }
    let wallets = super::keypairs::derive_xrpl_wallets(main_seed, count)?;
    super::keypairs::ensure_funded_xrpl(
        client,
        main_wallet,
        &wallets,
        per_wallet_drops,
        faucet_url,
    )
    .await?;
    Ok(wallets)
}
