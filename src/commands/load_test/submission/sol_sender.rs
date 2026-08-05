//! Solana GMP transaction submission.

use crate::config::ChainsConfig;
use alloy::hex;
use alloy::primitives::keccak256;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::LoadTestArgs;
use super::gmp_payload::GmpPayloadEncoding;
use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity, TxMetrics};
use super::run_sizing::SustainedPlan;
use super::submitter::TransactionSubmitter;
use super::sustained;
use crate::config::AxelarChainContract;
use crate::solana;
use crate::types::Network;
use crate::ui;
use eyre::eyre;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tokio::task::spawn_blocking;

/// Per-network funding hint for an empty Solana wallet. `solana airdrop`
/// only works on devnet/testnet; on mainnet users have to source SOL
/// elsewhere.
fn fund_hint(network: Network, pubkey: &Pubkey) -> String {
    match network {
        Network::Mainnet => format!("Fund {pubkey} with mainnet SOL (no faucet) before retrying."),
        _ => format!("Fund it first:\n  solana airdrop 2 {pubkey}"),
    }
}

struct SolanaSubmitter {
    rpc_url: String,
    network: Network,
    destination_chain: String,
    destination_address: String,
}

impl SolanaSubmitter {
    fn new(args: &LoadTestArgs, destination_address: &str) -> Self {
        Self {
            rpc_url: args.source_rpc.to_string(),
            network: args.network,
            destination_chain: args.destination_chain.to_string(),
            destination_address: destination_address.to_string(),
        }
    }
}

struct SolanaSubmitJob {
    signer: Arc<dyn Signer + Send + Sync>,
    payload: Vec<u8>,
}

impl TransactionSubmitter for SolanaSubmitter {
    type Job = SolanaSubmitJob;

    fn submit(&self, job: Self::Job) -> impl Future<Output = TxMetrics> + Send {
        let rpc_url = self.rpc_url.clone();
        let network = self.network;
        let destination_chain = self.destination_chain.clone();
        let destination_address = self.destination_address.clone();
        async move {
            spawn_blocking(move || {
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
pub async fn run_load_test_with_metrics(
    args: &LoadTestArgs,
    num_txs: u64,
    destination_address: &str,
    payload_encoding: GmpPayloadEncoding,
) -> eyre::Result<LoadTestReport> {
    let num_txs = usize::try_from(num_txs)?;
    let payload_encoder = payload_encoding.prepare(args.network);

    let main_keypair = solana::load_keypair(args.keypair.as_deref()).await?;
    let source_rpc = args.source_rpc.to_string();
    let network = args.network;
    let keypairs = spawn_blocking(move || {
        prepare_burst_keypairs(&source_rpc, network, num_txs, &main_keypair)
    })
    .await??;
    let key_count = keypairs.len();

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    let jobs = keypairs
        .iter()
        .map(|keypair| SolanaSubmitJob {
            signer: Arc::clone(keypair),
            payload: payload_encoder.encode(&payload),
        })
        .collect();
    let burst = super::submitter::run_burst(
        SolanaSubmitter::new(args, destination_address),
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

fn prepare_burst_keypairs(
    source_rpc: &str,
    network: Network,
    num_txs: usize,
    main_keypair: &Keypair,
) -> eyre::Result<Arc<Vec<Arc<dyn Signer + Send + Sync>>>> {
    check_wallet_balance(source_rpc, network, main_keypair)?;

    Ok(Arc::new(prepare_keypairs(
        source_rpc,
        num_txs,
        main_keypair,
    )?))
}

fn check_wallet_balance(
    source_rpc: &str,
    network: Network,
    main_keypair: &Keypair,
) -> eyre::Result<()> {
    let rpc_client = RpcClient::new_with_commitment(source_rpc, CommitmentConfig::finalized());
    let pubkey = main_keypair.pubkey();
    let balance = rpc_client.get_balance(&pubkey).unwrap_or(0);
    let sol = balance as f64 / 1e9;

    ui::kv("wallet", &format!("{pubkey} ({sol:.4} SOL)"));

    if balance == 0 {
        return Err(eyre!(
            "wallet ({pubkey}) has no SOL. {}",
            fund_hint(network, &pubkey)
        ));
    }

    Ok(())
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
    let payload_hash = hex::encode(keccak256(payload));
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
async fn prepare_sustained_keypairs(
    args: &LoadTestArgs,
    plan: SustainedPlan,
) -> eyre::Result<Arc<Vec<Arc<dyn Signer + Send + Sync>>>> {
    let main_keypair = solana::load_keypair(args.keypair.as_deref()).await?;
    let source_rpc = args.source_rpc.to_string();
    let destination_chain = args.destination_chain.to_string();
    let network = args.network;

    spawn_blocking(move || {
        prepare_sustained_keypairs_blocking(
            &source_rpc,
            &destination_chain,
            network,
            plan,
            &main_keypair,
        )
    })
    .await?
}

fn prepare_sustained_keypairs_blocking(
    source_rpc: &str,
    destination_chain: &str,
    network: Network,
    plan: SustainedPlan,
    main_keypair: &Keypair,
) -> eyre::Result<Arc<Vec<Arc<dyn Signer + Send + Sync>>>> {
    check_wallet_balance(source_rpc, network, main_keypair)?;

    let pool_size = plan.tps * plan.key_cycle;
    let derived = keypairs::derive_keypairs(main_keypair, pool_size)?;
    let fires_per_key = (plan.duration_secs / plan.key_cycle as u64).max(1);
    let gas_per_tx = match network {
        Network::DevnetAmplifier => 0,
        _ => solana::pay_gas_lamports(destination_chain),
    };
    keypairs::ensure_funded_for_sustained(
        source_rpc,
        main_keypair,
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
    payload_encoding: GmpPayloadEncoding,
    destination_address: &str,
    verify_tx: Option<mpsc::UnboundedSender<super::verify::PendingTx>>,
    send_done: Option<Arc<AtomicBool>>,
    spinner_tx: oneshot::Sender<indicatif::ProgressBar>,
) -> eyre::Result<LoadTestReport> {
    let SustainedPlan {
        tps,
        duration_secs,
        key_cycle,
    } = plan;
    let pool_size = tps * key_cycle;
    let total_expected = plan.total_transactions();
    let payload_encoder = payload_encoding.prepare(args.network);

    let payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        Option::None => Option::None,
    };

    let keypairs_pool = prepare_sustained_keypairs(args, plan).await?;

    let spinner = ui::wait_spinner(&format!("[0/{duration_secs}s] starting sustained send..."));
    // Send a clone to the verification task so it can display progress.
    let _ = spinner_tx.send(spinner.clone());

    let source_chain = args.source_axelar_id.to_string();
    let network = args.network;

    // Check if source chain has a voting verifier (for correct initial phase).
    let cfg = ChainsConfig::load(&args.config).await?;
    let has_voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, &args.source_chain)
        .is_ok();

    let make_task = sustained::submission_tasks(
        SolanaSubmitter::new(args, destination_address),
        move |key_index, _nonce| SolanaSubmitJob {
            signer: Arc::clone(&keypairs_pool[key_index]),
            payload: payload_encoder.encode(&payload),
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
