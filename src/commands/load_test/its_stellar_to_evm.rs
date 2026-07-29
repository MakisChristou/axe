//! Stellar -> any EVM ITS load test.
//!
//! Mirrors `its_sol_to_evm.rs`:
//!   1. Deploy the AXE interchain token on Stellar (or reuse cached token_id)
//!   2. Register it on the EVM destination via `deploy_remote_interchain_token`
//!   3. Wait for the remote-deploy message to land on the EVM ITS proxy
//!   4. Distribute AXE balances to ephemeral Stellar wallets
//!   5. Fire `interchain_transfer` calls (burst or sustained)
//!   6. Verify through Amplifier (voted → hub_approved → routed → approved → executed)

use std::time::Instant;

use eyre::{Result, eyre};

use super::its_prerequisites::{self, GatewayRequirement};
use super::its_stellar_source::{
    self, ItsStellarSubmitter, RemoteDeploymentVerifier, SustainedTransferArgs, TokenSetupRequest,
    amount_per_key, derive_and_fund_wallets, distribute_token_balances, parse_gas_stroops,
    setup_token, transfer_amount,
};
use super::its_verification;
use super::its_verification::{EvmItsTarget, ItsBurstReport, finish_burst};
use super::metrics::ComputeUnitSummary;
use super::run_sizing::{RunSizing, SustainedPlan};
use super::verification_session::VerificationSession;
use super::verify::VerificationRoute;
use super::{LoadTestArgs, validate_evm_rpc};
use crate::config::ChainsConfig;
use crate::stellar::{StellarClient, StellarWallet};
use crate::ui;

pub async fn run(args: LoadTestArgs, _run_start: Instant, sizing: RunSizing) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::AmplifierOnly)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let stellar = init_stellar_setup(&args, src).await?;
    let evm = resolve_evm_targets(&cfg, dest)?;
    let gas_stroops = parse_gas_stroops(args.gas_value.as_deref())?;

    let remote_verifier = EvmRemoteDeployment {
        gateway: evm.evm_gateway_addr,
        rpc_url: &evm_rpc_url,
    };
    let token = setup_token(TokenSetupRequest {
        client: &stellar.client,
        main_wallet: &stellar.main_wallet,
        its_contract: &stellar.its_addr,
        gateway_contract: &stellar.gateway_addr,
        gas_token: &stellar.xlm_addr,
        gas_stroops: super::units::Stroops::new(gas_stroops),
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
    // /100 → 0.01 whole tokens per tx so the cron's source-side supply lasts.
    let amount_per_tx = transfer_amount(token.decimals);

    // Burst: 1 tx/key. Sustained: each derived key serves `key_cycle` txs in
    // its rotation slot before rotating out, so fund it for that many gas
    // payments.
    let txs_per_key = if sizing.is_burst() { 1 } else { args.key_cycle };
    let wallets = derive_and_fund_wallets(
        &stellar.client,
        &stellar.main_wallet,
        sizing.num_keys,
        stellar.use_friendbot,
        gas_stroops,
        txs_per_key,
    )
    .await?;

    let amount_per_key = amount_per_key(&sizing, args.key_cycle, token.decimals);
    distribute_token_balances(
        &stellar.client,
        &stellar.main_wallet,
        &token.token_address,
        &wallets,
        amount_per_key,
    )
    .await?;

    let pipeline = PipelineContext {
        args: &args,
        stellar: &stellar,
        evm: &evm,
        sizing: &sizing,
        token_id: token.token_id,
        gas_stroops,
        amount_per_tx,
    };
    if !sizing.is_burst() {
        run_sustained_pipeline(&pipeline, wallets).await
    } else {
        run_burst_pipeline(&pipeline, wallets).await
    }
}

/// Stellar source-side resources: client + activated main wallet plus the
/// three contract addresses we use throughout (ITS, AxelarGateway, XLM token).
struct StellarSetup {
    client: StellarClient,
    main_wallet: StellarWallet,
    its_addr: String,
    gateway_addr: String,
    xlm_addr: String,
    use_friendbot: bool,
}

/// EVM destination addresses resolved from config for the destination chain.
struct EvmTargets {
    evm_its_addr: alloy::primitives::Address,
    dest_address_bytes: Vec<u8>,
    evm_gateway_addr: alloy::primitives::Address,
    axelarnet_gw_addr: String,
}

struct EvmRemoteDeployment<'a> {
    gateway: alloy::primitives::Address,
    rpc_url: &'a str,
}

