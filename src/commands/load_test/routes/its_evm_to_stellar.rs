//! EVM to Stellar ITS route.
//!
//! Source-side flow is identical to `its_evm_to_sol.rs` (deploy/cache the
//! AXE token on the EVM source, distribute to derived signers, fire
//! `interchainTransfer` calls). The destination side uses Stellar's
//! `is_message_approved` / `is_message_executed` view calls via the
//! `verify_onchain_stellar_its[_streaming]` verifier.

use alloy::primitives::keccak256;
use std::time::Instant;

use super::its_evm_source::EvmTokenRunSizing as RunSizing;
use super::its_prerequisites::{self, GatewayRequirement};
use super::its_verification;
use super::its_verification::{ItsBurstReport, StellarItsTarget, finish_burst};
use super::keypairs;
use super::metrics::ComputeUnitSummary;
use super::run_sizing::{RunMode, RunSizing as ValidatedRunSizing, SustainedPlan};
use super::{LoadTestArgs, check_evm_balance, validate_evm_rpc};
use crate::config::AxelarChainContract;
use crate::config::ChainContract;
use crate::config::ChainsConfig;
use crate::evm::InterchainTokenService;
use crate::stellar::StellarClient;
use crate::ui;
use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;

pub async fn run(args: LoadTestArgs, validated_sizing: ValidatedRunSizing) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.to_string();
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config).await?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::Required)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let evm_src = init_evm_source_context(args.private_key.as_deref(), evm_rpc_url.clone()).await?;
    let evm_targets = resolve_evm_targets(&cfg, src)?;
    let stellar = resolve_stellar_targets(&args, evm_src.deployer_address).await?;
    let gas_value_wei = super::its_evm_source::standard_gas_value_wei(&args).await?;
    let gas_value = U256::from(gas_value_wei);
    let mut sizing = RunSizing::standard(validated_sizing);

    let token =
        resolve_or_deploy_token(&args, &evm_src, &evm_targets, &stellar, gas_value, &sizing)
            .await?;

    // compute_run_sizing assumes EVM-18 source decimals; rescale to the
    // actual on-chain decimals (Hedera HTS-fork AXE = 6 dec).
    {
        let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
        super::its_evm_source::rescale_sizing_for_decimals(
            &mut sizing.amount_per_tx,
            &mut sizing.amount_per_key,
            &mut sizing.total_supply,
            &read_provider,
            token.token_addr,
        )
        .await?;
    }

    if let Some(ref deploy_msg_id) = token.deploy_message_id {
        let source_axelar_id = super::axelar_id_for_chain(&args.config, src).await?;
        super::verify::wait_for_its_remote_deploy_to_stellar(
            super::verify::StellarRemoteDeployWait {
                config: args.config.clone(),
                source_chain: source_axelar_id,
                destination_chain: dest.to_string(),
                deploy_message_id: deploy_msg_id.clone(),
                stellar_rpc: stellar.rpc.clone(),
                network_type: stellar.network_type.clone(),
                gateway_contract: stellar.gateway_addr.clone(),
                its_contract: stellar.its_addr.clone(),
                signer_pk: stellar.signer_pk,
                token_id: token.token_id.0,
            },
        )
        .await?;
    }

    let derived = derive_and_fund_signers(&evm_src, &sizing, gas_value_wei).await?;
    distribute_axe_tokens(&evm_src, token.token_addr, &derived, sizing.amount_per_key).await?;

    let (stellar_recipient_addr, receiver_bytes) =
        load_stellar_recipient(args.private_key.as_deref())?;

    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id)
            .await;

    let transfer = TransferContext {
        rpc_url: evm_rpc_url,
        derived,
        its_proxy_addr: evm_targets.its_proxy_addr,
        token_id: token.token_id,
        receiver_bytes,
        amount_per_tx: sizing.amount_per_tx,
        gas_value,
        gas_arg_scaling_factor,
    };

    match sizing.mode() {
        RunMode::Sustained(plan) => {
            run_sustained_pipeline(
                &args,
                &cfg,
                transfer,
                &stellar,
                &stellar_recipient_addr,
                &sizing,
                plan,
            )
            .await
        }
        RunMode::Burst { .. } => {
            run_burst_pipeline(&args, transfer, &stellar, &stellar_recipient_addr, &sizing).await
        }
    }
}

