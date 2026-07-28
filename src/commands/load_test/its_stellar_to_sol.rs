//! Stellar -> Solana ITS load test.
//!
//! Mirrors `its_stellar_to_evm.rs` but with a Solana destination:
//!   1. Deploy the AXE interchain token on Stellar (or reuse cached token_id)
//!   2. Register it on the Solana destination via `deploy_remote_interchain_token`
//!   3. Wait for the remote-deploy to land on the Solana ITS program
//!   4. Distribute AXE balances to ephemeral Stellar wallets
//!   5. Fire `interchain_transfer` calls (burst or sustained)
//!   6. Verify through Amplifier (voted → hub_approved → routed → approved → executed)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use eyre::Result;
use solana_sdk::signer::Signer;
use tokio::sync::Mutex;

use super::its_prerequisites::{self, GatewayRequirement};
use super::its_stellar_source::{
    self, RemoteDeploymentVerifier, SustainedTransferArgs, SustainedTransferContext,
    TokenSetupRequest, TransferRequest, amount_per_key, derive_and_fund_wallets,
    distribute_token_balances, parse_gas_stroops, setup_token, transfer_amount,
};
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::run_sizing::RunSizing;
use super::sustained;
use super::{LoadTestArgs, finish_report, validate_solana_rpc};
use crate::config::ChainsConfig;
use crate::stellar::{StellarClient, StellarWallet};
use crate::ui;

pub async fn run(args: LoadTestArgs, _run_start: Instant, sizing: RunSizing) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let solana_rpc_url = args.destination_rpc.clone();
    validate_solana_rpc(&solana_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::Required)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let stellar = read_stellar_setup(&args.config, src, &args.source_rpc)?;
    let main_wallet = init_stellar_main_wallet(
        &stellar.client,
        args.private_key.as_deref(),
        stellar.use_friendbot,
    )
    .await?;
    let solana = resolve_solana_target(args.keypair.as_deref(), args.network)?;
    let gas_stroops = parse_gas_stroops(args.gas_value.as_deref())?;

    let (token_id, wallets, amount_per_tx) = prepare_token_and_wallets(
        &args,
        &stellar,
        &main_wallet,
        &solana_rpc_url,
        gas_stroops,
        &sizing,
    )
    .await?;

    let transfer = ItsTransferSpec {
        token_id,
        gas_stroops,
        axelarnet_gw_addr: cfg
            .axelar
            .global_contract_address("AxelarnetGateway")?
            .to_string(),
        amount_per_tx,
    };

    if !sizing.is_burst() {
        run_sustained_pipeline(&args, &stellar, &solana, wallets, &transfer, &sizing).await
    } else {
        run_burst_pipeline(&args, &stellar, &solana, wallets, &transfer, &sizing).await
    }
}

/// Stellar source-side configuration: the connected client, the contract
/// addresses needed for ITS calls, and whether the network supports
/// Friendbot-based account activation.
struct StellarSetup {
    client: StellarClient,
    its_addr: String,
    gateway_addr: String,
    xlm_addr: String,
    use_friendbot: bool,
}

/// Solana destination identity: the recipient pubkey and the equivalent
/// 32-byte address used as the ITS payload destination.
struct SolanaTarget {
    recipient: solana_sdk::pubkey::Pubkey,
    address_bytes: Vec<u8>,
}

struct SolanaRemoteDeployment<'a> {
    rpc_url: &'a str,
    network: crate::types::Network,
}

impl RemoteDeploymentVerifier for SolanaRemoteDeployment<'_> {
    async fn wait_for_remote_deploy(
        &self,
        config: &std::path::Path,
        source_axelar_id: &str,
        destination_axelar_id: &str,
        message_id: &str,
    ) -> Result<()> {
        super::verify::wait_for_its_remote_deploy_to_solana(
            config,
            source_axelar_id,
            destination_axelar_id,
            message_id,
            self.rpc_url,
            self.network,
        )
        .await
    }
}

/// Per-transfer payload bits common to burst and sustained modes: the
/// deployed ITS token, the gas value, and the GMP-hub destination
/// (AxelarnetGateway) used for verification routing.
struct ItsTransferSpec {
    token_id: [u8; 32],
    gas_stroops: u64,
    axelarnet_gw_addr: String,
    /// Per-transfer amount, already scaled to the resolved token's decimals.
    amount_per_tx: u128,
}