impl RemoteDeploymentVerifier for EvmRemoteDeployment<'_> {
    async fn wait_for_remote_deploy(
        &self,
        config: &std::path::Path,
        source_axelar_id: &str,
        destination_axelar_id: &str,
        message_id: &str,
    ) -> Result<()> {
        super::verify::wait_for_its_remote_deploy(
            config,
            source_axelar_id,
            destination_axelar_id,
            message_id,
            self.gateway,
            self.rpc_url,
        )
        .await
    }

    fn after_remote_deploy(&self, source_chain: &str, token_id: [u8; 32]) {
        super::helpers::hint_persist_axe_token(
            source_chain,
            &alloy::primitives::FixedBytes::from(token_id),
        );
    }
}

/// Verify Axelar-side prerequisites (cosmos Gateway for `dest`, global
/// AxelarnetGateway). Bails with the existing error strings if either is
/// missing.
/// Read Stellar source-chain config (network type, contract addresses), build
/// the RPC client, load the main wallet, and ensure it is activated
/// (Friendbot on testnet/futurenet, else bail).
async fn init_stellar_setup(args: &LoadTestArgs, src: &str) -> Result<StellarSetup> {
    let stellar_rpc = &args.source_rpc;
    let network_type = super::read_stellar_network_type(&args.config, src)?;
    let stellar_client = StellarClient::new(stellar_rpc, &network_type)?;
    let stellar_its_addr =
        super::read_stellar_contract_address(&args.config, src, "InterchainTokenService")?;
    let stellar_gateway_addr =
        super::read_stellar_contract_address(&args.config, src, "AxelarGateway")?;
    let stellar_xlm_addr = super::read_stellar_token_address(&args.config, src)?;
    ui::address("Stellar ITS", &stellar_its_addr);
    ui::address("Stellar AxelarGateway", &stellar_gateway_addr);
    ui::address("Stellar XLM token", &stellar_xlm_addr);

    let main_wallet = super::load_stellar_main_wallet(args.private_key.as_deref())?;
    ui::kv("Stellar wallet", &main_wallet.address());

    // For ITS the main wallet itself signs deploy + distribution txs, so it
    // must be activated. (GMP doesn't need this — ephemeral wallets sign
    // there.) Friendbot it on testnet/futurenet; otherwise leave to the user.
    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    if stellar_client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
    {
        if use_friendbot {
            ui::info("activating Stellar main wallet via Friendbot...");
            stellar_client
                .friendbot_fund(&main_wallet.address())
                .await?;
            ui::success("main wallet activated");
        } else {
            eyre::bail!(
                "Stellar main wallet {} is not activated — fund it manually (need ≥ 2 XLM \
                 base reserve plus enough for token deploys + per-key distribution).",
                main_wallet.address()
            );
        }
    }

    Ok(StellarSetup {
        client: stellar_client,
        main_wallet,
        its_addr: stellar_its_addr,
        gateway_addr: stellar_gateway_addr,
        xlm_addr: stellar_xlm_addr,
        use_friendbot,
    })
}

/// Resolve the EVM-destination addresses (ITS proxy, gateway, axelarnet
/// gateway) from config and emit the matching UI lines.
fn resolve_evm_targets(cfg: &ChainsConfig, dest: &str) -> Result<EvmTargets> {
    let dest_cfg = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre!("destination chain '{dest}' not found in config"))?;
    let evm_its_addr: alloy::primitives::Address = dest_cfg
        .contract_address("InterchainTokenService", dest)?
        .parse()?;
    ui::address("destination ITS", &format!("{evm_its_addr}"));
    let dest_address_bytes = evm_its_addr.as_slice().to_vec();

    let evm_gateway_addr: alloy::primitives::Address =
        dest_cfg.contract_address("AxelarGateway", dest)?.parse()?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    let axelarnet_gw_addr = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    Ok(EvmTargets {
        evm_its_addr,
        dest_address_bytes,
        evm_gateway_addr,
        axelarnet_gw_addr,
    })
}