/// EVM source-side state: signer, derived addresses, and the RPC URL used for
/// every per-call provider this module builds.
struct EvmSourceContext {
    signer: PrivateKeySigner,
    deployer_address: Address,
    main_key: [u8; 32],
    rpc_url: String,
}

/// EVM source ITS contract addresses.
struct EvmTargets {
    its_factory_addr: Address,
    its_proxy_addr: Address,
}

/// Stellar destination configuration plus the deterministic 32-byte source
/// pubkey used by the simulate-only verify view calls.
struct StellarTargets {
    rpc: String,
    network_type: String,
    gateway_addr: String,
    its_addr: String,
    signer_pk: [u8; 32],
}

/// Bundle of values consumed by both the burst and sustained pipelines when
/// firing `interchainTransfer` calls from the derived EVM signers.
struct TransferContext {
    rpc_url: String,
    derived: Vec<PrivateKeySigner>,
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    receiver_bytes: Bytes,
    amount_per_tx: U256,
    gas_value: U256,
    /// See `its_evm_source::read_gas_arg_scaling_factor` — Hedera = 10,
    /// other EVM chains = 0.
    gas_arg_scaling_factor: u32,
}

/// ITS token identity on the EVM source plus the remote-deploy message if this
/// run had to create/register a fresh token.
struct TokenIdentity {
    token_id: FixedBytes<32>,
    token_addr: Address,
    deploy_message_id: Option<String>,
}

impl From<(FixedBytes<32>, Address, Option<String>)> for TokenIdentity {
    fn from(
        (token_id, token_addr, deploy_message_id): (FixedBytes<32>, Address, Option<String>),
    ) -> Self {
        Self {
            token_id,
            token_addr,
            deploy_message_id,
        }
    }
}

