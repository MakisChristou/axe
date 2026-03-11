use std::fs::File;
use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::{
    primitives::{keccak256, Address, Bytes, FixedBytes},
    providers::Provider,
    sol,
    sol_types::{SolEvent, SolValue},
};
use eyre::eyre;
use futures::future::join_all;
use indicatif::ProgressBar;
use rand::Rng;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::Mutex;

use super::metrics::{LoadTestReport, TxMetrics};
use super::LoadTestArgs;
use crate::evm::{AxelarAmplifierGateway, ContractCall};
use crate::ui;

/// Solana memo program address (devnet).
pub const MEMO_PROGRAM_ADDRESS: &str = "memKnP9ex71TveNFpsFNVqAYGEe1v9uHVsHNdFPW6FY";

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
fn make_executable_payload(custom: &Option<Vec<u8>>, counter_pda: &Pubkey) -> Vec<u8> {
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

/// Run EVM->Sol load test and return metrics report.
///
/// Calls gateway.callContract() directly with ExecutablePayload-formatted payloads.
#[allow(clippy::too_many_arguments, clippy::float_arithmetic)]
pub async fn run_load_test_with_metrics<P: Provider + Clone + 'static>(
    args: &LoadTestArgs,
    gateway_addr: Address,
    signer_address: Address,
    destination_address: &str,
    provider: &P,
) -> eyre::Result<LoadTestReport> {
    let num_txs = args.num_txs.max(1) as usize;

    // Derive the memo program's counter PDA
    let memo_program_id = Pubkey::from_str(MEMO_PROGRAM_ADDRESS)
        .map_err(|e| eyre!("invalid memo program address: {e}"))?;
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let tx_output = args.output_dir.join("transactions.txt");
    if let Some(parent) = tx_output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output_file = Arc::new(Mutex::new(
        File::create(&tx_output).map_err(|e| eyre!("failed to create output file: {e}"))?,
    ));

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };

    /// Minimum delay (ms) between EVM transactions to avoid RPC rate limiting.
    const EVM_TX_STAGGER_MS: u64 = 200;

    let stagger = Duration::from_millis(EVM_TX_STAGGER_MS);
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let mut pending_tasks = Vec::new();
    let test_start = Instant::now();

    let dest_chain = args.destination_chain.clone();
    let dest_addr = destination_address.to_string();

    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));

    // Send N txs sequentially with a small stagger to avoid rate limiting
    for i in 0..num_txs {
        let tx_payload = make_executable_payload(&payload, &counter_pda);
        let output_clone = Arc::clone(&output_file);
        let metrics_clone = Arc::clone(&metrics_list);
        let dest_chain = dest_chain.clone();
        let dest_addr = dest_addr.clone();
        let provider = provider.clone();
        let gw_addr = gateway_addr;
        let signer_addr = signer_address;
        let counter = Arc::clone(&confirmed_counter);
        let sp = spinner.clone();
        let total = num_txs;

        let handle = tokio::spawn(async move {
            execute_and_record_evm(
                &provider,
                gw_addr,
                signer_addr,
                &dest_chain,
                &dest_addr,
                &tx_payload,
                output_clone,
                metrics_clone,
                counter,
                sp,
                total,
            )
            .await;
        });
        pending_tasks.push(handle);
        if i + 1 < num_txs {
            tokio::time::sleep(stagger).await;
        }
    }

    let total_submitted = pending_tasks.len() as u64;
    let test_duration = test_start.elapsed().as_secs_f64();

    join_all(pending_tasks).await;
    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!("sent {confirmed_count}/{total_submitted} confirmed"));

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();

    #[allow(clippy::cast_precision_loss)]
    let report = LoadTestReport {
        source_chain: args.source_chain.clone(),
        destination_chain: args.destination_chain.clone(),
        destination_address: dest_addr,
        num_txs: args.num_txs,
        num_keys: 1,
        total_submitted,
        total_confirmed,
        total_failed,
        test_duration_secs: test_duration,
        tps_submitted: if test_duration > 0.0 {
            total_submitted as f64 / test_duration
        } else {
            0.0
        },
        tps_confirmed: if test_duration > 0.0 {
            total_confirmed as f64 / test_duration
        } else {
            0.0
        },
        landing_rate: if total_submitted > 0 {
            total_confirmed as f64 / total_submitted as f64
        } else {
            0.0
        },
        avg_latency_ms: if latencies.is_empty() {
            None
        } else {
            Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64)
        },
        min_latency_ms: latencies.iter().min().copied(),
        max_latency_ms: latencies.iter().max().copied(),
        avg_compute_units: None,
        min_compute_units: None,
        max_compute_units: None,
        verification: None,
        transactions: metrics,
    };

    let metrics_output = args.output_dir.join("metrics.json");
    let metrics_json = serde_json::to_string_pretty(&report)?;
    std::fs::write(&metrics_output, metrics_json)?;

    ui::kv("metrics saved to", &metrics_output.display().to_string());
    ui::kv("transactions saved to", &tx_output.display().to_string());

    Ok(report)
}