/// Verify Axelar-side prerequisites (cosmos Gateway for `dest`, global
/// AxelarnetGateway). Bails with the existing error strings if either is
/// missing.
/// Read the Stellar source-chain config (network type, ITS / gateway / XLM
/// addresses) and emit the matching UI lines. Returns a bundle ready for
/// downstream stages.
fn read_stellar_setup(
    config: &std::path::Path,
    src: &str,
    stellar_rpc: &str,
) -> Result<StellarSetup> {
    let network_type = super::read_stellar_network_type(config, src)?;
    let client = StellarClient::new(stellar_rpc, &network_type)?;
    let its_addr = super::read_stellar_contract_address(config, src, "InterchainTokenService")?;
    let gateway_addr = super::read_stellar_contract_address(config, src, "AxelarGateway")?;
    let xlm_addr = super::read_stellar_token_address(config, src)?;
    ui::address("Stellar ITS", &its_addr);
    ui::address("Stellar AxelarGateway", &gateway_addr);
    ui::address("Stellar XLM token", &xlm_addr);

    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    Ok(StellarSetup {
        client,
        its_addr,
        gateway_addr,
        xlm_addr,
        use_friendbot,
    })
}

/// Load the Stellar main wallet and ensure it is activated. For ITS the main
/// wallet itself signs deploy + distribution txs, so it must be activated.
/// (GMP doesn't need this — ephemeral wallets sign there.) Friendbot it on
/// testnet/futurenet; otherwise leave to the user.
async fn init_stellar_main_wallet(
    client: &StellarClient,
    private_key: Option<&str>,
    use_friendbot: bool,
) -> Result<StellarWallet> {
    let main_wallet = super::load_stellar_main_wallet(private_key)?;
    ui::kv("Stellar wallet", &main_wallet.address());

    if client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
    {
        if use_friendbot {
            ui::info("activating Stellar main wallet via Friendbot...");
            client.friendbot_fund(&main_wallet.address()).await?;
            ui::success("main wallet activated");
        } else {
            eyre::bail!(
                "Stellar main wallet {} is not activated — fund it manually (need ≥ 2 XLM \
                 base reserve plus enough for token deploys + per-key distribution).",
                main_wallet.address()
            );
        }
    }
    Ok(main_wallet)
}

/// Resolve the Solana destination: load the keypair (only used for its
/// pubkey — the relayer drives destination-side execution), build the 32-byte
/// address, and emit the matching UI lines.
fn resolve_solana_target(
    keypair: Option<&str>,
    network: crate::types::Network,
) -> Result<SolanaTarget> {
    let sol_keypair = crate::solana::load_keypair(keypair)?;
    let recipient = sol_keypair.pubkey();
    let address_bytes = recipient.to_bytes().to_vec();
    ui::kv("Solana recipient", &recipient.to_string());
    ui::address("Solana ITS program", &network.solana_its_id().to_string());
    Ok(SolanaTarget {
        recipient,
        address_bytes,
    })
}

/// Run the cache-or-deploy ITS token setup, derive + fund the ephemeral
/// wallets, then distribute AXE to each wallet according to the run sizing.
/// Returns `(token_id, wallets)` ready for the burst/sustained pipelines.
async fn prepare_token_and_wallets(
    args: &LoadTestArgs,
    stellar: &StellarSetup,
    main_wallet: &StellarWallet,
    solana_rpc_url: &str,
    gas_stroops: u64,
    sizing: &RunSizing,
) -> Result<([u8; 32], Vec<StellarWallet>, u128)> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let remote_verifier = SolanaRemoteDeployment {
        rpc_url: solana_rpc_url,
        network: args.network,
    };
    let token = setup_token(TokenSetupRequest {
        client: &stellar.client,
        main_wallet,
        its_contract: &stellar.its_addr,
        gateway_contract: &stellar.gateway_addr,
        gas_token: &stellar.xlm_addr,
        gas_stroops,
        source_chain: src,
        destination_chain: dest,
        destination_axelar_id: &args.destination_axelar_id,
        token_id_override: args.token_id.as_deref(),
        config: &args.config,
        required_transfers: sizing.num_keys,
        remote_verifier: &remote_verifier,
    })
    .await?;
    ui::kv("token ID", &hex::encode(token.token_id));
    ui::address("token contract (Stellar)", &token.token_address);

    let txs_per_key = if sizing.is_burst() { 1 } else { args.key_cycle };
    let wallets = derive_and_fund_wallets(
        &stellar.client,
        main_wallet,
        sizing.num_keys,
        stellar.use_friendbot,
        gas_stroops,
        txs_per_key,
    )
    .await?;

    let amount_per_key = amount_per_key(sizing, args.key_cycle, token.decimals);
    distribute_token_balances(
        &stellar.client,
        main_wallet,
        &token.token_address,
        &wallets,
        amount_per_key,
    )
    .await?;

    // /100 → 0.01 whole tokens per tx so the cron's source-side supply lasts.
    let amount_per_tx = transfer_amount(token.decimals);
    Ok((token.token_id, wallets, amount_per_tx))
}