/// Parse the EVM private key, check the funding balance, and emit the
/// matching UI line. Returns the signer plus the values derived from it
/// (deployer address, main key bytes, RPC URL).
async fn init_evm_source_context(
    private_key: Option<&str>,
    rpc_url: String,
) -> eyre::Result<EvmSourceContext> {
    let private_key = private_key.ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;
    let main_key: [u8; 32] = signer.to_bytes().into();

    {
        let balance: u128 = read_provider.get_balance(deployer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{deployer_address} ({eth:.6} ETH)"));
    }

    Ok(EvmSourceContext {
        signer,
        deployer_address,
        main_key,
        rpc_url,
    })
}

/// Resolve the EVM source ITS factory + service addresses from config and
/// emit the matching UI lines.
fn resolve_evm_targets(cfg: &ChainsConfig, src: &str) -> eyre::Result<EvmTargets> {
    let src_cfg = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?;
    let its_factory_addr: Address = src_cfg
        .contract_address(ChainContract::InterchainTokenFactory, src)?
        .parse()?;
    let its_proxy_addr: Address = src_cfg
        .contract_address(ChainContract::InterchainTokenService, src)?
        .parse()?;
    ui::address("ITS factory", &format!("{its_factory_addr}"));
    ui::address("ITS service", &format!("{its_proxy_addr}"));
    Ok(EvmTargets {
        its_factory_addr,
        its_proxy_addr,
    })
}

/// Resolve Stellar destination config (RPC, network type, gateway/example
/// addresses) and derive the deterministic dummy ed25519 pubkey used as the
/// simulation envelope's source account.
async fn resolve_stellar_targets(
    args: &LoadTestArgs,
    deployer_address: Address,
) -> eyre::Result<StellarTargets> {
    let dest = &args.destination_chain;
    let stellar_rpc = args.destination_rpc.to_string();
    let stellar_network_type = super::read_stellar_network_type(&args.config, dest).await?;
    let stellar_gateway_addr =
        super::read_stellar_contract_address(&args.config, dest, ChainContract::AxelarGateway)
            .await?;
    let stellar_its_addr = super::read_stellar_contract_address(
        &args.config,
        dest,
        ChainContract::InterchainTokenService,
    )
    .await?;
    let stellar_example_addr =
        super::read_stellar_contract_address(&args.config, dest, ChainContract::AxelarExample)
            .await?;
    ui::address("Stellar AxelarGateway", &stellar_gateway_addr);
    ui::address("Stellar ITS", &stellar_its_addr);
    ui::address("Stellar AxelarExample", &stellar_example_addr);

    // For the simulate-only view calls, we need a 32-byte source pubkey but
    // it's not authorizing anything — just used as the simulation envelope's
    // source account. Use the deployer's EVM address right-padded; any
    // existing Stellar account works too. Easiest: derive a deterministic
    // dummy ed25519 pk from the EVM key.
    let signer_pk: [u8; 32] = keccak256(deployer_address.as_slice()).into();

    Ok(StellarTargets {
        rpc: stellar_rpc,
        network_type: stellar_network_type,
        gateway_addr: stellar_gateway_addr,
        its_addr: stellar_its_addr,
        signer_pk,
    })
}

/// Parse the user-supplied gas value (wei), defaulting via the Axelarscan
/// `estimateGasFee` quote and falling back to a route-agnostic constant when
/// the API can't be reached.
/// Resolve the ITS token to use: either the user-supplied `--token-id`, a
/// cached deployment that still has source balance and is registered on
/// Stellar, or a fresh `deploy_its_token` call.
async fn resolve_or_deploy_token(
    args: &LoadTestArgs,
    evm_src: &EvmSourceContext,
    evm_targets: &EvmTargets,
    stellar: &StellarTargets,
    gas_value: U256,
    sizing: &RunSizing,
) -> eyre::Result<TokenIdentity> {
    let src = &args.source_chain;
    let write_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_src.rpc_url.parse()?);
    let its_service = InterchainTokenService::new(evm_targets.its_proxy_addr, &write_provider);
    let needed = sizing.amount_per_tx * U256::from(sizing.num_keys);
    let config_token = super::helpers::reusable_config_axe(
        &args.config,
        src,
        evm_targets.its_proxy_addr,
        &write_provider,
        evm_src.deployer_address,
        needed,
    )
    .await?;

    if let Some(token_id) = args.token_id.as_deref() {
        return resolve_provided_token(token_id, stellar, &its_service).await;
    }
    if let Some((tid, addr, stellar_token_addr)) =
        registered_config_token(stellar, config_token).await?
    {
        ui::kv("token ID (chains-config)", &format!("{tid}"));
        ui::address("token address", &format!("{addr}"));
        ui::address("Stellar token", &stellar_token_addr);
        return Ok((tid, addr, None).into());
    }
    resolve_cached_or_deploy(
        args,
        evm_src,
        evm_targets,
        stellar,
        gas_value,
        sizing,
        &write_provider,
    )
    .await
}

async fn resolve_provided_token<P: Provider>(
    value: &str,
    stellar: &StellarTargets,
    its_service: &InterchainTokenService::InterchainTokenServiceInstance<P>,
) -> eyre::Result<TokenIdentity> {
    let token_id: FixedBytes<32> = value
        .parse()
        .map_err(|e| eyre!("invalid --token-id: {e}"))?;
    let token_addr = its_service
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
    let stellar_token_addr = require_registered_stellar_token(stellar, token_id).await?;
    ui::kv("token ID (provided)", &format!("{token_id}"));
    ui::address("token address", &format!("{token_addr}"));
    ui::address("Stellar token", &stellar_token_addr);
    Ok((token_id, token_addr, None).into())
}

async fn registered_config_token(
    stellar: &StellarTargets,
    config_token: Option<(FixedBytes<32>, Address)>,
) -> eyre::Result<Option<(FixedBytes<32>, Address, String)>> {
    let Some((token_id, token_addr)) = config_token else {
        return Ok(None);
    };
    let Some(stellar_token_addr) = registered_stellar_token_address(stellar, token_id).await?
    else {
        ui::warn("chains-config AXE not registered on Stellar, deploying fresh...");
        return Ok(None);
    };
    Ok(Some((token_id, token_addr, stellar_token_addr)))
}

