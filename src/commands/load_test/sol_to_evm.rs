use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use eyre::eyre;
use futures::future::join_all;
use indicatif::ProgressBar;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tokio::sync::Mutex;

use alloy::primitives::keccak256;
use alloy::sol_types::SolValue;
use rand::Rng;

use super::LoadTestArgs;
use super::keypairs;
use super::metrics::{LoadTestReport, TxMetrics};
use crate::solana;
use crate::ui;

/// Generate a unique ABI-encoded payload compatible with `SenderReceiver._execute`.
/// The contract does `abi.decode(payload_, (string))`, so we must ABI-encode the string.
fn make_payload(custom: &Option<Vec<u8>>) -> Vec<u8> {
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


/// Prepare the signing keypairs for the load test.
///
/// When `num_keys >= 2`, derives N keypairs from the main one, funds any that
/// are below the minimum balance, and returns the list. Shows progress bar
/// during funding.
///
/// When `num_keys <= 1`, returns the main keypair as the only signer.
fn prepare_keypairs(
    solana_rpc: &str,
    num_keys: usize,
    main_keypair: &Keypair,
) -> eyre::Result<Vec<Arc<dyn Signer + Send + Sync>>> {
    if num_keys <= 1 {
        return Ok(vec![Arc::new(Keypair::new_from_array(
            main_keypair.to_bytes()[..32].try_into().unwrap(),
        )) as Arc<dyn Signer + Send + Sync>]);
    }

    let derived = keypairs::derive_keypairs(main_keypair, num_keys)?;
    let balances = keypairs::ensure_funded(solana_rpc, main_keypair, &derived)?;

    #[allow(clippy::float_arithmetic)]
    let total_sol: f64 = balances.iter().sum::<u64>() as f64 / 1e9;
    ui::success(&format!(
        "funded {} keys ({:.4} SOL)",
        derived.len(),
        total_sol,
    ));

    Ok(derived
        .into_iter()
        .map(|kp| Arc::new(kp) as Arc<dyn Signer + Send + Sync>)
        .collect())
}

/// Run load test and return metrics report.
#[allow(clippy::too_many_lines, clippy::float_arithmetic)]
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    destination_address: &str,
) -> eyre::Result<LoadTestReport> {
    let num_txs = args.num_txs.max(1) as usize;

    let tx_output = args.output_dir.join("transactions.txt");
    if let Some(parent) = tx_output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output_file = Arc::new(Mutex::new(
        File::create(&tx_output).map_err(|e| eyre!("failed to create output file: {e}"))?,
    ));

    let main_keypair = solana::load_keypair(args.keypair.as_deref())?;

    // Check main wallet balance
    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        &args.solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    );
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    #[allow(clippy::float_arithmetic)]
    let sol = balance as f64 / 1e9;
    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));
    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. Fund it first:\n  solana airdrop 2 {pubkey}"
        ));
    }

    // Derive and fund keypairs (1 key per tx to avoid nonce contention)
    let keypairs = prepare_keypairs(&args.solana_rpc, num_txs, &main_keypair)?;
    let keypairs = Arc::new(keypairs);
    let key_count = keypairs.len();

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let mut pending_tasks = Vec::new();

    let test_start = Instant::now();
    let solana_rpc = args.solana_rpc.clone();

    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{key_count} confirmed)..."));

    // Fire all txs in parallel (one per keypair)
    for i in 0..key_count {
        let kp = Arc::clone(&keypairs[i]);
        let dest_chain = args.destination_chain.clone();
        let dest_addr = destination_address.to_string();
        let tx_payload = make_payload(&payload);
        let output_clone = Arc::clone(&output_file);
        let metrics_clone = Arc::clone(&metrics_list);
        let rpc = solana_rpc.clone();
        let counter = Arc::clone(&confirmed_counter);
        let sp = spinner.clone();
        let total = key_count;

        let handle = tokio::spawn(async move {
            execute_and_record(
                &rpc,
                kp,
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
    let compute_units: Vec<u64> = metrics.iter().filter_map(|m| m.compute_units).collect();

    #[allow(clippy::cast_precision_loss)]
    let report = LoadTestReport {
        source_chain: args.source_chain.clone(),
        destination_chain: args.destination_chain.clone(),
        destination_address: destination_address.to_string(),
        num_txs: args.num_txs,
        num_keys: key_count,
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
            Option::None
        } else {
            Some(latencies.iter().sum::<u64>() as f64 / latencies.len() as f64)
        },
        min_latency_ms: latencies.iter().min().copied(),
        max_latency_ms: latencies.iter().max().copied(),
        avg_compute_units: if compute_units.is_empty() {
            Option::None
        } else {
            Some(compute_units.iter().sum::<u64>() as f64 / compute_units.len() as f64)
        },
        min_compute_units: compute_units.iter().min().copied(),
        max_compute_units: compute_units.iter().max().copied(),
        verification: Option::None,
        transactions: metrics,
    };

    let metrics_output = args.output_dir.join("metrics.json");
    let metrics_json = serde_json::to_string_pretty(&report)?;
    std::fs::write(&metrics_output, metrics_json)?;

    ui::kv("metrics saved to", &metrics_output.display().to_string());
    ui::kv("transactions saved to", &tx_output.display().to_string());

    Ok(report)
}

#[allow(clippy::semicolon_outside_block, clippy::too_many_arguments)]
async fn execute_and_record(
    solana_rpc: &str,
    keypair: Arc<dyn Signer + Send + Sync>,
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

    let source_addr = keypair.pubkey().to_string();
    let payload_hash = alloy::hex::encode(keccak256(payload));

    match solana::send_call_contract(solana_rpc, keypair.as_ref(), dest_chain, dest_addr, payload) {
        Ok((sig, mut metrics)) => {
            metrics.payload = payload.to_vec();
            metrics.payload_hash = payload_hash;
            metrics.source_address = source_addr;
            metrics.send_instant = Some(submit_start);
            {
                let mut file = output_file.lock().await;
                if let Err(e) = writeln!(file, "{sig}") {
                    eprintln!("  failed to write signature to file: {e}");
                }
            }
            let done = confirmed_counter.fetch_add(1, Ordering::Relaxed) + 1;
            spinner.set_message(format!("sending ({done}/{total} confirmed)..."));
            metrics_list.lock().await.push(metrics);
        }
        Err(e) => {
            #[allow(clippy::cast_possible_truncation)]
            let elapsed_ms = submit_start.elapsed().as_millis() as u64;
            let metrics = TxMetrics {
                signature: String::new(),
                submit_time_ms: elapsed_ms,
                confirm_time_ms: Option::None,
                latency_ms: Option::None,
                compute_units: Option::None,
                slot: Option::None,
                success: false,
                error: Some(e.to_string()),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            };
            metrics_list.lock().await.push(metrics);
        }
    }
}
