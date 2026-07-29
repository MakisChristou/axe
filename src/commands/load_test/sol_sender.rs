use std::sync::Arc;
use std::time::Instant;

use super::LoadTestArgs;
use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity, TxMetrics};
use super::run_sizing::SustainedPlan;
use super::submitter::TransactionSubmitter;
use super::sustained;
use crate::solana;
use crate::types::Network;
use crate::ui;
use alloy::sol_types::SolValue;
use eyre::eyre;
use rand::Rng;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;

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

struct SolanaSubmitter {
    rpc_url: String,
    network: Network,
    destination_chain: String,
    destination_address: String,
}

struct SolanaSubmitJob {
    signer: Arc<dyn Signer + Send + Sync>,
    payload: Vec<u8>,
}

impl TransactionSubmitter for SolanaSubmitter {
    type Job = SolanaSubmitJob;

    fn submit(&self, job: Self::Job) -> impl std::future::Future<Output = TxMetrics> + Send {
        let rpc_url = self.rpc_url.clone();
        let network = self.network;
        let destination_chain = self.destination_chain.clone();
        let destination_address = self.destination_address.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                send_sol_tx(
                    &rpc_url,
                    job.signer.as_ref(),
                    network,
                    &destination_chain,
                    &destination_address,
                    &job.payload,
                )
            })
            .await
            .unwrap_or_else(|error| TxMetrics::failed("", 0, format!("task panicked: {error}")))
        }
    }
}

struct SolanaSustainedSubmitter {
    rpc_url: String,
    network: Network,
    destination_chain: String,
    destination_address: String,
}

impl TransactionSubmitter for SolanaSustainedSubmitter {
    type Job = SolanaSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        let rpc_url = self.rpc_url.clone();
        let network = self.network;
        let destination_chain = self.destination_chain.clone();
        let destination_address = self.destination_address.clone();
        tokio::task::spawn_blocking(move || {
            send_sol_tx(
                &rpc_url,
                job.signer.as_ref(),
                network,
                &destination_chain,
                &destination_address,
                &job.payload,
            )
        })
        .await
        .unwrap_or_else(|error| TxMetrics::failed("", 0, format!("task panicked: {error}")))
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
        return Ok(vec![
            Arc::new(keypairs::clone_keypair(main_keypair)?) as Arc<dyn Signer + Send + Sync>
        ]);
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
    num_txs: u64,
    destination_address: &str,
    evm_destination: bool,
) -> eyre::Result<LoadTestReport> {
    let num_txs = usize::try_from(num_txs)?;

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

    let jobs = keypairs
        .iter()
        .map(|keypair| SolanaSubmitJob {
            signer: Arc::clone(keypair),
            payload: if evm_destination {
                make_payload(&payload)
            } else {
                super::evm_sender::make_executable_payload(&payload, &counter_pda)
            },
        })
        .collect();
    let burst = super::submitter::run_burst(
        SolanaSubmitter {
            rpc_url: args.source_rpc.clone(),
            network: args.network,
            destination_chain: args.destination_chain.clone(),
            destination_address: destination_address.to_string(),
        },
        jobs,
        key_count,
    )
    .await?;
    let report = LoadTestReport::from_transactions(
        ReportInput {
            run: RunIdentity::burst(args),
            destination_address: destination_address.to_string(),
            num_txs: num_txs as u64,
            num_keys: key_count,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Include,
        },
        burst.metrics,
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
            TxMetrics::failed("", elapsed_ms, e.to_string())
        }
    }
}

/// Run Solana sustained load test at a controlled TPS rate.
fn prepare_sustained_keypairs(
    args: &LoadTestArgs,
    plan: SustainedPlan,
) -> eyre::Result<Arc<Vec<Arc<dyn Signer + Send + Sync>>>> {
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
    let pool_size = plan.tps * plan.key_cycle;
    let derived = keypairs::derive_keypairs(&main_keypair, pool_size)?;
    let fires_per_key = (plan.duration_secs / plan.key_cycle as u64).max(1);
    let gas_per_tx = match args.network {
        Network::DevnetAmplifier => 0,
        _ => solana::pay_gas_lamports(&args.destination_chain),
    };
    keypairs::ensure_funded_for_sustained(
        &args.source_rpc,
        &main_keypair,
        &derived,
        fires_per_key,
        super::units::Lamports::new(gas_per_tx),
    )?;
    ui::info(&format!(
        "derived {} Solana signing keys (pool: {} tx/s × {}s cycle)",
        pool_size, plan.tps, plan.key_cycle
    ));
    Ok(Arc::new(
        derived
            .into_iter()
            .map(|keypair| Arc::new(keypair) as Arc<dyn Signer + Send + Sync>)
            .collect(),
    ))
}

pub(super) async fn run_sustained_load_test_with_metrics(
    args: &LoadTestArgs,
    plan: SustainedPlan,
    evm_destination: bool,
    destination_address: &str,
    verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    send_done: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    spinner_tx: tokio::sync::oneshot::Sender<indicatif::ProgressBar>,
) -> eyre::Result<LoadTestReport> {
    let SustainedPlan {
        tps,
        duration_secs,
        key_cycle,
    } = plan;
    let pool_size = tps * key_cycle;
    let total_expected = plan.total_transactions();

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    let keypairs_pool = prepare_sustained_keypairs(args, plan)?;

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

    let make_task = sustained::submission_tasks(
        SolanaSustainedSubmitter {
            rpc_url: solana_rpc,
            network,
            destination_chain: dest_chain,
            destination_address: dest_addr,
        },
        move |key_index, _nonce| SolanaSubmitJob {
            signer: Arc::clone(&keypairs_pool[key_index]),
            payload: if evm_dest {
                make_payload(&payload)
            } else {
                super::evm_sender::make_executable_payload(&payload, &counter_pda)
            },
        },
        verify_tx,
        sustained::GmpPendingTxAdapter {
            source_chain,
            has_voting_verifier,
            source_type: super::verify::SourceChainType::Svm,
            network,
            legacy: false,
        },
    );

    let result = sustained::run_sustained_loop(
        SustainedPlan {
            tps,
            duration_secs,
            key_cycle,
        },
        None,
        make_task,
        send_done.clone(),
        spinner,
    )
    .await?;

    Ok(sustained::build_sustained_report(
        result,
        RunIdentity::sustained(args, plan),
        destination_address,
        total_expected,
        pool_size,
    ))
}
