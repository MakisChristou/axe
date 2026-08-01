//! Stellar to Solana ITS route.
//!
//! Mirrors `its_stellar_to_evm.rs` but with a Solana destination:
//!   1. Deploy the AXE interchain token on Stellar (or reuse cached token_id)
//!   2. Register it on the Solana destination via `deploy_remote_interchain_token`
//!   3. Wait for the remote-deploy to land on the Solana ITS program
//!   4. Distribute AXE balances to ephemeral Stellar wallets
//!   5. Fire `interchain_transfer` calls (burst or sustained)
//!   6. Verify through Amplifier (voted → hub_approved → routed → approved → executed)

use crate::solana::load_keypair;
use std::path::Path;
use std::time::Instant;

use eyre::Result;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;

use super::its_prerequisites::{self, GatewayRequirement};
use super::its_stellar_source::{
    self, ItsStellarSubmitter, RemoteDeploymentVerifier, SustainedTransferArgs, TokenSetupRequest,
    amount_per_key, derive_and_fund_wallets, distribute_token_balances, parse_gas_stroops,
    setup_token, transfer_amount,
};
use super::its_verification;
use super::its_verification::{ItsBurstReport, SolanaItsTarget, finish_burst};
use super::metrics::ComputeUnitSummary;
use super::run_sizing::{RunMode, RunSizing, SustainedPlan};
use super::units::Stroops;
use super::{LoadTestArgs, validate_solana_rpc};
use crate::config::AxelarGlobalContract;
use crate::config::ChainContract;
use crate::config::ChainsConfig;
use crate::stellar::{StellarClient, StellarWallet};
use crate::types::Network;
use crate::ui;

pub async fn run(args: LoadTestArgs, sizing: RunSizing) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let solana_rpc_url = args.destination_rpc.to_string();
    validate_solana_rpc(&solana_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config).await?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::Required)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let stellar = read_stellar_setup(&args.config, src, &args.source_rpc).await?;
    let main_wallet = init_stellar_main_wallet(
        &stellar.client,
        args.private_key.as_deref(),
        stellar.use_friendbot,
    )
    .await?;
    let solana = resolve_solana_target(args.keypair.as_deref(), args.network).await?;
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
            .global_contract_address(AxelarGlobalContract::AxelarnetGateway)?
            .to_string(),
        amount_per_tx,
    };

    match sizing.mode() {
        RunMode::Sustained(plan) => {
            run_sustained_pipeline(&args, &stellar, &solana, wallets, &transfer, &sizing, plan)
                .await
        }
        RunMode::Burst { .. } => {
            run_burst_pipeline(&args, &stellar, &solana, wallets, &transfer, &sizing).await
        }
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
    recipient: Pubkey,
    address_bytes: Vec<u8>,
}

struct SolanaRemoteDeployment {
    rpc_url: String,
    network: Network,
}

impl RemoteDeploymentVerifier for SolanaRemoteDeployment {
    async fn wait_for_remote_deploy(
        &self,
        config: &Path,
        source_axelar_id: &str,
        destination_axelar_id: &str,
        message_id: &str,
    ) -> Result<()> {
        super::verify::wait_for_its_remote_deploy_to_solana(
            config,
            source_axelar_id,
            destination_axelar_id,
            message_id,
            &self.rpc_url,
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
    gas_stroops: Stroops,
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
async fn read_stellar_setup(config: &Path, src: &str, stellar_rpc: &str) -> Result<StellarSetup> {
    let network_type = super::read_stellar_network_type(config, src).await?;
    let client = StellarClient::new(stellar_rpc, &network_type)?;
    let its_addr =
        super::read_stellar_contract_address(config, src, ChainContract::InterchainTokenService)
            .await?;
    let gateway_addr =
        super::read_stellar_contract_address(config, src, ChainContract::AxelarGateway).await?;
    let xlm_addr = super::read_stellar_token_address(config, src).await?;
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
async fn resolve_solana_target(keypair: Option<&str>, network: Network) -> Result<SolanaTarget> {
    let sol_keypair = load_keypair(keypair).await?;
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
    gas_stroops: Stroops,
    sizing: &RunSizing,
) -> Result<([u8; 32], Vec<StellarWallet>, u128)> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let remote_verifier = SolanaRemoteDeployment {
        rpc_url: solana_rpc_url.to_string(),
        network: args.network,
    };
    let token = setup_token(
        &stellar.client,
        main_wallet,
        &remote_verifier,
        TokenSetupRequest {
            its_contract: stellar.its_addr.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            gas_token: stellar.xlm_addr.clone(),
            gas_stroops,
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_axelar_id: args.destination_axelar_id.to_string(),
            token_id_override: args.token_id.clone(),
            config: args.config.clone(),
            required_transfers: sizing.num_keys,
        },
    )
    .await?;
    ui::kv("token ID", &hex::encode(token.token_id));
    ui::address("token contract (Stellar)", &token.token_address);

    let txs_per_key = sizing.transactions_per_key();
    let wallets = derive_and_fund_wallets(
        &stellar.client,
        main_wallet,
        sizing.num_keys,
        stellar.use_friendbot,
        gas_stroops,
        txs_per_key,
    )
    .await?;

    let amount_per_key = amount_per_key(sizing, token.decimals);
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
    plan: SustainedPlan,
) -> Result<()> {
    let duration_secs = plan.duration_secs;
    let submitter = ItsStellarSubmitter {
        client: stellar.client.clone(),
        its_contract: stellar.its_addr.clone(),
        gateway_contract: stellar.gateway_addr.clone(),
        token_id: transfer.token_id.into(),
        destination_chain: args.destination_axelar_id.to_string(),
        destination_address_bytes: solana.address_bytes.clone(),
        gas_token: stellar.xlm_addr.clone(),
        gas_stroops: transfer.gas_stroops,
        amount_per_tx: transfer.amount_per_tx,
        axelarnet_gw_addr: transfer.axelarnet_gw_addr.clone(),
    };
    let destination_address = solana.recipient.to_string();
    its_verification::run_sustained(
        args,
        SolanaItsTarget {
            rpc_url: args.destination_rpc.to_string(),
        },
        &format!("[0/{duration_secs}s] starting sustained Stellar ITS send..."),
        &destination_address,
        sizing.total_expected,
        sizing.num_keys,
        |context| {
            its_stellar_source::run_sustained(SustainedTransferArgs {
                submitter,
                wallets,
                plan,
                verify_tx: Some(context.verify_tx),
                send_done: Some(context.send_done),
                spinner: context.spinner,
            })
        },
    )
    .await
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
    let num_keys = sizing.num_keys;
    let token_id = transfer.token_id;
    let gas_stroops = transfer.gas_stroops;
    let amount_per_tx = transfer.amount_per_tx;

    let test_start = Instant::now();
    let burst = its_stellar_source::run_its_burst(
        ItsStellarSubmitter {
            client: stellar.client.clone(),
            its_contract: stellar.its_addr.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            token_id: token_id.into(),
            destination_chain: args.destination_axelar_id.to_string(),
            destination_address_bytes: solana.address_bytes.clone(),
            gas_token: stellar.xlm_addr.clone(),
            gas_stroops,
            amount_per_tx,
            axelarnet_gw_addr: transfer.axelarnet_gw_addr.clone(),
        },
        wallets,
        100,
    )
    .await?;
    let num_txs = burst.total_submitted;
    finish_burst(
        args,
        SolanaItsTarget {
            rpc_url: args.destination_rpc.to_string(),
        },
        burst,
        ItsBurstReport {
            destination_address: solana.recipient.to_string(),
            num_txs,
            num_keys,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        test_start,
    )
    .await
}