async fn resolve_cached_or_deploy<P: Provider>(
    args: &LoadTestArgs,
    evm_src: &EvmSourceContext,
    evm_targets: &EvmTargets,
    stellar: &StellarTargets,
    gas_value: U256,
    sizing: &RunSizing,
    provider: &P,
) -> eyre::Result<TokenIdentity> {
    if let Some(cached) =
        super::its_evm_source::cached_evm_token(&args.source_chain, &args.destination_chain).await?
    {
        let needed = sizing.amount_per_tx * U256::from(sizing.num_keys);
        let balance = super::its_evm_source::token_balance(
            provider,
            cached.token_address,
            evm_src.deployer_address,
        )
        .await;
        if balance >= needed {
            if let Some(stellar_token_addr) =
                registered_stellar_token_address(stellar, cached.token_id).await?
            {
                ui::info(&format!(
                    "reusing cached ITS token: {}",
                    cached.token_address
                ));
                ui::kv("token ID (cached)", &format!("{}", cached.token_id));
                ui::address("Stellar token", &stellar_token_addr);
                return Ok((cached.token_id, cached.token_address, None).into());
            }
            ui::warn("cached token is not registered on Stellar, deploying fresh...");
        } else {
            ui::warn(&format!(
                "cached token has insufficient supply ({balance} < {needed}), deploying fresh..."
            ));
        }
    }
    Ok(super::its_evm_source::deploy_its_token(
        provider,
        evm_targets.its_factory_addr,
        evm_src.deployer_address,
        &args.destination_chain,
        sizing.total_supply,
        &args.source_chain,
        gas_value,
    )
    .await?
    .into())
}

async fn require_registered_stellar_token(
    stellar: &StellarTargets,
    token_id: FixedBytes<32>,
) -> eyre::Result<String> {
    registered_stellar_token_address(stellar, token_id)
        .await?
        .ok_or_else(|| eyre!("token ID {token_id} is not registered on Stellar ITS"))
}

async fn registered_stellar_token_address(
    stellar: &StellarTargets,
    token_id: FixedBytes<32>,
) -> eyre::Result<Option<String>> {
    let client = StellarClient::new(&stellar.rpc, &stellar.network_type)?;
    client
        .its_registered_token_address_view(&stellar.signer_pk, &stellar.its_addr, token_id.0)
        .await
}

/// Derive ephemeral EVM signers from the main key and ensure each is funded
/// with enough native gas to cover the planned send rounds plus the gas
/// value passed to ITS.
async fn derive_and_fund_signers(
    evm_src: &EvmSourceContext,
    sizing: &RunSizing,
    gas_value_wei: u128,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let derived = keypairs::derive_evm_signers(&evm_src.main_key, sizing.num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));

    let funding_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_src.rpc_url.parse()?);
    // ITS hub routing pays 2× gas_value per transfer (two commands).
    let hub_gas_value_wei = gas_value_wei.saturating_mul(2);
    let gas_extra_per_key = if sizing.is_burst() {
        hub_gas_value_wei
    } else {
        let rounds = sizing.transactions_per_key();
        let buffered = rounds + rounds / 5 + 1;
        hub_gas_value_wei.saturating_mul(buffered as u128)
    };
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        &evm_src.signer,
        &derived,
        super::units::Wei::from_u128(gas_extra_per_key),
    )
    .await?;
    Ok(derived)
}

/// Distribute the AXE token balance from the deployer to each derived signer
/// so they can each fund their share of `interchainTransfer` calls.
async fn distribute_axe_tokens(
    evm_src: &EvmSourceContext,
    token_addr: Address,
    derived: &[PrivateKeySigner],
    amount_per_key: U256,
) -> eyre::Result<()> {
    let token_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_src.rpc_url.parse()?);
    super::its_evm_source::distribute_tokens(&token_provider, token_addr, derived, amount_per_key)
        .await
}

/// Load the Stellar recipient wallet (requires `STELLAR_PRIVATE_KEY`) and
/// produce the ASCII-bytes encoding ITS expects for Stellar destinations.
///
/// ITS expects `encodeITSDestination` for Stellar = ASCII bytes of the
/// destination address. For plain `interchain_transfer` (no data), the
/// recipient must be a Stellar account (G-address) — sending to a contract
/// C-address makes ITS try `execute_with_data` with empty data, which the
/// AxelarExample callback rejects (Contract Error #17).
fn load_stellar_recipient(private_key: Option<&str>) -> eyre::Result<(String, Bytes)> {
    let stellar_recipient_wallet = super::load_stellar_main_wallet(private_key)
        .map_err(|e| eyre!("EVM→Stellar ITS needs STELLAR_PRIVATE_KEY for the recipient: {e}"))?;
    let stellar_recipient_addr = stellar_recipient_wallet.address();
    let receiver_bytes = Bytes::from(stellar_recipient_addr.as_bytes().to_vec());
    ui::address("destination Stellar account", &stellar_recipient_addr);
    Ok((stellar_recipient_addr, receiver_bytes))
}

