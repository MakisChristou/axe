use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::eyre;
use futures::future::join_all;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::signer::Signer;
use tokio::sync::Mutex;

use alloy::primitives::keccak256;
use alloy::sol_types::SolValue;
use rand::Rng;

use super::metrics::{LoadTestReport, TxMetrics};
use super::LoadTestArgs;
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
            let message = format!("hello from axelar-evm-deployer load test {suffix}");
            (message,).abi_encode_params()
        }
    }
}

/// Solana devnet RPC endpoint.
const SOLANA_RPC: &str = "https://api.devnet.solana.com";

/// Contention testing mode.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum ContentionMode {
    /// Round-robin across derived keypairs (default)
    #[default]
    None,
    /// All transactions from a single keypair
    SingleAccount,
    /// Fire all transactions in parallel without delay
    Parallel,
}

/// Run load test and return metrics report.
#[allow(clippy::too_many_lines, clippy::float_arithmetic)]
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    destination_address: &str,
) -> eyre::Result<LoadTestReport> {
    ui::kv("duration", &format!("{}s", args.time));
    ui::kv("delay", &format!("{}ms", args.delay));
    ui::kv("contention mode", &format!("{:?}", args.contention_mode));

    let tx_output = args.output_dir.join("transactions.txt");
    if let Some(parent) = tx_output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output_file = Arc::new(Mutex::new(
        File::create(&tx_output).map_err(|e| eyre!("failed to create output file: {e}"))?,
    ));

    let keypairs = if let (Some(mnemonic), Some(count)) =
        (&args.mnemonic, args.addresses_to_derive)
    {
        if count == 0 {
            return Err(eyre!("--addresses-to-derive must be at least 1"));
        }
        solana::derive_keypairs_from_mnemonic(mnemonic, count)?
    } else {
        let kp = solana::load_keypair(args.keypair.as_deref())?;
        vec![Arc::new(kp) as Arc<dyn Signer + Send + Sync>]
    };

    let num_keypairs = keypairs.len();
    ui::kv("keypairs", &num_keypairs.to_string());

    // Check balances before starting
    let rpc_client = RpcClient::new_with_commitment(SOLANA_RPC, CommitmentConfig::confirmed());
    for (i, kp) in keypairs.iter().enumerate() {
        let pubkey = kp.pubkey();
        let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
        #[allow(clippy::float_arithmetic)]
        let sol = balance as f64 / 1_000_000_000.0;
        ui::kv(&format!("  wallet {i}"), &format!("{pubkey} ({sol:.4} SOL)"));
        if balance == 0 {
            return Err(eyre!(
                "wallet {i} ({pubkey}) has no SOL. Fund it first:\n  solana airdrop 2 {pubkey} --url devnet"
            ));
        }
    }

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    let duration = Duration::from_secs(args.time);
    let delay_duration = Duration::from_millis(args.delay);

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let mut pending_tasks = Vec::new();

    let mut keypair_index = 0;
    let test_start = Instant::now();
    let start_time = Instant::now();

    println!();

    match args.contention_mode {
        ContentionMode::Parallel => loop {
            if start_time.elapsed() >= duration {
                break;
            }
            for keypair in &keypairs {
                let keypair = Arc::clone(keypair);
                let dest_chain = args.destination_chain.clone();
                let dest_addr = destination_address.to_string();
                let tx_payload = make_payload(&payload);
                let output_clone = Arc::clone(&output_file);
                let metrics_clone = Arc::clone(&metrics_list);

                let handle = tokio::spawn(async move {
                    execute_and_record(
                        keypair, &dest_chain, &dest_addr, &tx_payload, output_clone,
                        metrics_clone,
                    )
                    .await;
                });
                pending_tasks.push(handle);
            }
            tokio::time::sleep(delay_duration).await;
        },
        ContentionMode::SingleAccount => {
            let single_keypair = Arc::clone(&keypairs[0]);
            loop {
                if start_time.elapsed() >= duration {
                    break;
                }
                let keypair = Arc::clone(&single_keypair);
                let dest_chain = args.destination_chain.clone();
                let dest_addr = destination_address.to_string();
                let tx_payload = make_payload(&payload);
                let output_clone = Arc::clone(&output_file);
                let metrics_clone = Arc::clone(&metrics_list);

                let handle = tokio::spawn(async move {
                    execute_and_record(
                        keypair, &dest_chain, &dest_addr, &tx_payload, output_clone,
                        metrics_clone,
                    )
                    .await;
                });
                pending_tasks.push(handle);
                tokio::time::sleep(delay_duration).await;
            }
        }
        ContentionMode::None => loop {
            if start_time.elapsed() >= duration {
                break;
            }
            if keypair_index >= keypairs.len() {
                keypair_index = 0;
            }
            let keypair = Arc::clone(&keypairs[keypair_index]);
            keypair_index += 1;

            let dest_chain = args.destination_chain.clone();
            let dest_addr = destination_address.to_string();
            let tx_payload = make_payload(&payload);
            let output_clone = Arc::clone(&output_file);
            let metrics_clone = Arc::clone(&metrics_list);

            let handle = tokio::spawn(async move {
                execute_and_record(
                    keypair, &dest_chain, &dest_addr, &tx_payload, output_clone, metrics_clone,
                )
                .await;
            });
            pending_tasks.push(handle);
            tokio::time::sleep(delay_duration).await;
        },
    }

    let total_submitted = pending_tasks.len() as u64;
    let test_duration = test_start.elapsed().as_secs_f64();

    if !pending_tasks.is_empty() {
        let spinner = ui::wait_spinner(&format!(
            "Waiting for {} pending transactions...",
            pending_tasks.len()
        ));
        join_all(pending_tasks).await;
        spinner.finish_and_clear();
    }

    let metrics = metrics_list.lock().await.clone();
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;

    let latencies: Vec<u64> = metrics.iter().filter_map(|m| m.latency_ms).collect();
    let compute_units: Vec<u64> = metrics.iter().filter_map(|m| m.compute_units).collect();

    #[allow(clippy::cast_precision_loss)]
    let report = LoadTestReport {
        destination_chain: args.destination_chain.clone(),
        destination_address: destination_address.to_string(),
        duration_secs: args.time,
        delay_ms: args.delay,
        contention_mode: format!("{:?}", args.contention_mode),
        num_keypairs,
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

    println!();
    ui::kv("total submitted", &report.total_submitted.to_string());
    ui::kv("total confirmed", &report.total_confirmed.to_string());
    ui::kv("total failed", &report.total_failed.to_string());
    ui::kv("test duration", &format!("{:.2}s", report.test_duration_secs));
    ui::kv("TPS (submitted)", &format!("{:.2}", report.tps_submitted));
    ui::kv("TPS (confirmed)", &format!("{:.2}", report.tps_confirmed));
    ui::kv(
        "landing rate",
        &format!("{:.1}%", report.landing_rate * 100.0),
    );
    if let Some(avg) = report.avg_latency_ms {
        ui::kv("avg latency", &format!("{avg:.1}ms"));
    }
    if let Some(avg) = report.avg_compute_units {
        ui::kv("avg compute units", &format!("{avg:.0}"));
    }
    ui::kv("metrics saved to", &metrics_output.display().to_string());
    ui::kv(
        "transactions saved to",
        &tx_output.display().to_string(),
    );

    Ok(report)
}

#[allow(clippy::semicolon_outside_block)]
async fn execute_and_record(
    keypair: Arc<dyn Signer + Send + Sync>,
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
    output_file: Arc<Mutex<File>>,
    metrics_list: Arc<Mutex<Vec<TxMetrics>>>,
) {
    let submit_start = Instant::now();

    let source_addr = keypair.pubkey().to_string();
    let payload_hash = alloy::hex::encode(keccak256(payload));

    match solana::send_call_contract(SOLANA_RPC, keypair.as_ref(), dest_chain, dest_addr, payload) {
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
            let sig_short = if sig.len() > 24 {
                format!("{}..{}", &sig[..16], &sig[sig.len() - 8..])
            } else {
                sig.clone()
            };
            ui::success(&format!(
                "{sig_short} ({}ms, {} CU)",
                metrics.latency_ms.unwrap_or(0),
                metrics.compute_units.unwrap_or(0)
            ));
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
            ui::error(&format!("tx failed: {e}"));
            metrics_list.lock().await.push(metrics);
        }
    }
}
