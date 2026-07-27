use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use alloy::primitives::keccak256;
use alloy::sol_types::SolValue;
use eyre::eyre;
use futures::future::join_all;
use indicatif::ProgressBar;
use rand::Rng;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tokio::sync::Mutex;

use super::LoadTestArgs;
use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::sustained;
use crate::solana;
use crate::types::Network;
use crate::ui;

/// Per-network funding hint for an empty Solana wallet. `solana airdrop`
/// only works on devnet/testnet; on mainnet users have to source SOL
/// elsewhere.
fn fund_hint(network: Network, pubkey: &solana_sdk::pubkey::Pubkey) -> String {
    match network {
        Network::Mainnet => format!("Fund {pubkey} with mainnet SOL (no faucet) before retrying."),
        _ => format!("Fund it first:\n  solana airdrop 2 {pubkey}"),
    }
}

/// Generate a unique ABI-encoded payload compatible with `SenderReceiver._execute`.
/// The contract does `abi.decode(payload_, (string))`, so we must ABI-encode the string.
pub(super) fn make_payload(custom: &Option<Vec<u8>>) -> Vec<u8> {
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
///
/// When `evm_destination` is true, payloads are ABI-encoded strings for EVM
/// `SenderReceiver._execute`. When false, payloads use the Solana executable format.
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    destination_address: &str,
    evm_destination: bool,
) -> eyre::Result<LoadTestReport> {
    let num_txs = args.num_txs.max(1) as usize;

    let main_keypair = solana::load_keypair(args.keypair.as_deref())?;

    // Check main wallet balance
    let rpc_client = RpcClient::new_with_commitment(
        &args.source_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    );
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    let sol = balance as f64 / 1e9;
    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));
    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. {}",
            fund_hint(args.network, &pubkey)
        ));
    }

    // Derive and fund keypairs (1 key per tx to avoid nonce contention)
    let keypairs = prepare_keypairs(&args.source_rpc, num_txs, &main_keypair)?;
    let keypairs = Arc::new(keypairs);
    let key_count = keypairs.len();

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    let memo_program_id = super::evm_sender::memo_program_id(args.network);
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let mut pending_tasks = Vec::new();

    let test_start = Instant::now();
    let solana_rpc = args.source_rpc.clone();

    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{key_count} confirmed)..."));

    // Fire all txs in parallel (one per keypair)
    for i in 0..key_count {
        let kp = Arc::clone(&keypairs[i]);
        let dest_chain = args.destination_chain.clone();
        let dest_addr = destination_address.to_string();
        let tx_payload = if evm_destination {
            make_payload(&payload)
        } else {
            super::evm_sender::make_executable_payload(&payload, &counter_pda)
        };
        let metrics_clone = Arc::clone(&metrics_list);
        let rpc = solana_rpc.clone();
        let counter = Arc::clone(&confirmed_counter);
        let sp = spinner.clone();
        let total = key_count;
        let network = args.network;

        let handle = tokio::spawn(async move {
            execute_and_record(
                &rpc,
                kp,
                network,
                &dest_chain,
                &dest_addr,
                &tx_payload,
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
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: args.source_chain.clone(),
            destination_chain: args.destination_chain.clone(),
            destination_address: destination_address.to_string(),
            num_txs: args.num_txs,
            num_keys: key_count,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Include,
        },
        metrics,
    );

    Ok(report)
}

/// Send a single Solana callContract tx and return metrics. Used by sustained mode.
fn send_sol_tx(
    solana_rpc: &str,
    keypair: &(dyn Signer + Send + Sync),
    network: Network,
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
) -> TxMetrics {
    let submit_start = Instant::now();
    let source_addr = keypair.pubkey().to_string();
    let payload_hash = alloy::hex::encode(alloy::primitives::keccak256(payload));
    match solana::send_call_contract(solana_rpc, keypair, network, dest_chain, dest_addr, payload) {
        Ok((_sig, mut metrics)) => {
            metrics.payload = payload.to_vec();
            metrics.payload_hash = payload_hash;
            metrics.source_address = source_addr;
            metrics.send_instant = Some(submit_start);
            metrics
        }
        Err(e) => {
            let elapsed_ms = submit_start.elapsed().as_millis() as u64;
            TxMetrics {
                signature: String::new(),
                submit_time_ms: elapsed_ms,
                confirm_time_ms: None,
                latency_ms: None,
                compute_units: None,
                slot: None,
                success: false,
                error: Some(e.to_string()),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: String::new(),
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            }
        }
    }
}

/// Run Solana sustained load test at a controlled TPS rate.
pub(super) async fn run_sustained_load_test_with_metrics(
    args: &LoadTestArgs,
    evm_destination: bool,
    destination_address: &str,
    verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    send_done: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    spinner_tx: tokio::sync::oneshot::Sender<indicatif::ProgressBar>,
) -> eyre::Result<LoadTestReport> {
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

    let main_keypair = solana::load_keypair(args.keypair.as_deref())?;
    let rpc_client = RpcClient::new_with_commitment(
        &args.source_rpc,
        solana_commitment_config::CommitmentConfig::finalized(),
    );
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    let sol = balance as f64 / 1e9;
    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));
    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. {}",
            fund_hint(args.network, &pubkey)
        ));
    }

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    // Derive and fund pool — each key fires duration/cycle times, and each
    // fire costs `gas_per_tx` lamports in gas (on non-devnet).
    let derived = keypairs::derive_keypairs(&main_keypair, pool_size)?;
    let fires_per_key = (duration_secs / key_cycle as u64).max(1);
    let gas_per_tx: u64 = match args.network {
        Network::DevnetAmplifier => 0, // devnet-amplifier doesn't pay gas
        _ => solana::pay_gas_lamports(&args.destination_chain),
    };
    let _balances = keypairs::ensure_funded_for_sustained(
        &args.source_rpc,
        &main_keypair,
        &derived,
        fires_per_key,
        gas_per_tx,
    )?;
    let keypairs_pool: Arc<Vec<Arc<dyn solana_sdk::signer::Signer + Send + Sync>>> = Arc::new(
        derived
            .into_iter()
            .map(|kp| Arc::new(kp) as Arc<dyn solana_sdk::signer::Signer + Send + Sync>)
            .collect(),
    );
    ui::info(&format!(
        "derived {} Solana signing keys (pool: {} tx/s × {}s cycle)",
        pool_size, tps, key_cycle
    ));

    let memo_program_id = super::evm_sender::memo_program_id(args.network);
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let spinner = ui::wait_spinner(&format!("[0/{duration_secs}s] starting sustained send..."));
    // Send a clone to the verification task so it can display progress.
    let _ = spinner_tx.send(spinner.clone());

    let dest_chain = args.destination_chain.clone();
    let dest_addr = destination_address.to_string();
    let solana_rpc = args.source_rpc.clone();
    let evm_dest = evm_destination;
    let source_chain = args.source_axelar_id.clone();
    let network = args.network;

    // Check if source chain has a voting verifier (for correct initial phase).
    let cfg = crate::config::ChainsConfig::load(&args.config)?;
    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_chain)
        .is_ok();

    let make_task: sustained::MakeTask = Box::new(move |key_idx: usize, _nonce: Option<u64>| {
        let kp = Arc::clone(&keypairs_pool[key_idx]);
        let tx_payload = if evm_dest {
            make_payload(&payload)
        } else {
            super::evm_sender::make_executable_payload(&payload, &counter_pda)
        };
        let dc = dest_chain.clone();
        let da = dest_addr.clone();
        let rpc = solana_rpc.clone();
        let vtx = verify_tx.clone();
        let sc = source_chain.clone();
        let has_vv = has_voting_verifier;

        Box::pin(async move {
            let mut result = tokio::task::spawn_blocking(move || {
                send_sol_tx(&rpc, kp.as_ref(), network, &dc, &da, &tx_payload)
            })
            .await
            .unwrap_or_else(|e| TxMetrics {
                signature: String::new(),
                submit_time_ms: 0,
                confirm_time_ms: None,
                latency_ms: None,
                compute_units: None,
                slot: None,
                success: false,
                error: Some(format!("task panicked: {e}")),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: String::new(),
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            });
            // Stream successful txs to the concurrent verification pipeline.
            if result.success
                && let Some(ref tx_sender) = vtx
            {
                match super::verify::tx_to_pending_solana(
                    &result,
                    0,
                    &sc,
                    has_vv,
                    super::verify::SourceChainType::Svm,
                    network,
                    false, // Solana source → EVM dest legacy support lands in a later phase
                ) {
                    Ok(pending) => {
                        if tx_sender.send(pending).is_err() {
                            eprintln!("warning: verification channel closed, tx won't be verified");
                        }
                    }
                    Err(e) => {
                        result.success = false;
                        result.error = Some(format!("failed to build verification state: {e}"));
                    }
                }
            }
            result
        })
    });

    let result = sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        None,
        make_task,
        send_done.clone(),
        spinner,
    )
    .await;

    Ok(sustained::build_sustained_report(
        result,
        &args.source_chain,
        &args.destination_chain,
        destination_address,
        total_expected,
        pool_size,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn execute_and_record(
    solana_rpc: &str,
    keypair: Arc<dyn Signer + Send + Sync>,
    network: Network,
    dest_chain: &str,
    dest_addr: &str,
    payload: &[u8],
    metrics_list: Arc<Mutex<Vec<TxMetrics>>>,
    confirmed_counter: Arc<AtomicU64>,
    spinner: ProgressBar,
    total: usize,
) {
    let submit_start = Instant::now();

    let source_addr = keypair.pubkey().to_string();
    let payload_hash = alloy::hex::encode(keccak256(payload));

    match solana::send_call_contract(
        solana_rpc,
        keypair.as_ref(),
        network,
        dest_chain,
        dest_addr,
        payload,
    ) {
        Ok((_sig, mut metrics)) => {
            metrics.payload = payload.to_vec();
            metrics.payload_hash = payload_hash;
            metrics.source_address = source_addr;
            metrics.send_instant = Some(submit_start);
            let done = confirmed_counter.fetch_add(1, Ordering::Relaxed) + 1;
            spinner.set_message(format!("sending ({done}/{total} confirmed)..."));
            metrics_list.lock().await.push(metrics);
        }
        Err(e) => {
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
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            };
            metrics_list.lock().await.push(metrics);
        }
    }
}