/// Drive the sustained-mode pipeline: spawn the streaming verifier, run the
/// Stellar ITS sustained loop, stitch amplifier timings back into the report,
/// and hand off to `finish_report`.
#[derive(Clone, Copy)]
struct PipelineContext<'a> {
    args: &'a LoadTestArgs,
    stellar: &'a StellarSetup,
    evm: &'a EvmTargets,
    sizing: &'a RunSizing,
    token_id: [u8; 32],
    gas_stroops: u64,
    amount_per_tx: u128,
}

async fn run_sustained_pipeline(
    pipeline: &PipelineContext<'_>,
    wallets: Vec<StellarWallet>,
) -> Result<()> {
    let PipelineContext {
        args,
        stellar,
        evm,
        sizing,
        token_id,
        gas_stroops,
        amount_per_tx,
    } = *pipeline;
    let SustainedPlan {
        tps: tps_n,
        duration_secs,
        key_cycle,
    } = sizing.sustained().expect("sustained mode");
    let mut verification = VerificationSession::start(
        VerificationRoute::from_args(args),
        EvmItsTarget {
            gateway_addr: evm.evm_gateway_addr,
            rpc_url: args.destination_rpc.clone(),
        },
    );

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained Stellar ITS send..."
    ));
    verification.attach_spinner(spinner.clone())?;

    let test_start = Instant::now();
    let result = its_stellar_source::run_sustained(SustainedTransferArgs {
        submitter: ItsStellarSubmitter {
            client: stellar.client.clone(),
            its_contract: stellar.its_addr.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            token_id: token_id.into(),
            destination_chain: args.destination_axelar_id.clone(),
            destination_address_bytes: evm.dest_address_bytes.clone(),
            gas_token: stellar.xlm_addr.clone(),
            gas_stroops: super::units::Stroops::new(gas_stroops),
            amount_per_tx,
            axelarnet_gw_addr: evm.axelarnet_gw_addr.clone(),
        },
        wallets,
        tps: tps_n,
        duration_secs,
        key_cycle,
        verify_tx: Some(verification.sender()),
        send_done: Some(verification.send_done()),
        spinner,
    })
    .await?;
    its_verification::finish_sustained(
        verification,
        args,
        result,
        &format!("{}", evm.evm_its_addr),
        sizing.total_expected,
        sizing.num_keys,
        test_start,
    )
    .await
}

/// Drive the burst-mode pipeline: fan out `num_keys` parallel ITS transfers,
/// batch-verify on the EVM destination, and hand off to `finish_report`.
async fn run_burst_pipeline(
    pipeline: &PipelineContext<'_>,
    wallets: Vec<StellarWallet>,
) -> Result<()> {
    let PipelineContext {
        args,
        stellar,
        evm,
        sizing,
        token_id,
        gas_stroops,
        amount_per_tx,
    } = *pipeline;
    let num_keys = sizing.num_keys;

    let test_start = Instant::now();
    let burst = its_stellar_source::run_its_burst(
        ItsStellarSubmitter {
            client: stellar.client.clone(),
            its_contract: stellar.its_addr.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            token_id: token_id.into(),
            destination_chain: args.destination_axelar_id.clone(),
            destination_address_bytes: evm.dest_address_bytes.clone(),
            gas_token: stellar.xlm_addr.clone(),
            gas_stroops: super::units::Stroops::new(gas_stroops),
            amount_per_tx,
            axelarnet_gw_addr: evm.axelarnet_gw_addr.clone(),
        },
        wallets,
        100,
    )
    .await?;
    let num_txs = burst.total_submitted;
    finish_burst(
        args,
        EvmItsTarget {
            gateway_addr: evm.evm_gateway_addr,
            rpc_url: args.destination_rpc.clone(),
        },
        burst,
        ItsBurstReport {
            destination_address: format!("{}", evm.evm_its_addr),
            num_txs,
            num_keys,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        test_start,
    )
    .await
}