/// Send a single callContract tx on the EVM gateway and record metrics.
#[allow(clippy::too_many_arguments, clippy::semicolon_outside_block)]
async fn execute_and_record_evm<P: Provider>(
    provider: &P,
    gateway_addr: Address,
    signer_address: Address,
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
    output_file: Arc<Mutex<File>>,
    metrics_list: Arc<Mutex<Vec<TxMetrics>>>,
    confirmed_counter: Arc<AtomicU64>,
    spinner: ProgressBar,
    total: usize,
) {
    let submit_start = Instant::now();
    let payload_hash = alloy::hex::encode(keccak256(payload));

    let gateway = AxelarAmplifierGateway::new(gateway_addr, provider);
    let call = gateway.callContract(
        dest_chain.to_string(),
        dest_addr.to_string(),
        Bytes::from(payload.to_vec()),
    );

    match call.send().await {
        Ok(pending) => {
            let tx_hash = *pending.tx_hash();
            match tokio::time::timeout(
                std::time::Duration::from_secs(120),
                pending.get_receipt(),
            )
            .await
            {
                Ok(Ok(receipt)) => {
                    #[allow(clippy::cast_possible_truncation)]
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

                    let metrics = TxMetrics {
                        signature: message_id.clone(),
                        submit_time_ms: 0,
                        confirm_time_ms: Some(latency_ms),
                        latency_ms: Some(latency_ms),
                        compute_units: Some(receipt.gas_used),
                        slot: receipt.block_number,
                        success: true,
                        error: None,
                        payload: payload.to_vec(),
                        payload_hash,
                        source_address: format!("{signer_address}"),
                        send_instant: Some(submit_start),
                        amplifier_timing: None,
                    };

                    {
                        let mut file = output_file.lock().await;
                        let _ = writeln!(file, "{message_id}");
                    }

                    let done = confirmed_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    spinner.set_message(format!("sending ({done}/{total} confirmed)..."));
                    metrics_list.lock().await.push(metrics);
                }
                Ok(Err(e)) => {
                    record_failure(submit_start, &e.to_string(), &metrics_list).await;
                }
                Err(_) => {
                    record_failure(submit_start, "tx timed out", &metrics_list).await;
                }
            }
        }
        Err(e) => {
            record_failure(submit_start, &e.to_string(), &metrics_list).await;
        }
    }
}

async fn record_failure(
    submit_start: Instant,
    error: &str,
    metrics_list: &Arc<Mutex<Vec<TxMetrics>>>,
) {
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = submit_start.elapsed().as_millis() as u64;
    let metrics = TxMetrics {
        signature: String::new(),
        submit_time_ms: elapsed_ms,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        success: false,
        error: Some(error.to_string()),
        payload: Vec::new(),
        payload_hash: String::new(),
        source_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    };
    metrics_list.lock().await.push(metrics);
}