/// Drive the sustained-mode pipeline: spawn the streaming verifier, run the
/// Stellar sustained sender, stitch amplifier timings back into the report,
/// and hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    stellar: &StellarSetup,
    solana: &SolanaTarget,
    wallets: Vec<StellarWallet>,
    transfer: &ItsTransferSpec,
    sizing: &RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let (tps_n, duration_secs, key_cycle) = sizing.sustained().expect("sustained mode");

    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = Arc::new(AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vdest_rpc = args.destination_rpc.clone();
    let vdone = Arc::clone(&send_done);
    let vnetwork = args.network;
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_solana_its_streaming(super::verify::StreamingVerification {
            route: super::verify::VerificationRoute {
                config: &vconfig,
                source_chain: &vsource,
                destination_chain: &vdest,
                network: vnetwork,
            },
            destination: super::verify::SolanaItsDestination {
                rpc_url: &vdest_rpc,
            },
            rx: verify_rx,
            send_done: vdone,
            spinner,
        })
        .await
    });

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained Stellar ITS send..."
    ));
    let _ = spinner_tx.send(spinner.clone());

    let test_start = Instant::now();
    let result = its_stellar_source::run_sustained(SustainedTransferArgs {
        context: SustainedTransferContext {
            client: stellar.client.clone(),
            wallets,
            its_contract: stellar.its_addr.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            token_id: transfer.token_id,
            destination_chain: args.destination_axelar_id.clone(),
            destination_address_bytes: solana.address_bytes.clone(),
            gas_token: stellar.xlm_addr.clone(),
            gas_stroops: transfer.gas_stroops,
            amount_per_tx: transfer.amount_per_tx,
            axelarnet_gw_addr: transfer.axelarnet_gw_addr.clone(),
        },
        tps: tps_n,
        duration_secs,
        key_cycle,
        verify_tx: Some(verify_tx),
        send_done: Some(send_done),
        spinner,
    })
    .await?;

    let mut report = sustained::build_sustained_report(
        result,
        src,
        dest,
        &solana.recipient.to_string(),
        sizing.total_expected,
        sizing.num_keys,
    );
    let (verification, timings) = verify_handle.await??;
    for (msg_id, timing) in timings {
        if let Some(tx) = report
            .transactions
            .iter_mut()
            .find(|t| t.signature == msg_id)
        {
            tx.amplifier_timing = Some(timing);
        }
    }
    report.verification = Some(verification);
    finish_report(args, &mut report, test_start)
}

/// Drive the burst-mode pipeline: fan out the Stellar transfers, build the
/// load-test report, run the Solana ITS verifier on the confirmed batch, and
/// hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    stellar: &StellarSetup,
    solana: &SolanaTarget,
    wallets: Vec<StellarWallet>,
    transfer: &ItsTransferSpec,
    sizing: &RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_keys = sizing.num_keys;
    let token_id = transfer.token_id;
    let gas_stroops = transfer.gas_stroops;
    let amount_per_tx = transfer.amount_per_tx;

    let test_start = Instant::now();
    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_keys} confirmed)..."));

    let client = Arc::new(stellar.client.clone());
    let stellar_its_arc = Arc::new(stellar.its_addr.clone());
    let stellar_gw_arc = Arc::new(stellar.gateway_addr.clone());
    let stellar_xlm_arc = Arc::new(stellar.xlm_addr.clone());
    let dest_chain_arc = Arc::new(args.destination_axelar_id.clone());
    let dest_addr_arc = Arc::new(solana.address_bytes.clone());
    let axelarnet_gw_arc = Arc::new(transfer.axelarnet_gw_addr.clone());

    let mut tasks = Vec::with_capacity(num_keys);
    for w in wallets {
        let c = Arc::clone(&client);
        let its = Arc::clone(&stellar_its_arc);
        let gw = Arc::clone(&stellar_gw_arc);
        let xlm = Arc::clone(&stellar_xlm_arc);
        let dc = Arc::clone(&dest_chain_arc);
        let da = Arc::clone(&dest_addr_arc);
        let gmp_dest_addr = Arc::clone(&axelarnet_gw_arc);
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed);
        let sp = spinner.clone();
        let total = num_keys;

        let handle = tokio::spawn(async move {
            let m = its_stellar_source::submit_transfer(TransferRequest {
                client: &c,
                wallet: &w,
                its_contract: &its,
                gateway_contract: &gw,
                token_id,
                destination_chain: &dc,
                destination_address_bytes: &da,
                gas_token: &xlm,
                gas_amount_stroops: gas_stroops,
                transfer_amount: amount_per_tx,
                gmp_dest_address: &gmp_dest_addr,
            })
            .await;
            if m.is_success() {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!("sending ({done}/{total} confirmed)..."));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    super::task_group::join_all(tasks).await?;
    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: solana.recipient.to_string(),
            num_txs: total_submitted,
            num_keys,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    let verification =
        super::verify::verify_onchain_solana_its(super::verify::ItsBatchVerification {
            route: super::verify::VerificationRoute {
                config: &args.config,
                source_chain: &args.source_axelar_id,
                destination_chain: &args.destination_axelar_id,
                network: args.network,
            },
            destination: super::verify::SolanaItsDestination {
                rpc_url: &args.destination_rpc,
            },
            metrics: &mut report.transactions,
        })
        .await?;
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}