/// Drive the sustained-mode pipeline: pre-fetch nonces, spawn the streaming
/// Stellar verifier, run the per-second send loop, stitch amplifier timings
/// back into the report, and hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    transfer: TransferContext,
    stellar: &StellarTargets,
    stellar_recipient_addr: &str,
    sizing: &RunSizing,
    plan: SustainedPlan,
) -> eyre::Result<()> {
    let SustainedPlan {
        tps,
        duration_secs,
        key_cycle,
    } = plan;

    let nonce_provider = ProviderBuilder::new().connect_http(transfer.rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for s in &transfer.derived {
        let n = nonce_provider.get_transaction_count(s.address()).await?;
        nonces.push(n);
    }

    let has_voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, &args.source_axelar_id)
        .is_ok();
    let TransferContext {
        rpc_url,
        derived,
        its_proxy_addr,
        token_id,
        receiver_bytes,
        amount_per_tx,
        gas_value,
        gas_arg_scaling_factor,
        ..
    } = transfer;
    let submitter = super::its_evm_source::ItsEvmSubmitter {
        rpc_url: rpc_url.parse()?,
        its_proxy: its_proxy_addr,
        token_id: token_id.into(),
        destination_chain: args.destination_axelar_id.to_string(),
        receiver: receiver_bytes,
        amount: amount_per_tx,
        gas_value: super::units::Wei::from_u256(gas_value),
        gas_arg_scaling_factor,
    };
    its_verification::run_sustained(
        args,
        StellarItsTarget {
            rpc_url: stellar.rpc.clone(),
            network_type: stellar.network_type.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            signer_pk: stellar.signer_pk,
        },
        &format!("[0/{duration_secs}s] starting sustained ITS send..."),
        stellar_recipient_addr,
        sizing.total_expected,
        sizing.num_keys,
        |context| async move {
            let make_task = super::its_evm_source::its_sustained_tasks(
                submitter,
                derived,
                Some(context.verify_tx),
                has_voting_verifier,
            );
            super::sustained::run_sustained_loop(
                SustainedPlan {
                    tps,
                    duration_secs,
                    key_cycle,
                },
                Some(nonces),
                make_task,
                Some(context.send_done),
                context.spinner,
            )
            .await
        },
    )
    .await
}

/// Drive the burst-mode pipeline: fan out one `interchainTransfer` per derived
/// signer (with bounded concurrency and 429-aware retries), run the batch
/// Stellar verifier on the confirmed set, and hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    transfer: TransferContext,
    stellar: &StellarTargets,
    stellar_recipient_addr: &str,
    sizing: &RunSizing,
) -> eyre::Result<()> {
    let num_txs = sizing.num_keys;

    let test_start = Instant::now();
    let burst = super::its_evm_source::run_its_burst(
        super::its_evm_source::ItsEvmSubmitter {
            rpc_url: transfer.rpc_url.parse()?,
            its_proxy: transfer.its_proxy_addr,
            token_id: transfer.token_id.into(),
            destination_chain: args.destination_axelar_id.to_string(),
            receiver: transfer.receiver_bytes.clone(),
            amount: transfer.amount_per_tx,
            gas_value: super::units::Wei::from_u256(transfer.gas_value),
            gas_arg_scaling_factor: transfer.gas_arg_scaling_factor,
        },
        &transfer.derived,
    )
    .await?;
    finish_burst(
        args,
        StellarItsTarget {
            rpc_url: stellar.rpc.clone(),
            network_type: stellar.network_type.clone(),
            gateway_contract: stellar.gateway_addr.clone(),
            signer_pk: stellar.signer_pk,
        },
        burst,
        ItsBurstReport {
            destination_address: stellar_recipient_addr.to_string(),
            num_txs: sizing.total_expected,
            num_keys: num_txs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        test_start,
    )
    .await
}
