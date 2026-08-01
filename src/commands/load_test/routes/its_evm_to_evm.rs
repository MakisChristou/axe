//! EVM to EVM ITS route.
//!
//! Mirrors `its_evm_to_sol` on the source side: deploy (or reuse) an
//! InterchainToken on the source EVM, deploy its remote counterpart on the
//! destination EVM via the ITS hub, then drive `interchainTransfer` calls
//! per ephemeral key.
//!
//! Differs only in the destination wiring:
//!   * The `interchainTransfer` `destinationAddress` is a 20-byte EVM
//!     address (the source signer's address by default, or `DEAD_ADDRESS`
//!     when no key is configured).
//!   * Remote-deploy waiting uses `verify::wait_for_its_remote_deploy`
//!     (EVM-destination variant) instead of the Solana-destination variant.
//!   * Final verification uses `verify::verify_onchain_evm_its` (and its
//!     streaming sibling) — the same verifier that backs `its_sol_to_evm`,
//!     `its_stellar_to_evm`, and `its_xrpl_to_evm`.

use std::collections::HashMap;
use std::time::Instant;

use super::its_evm_source::{
    self, EvmSource, EvmTokenRunSizing as RunSizing, ItsContracts, deploy_its_token,
    derive_and_fund_keys, distribute_tokens, hub_gas_extra_per_key, init_evm_source,
    resolve_its_contracts,
};
use super::its_prerequisites::{self, GatewayRequirement};
use super::its_verification;
use super::its_verification::{EvmItsTarget, ItsBurstReport, finish_burst};
use super::metrics::ComputeUnitSummary;
use super::run_sizing::{RunMode, RunSizing as ValidatedRunSizing, SustainedPlan};
use super::{LoadTestArgs, validate_evm_rpc};
use crate::config::AxelarChainContract;
use crate::config::ChainContract;
use crate::config::ChainsConfig;
use crate::evm::InterchainTokenService;
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

    let source_rpc_url = args.source_rpc.to_string();
    let dest_rpc_url = args.destination_rpc.to_string();
    validate_evm_rpc(&source_rpc_url).await?;
    validate_evm_rpc(&dest_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config).await?;
    its_prerequisites::verify(
        &cfg,
        &args.destination_axelar_id,
        GatewayRequirement::AmplifierOnly,
    )?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let evm_source = init_evm_source(&args, &source_rpc_url).await?;
    let its = resolve_its_contracts(&cfg, src)?;
    let dest_gateway_addr = resolve_dest_gateway(&cfg, dest)?;
    let receiver = derive_receiver(&evm_source);
    ui::address("receiver", &format!("{receiver}"));

    let gas_value_wei = its_evm_source::standard_gas_value_wei(&args).await?;
    let gas_value = U256::from(gas_value_wei);
    let mut sizing = RunSizing::standard(validated_sizing);

    let token = resolve_or_deploy_token(
        &args,
        &evm_source,
        &its,
        &source_rpc_url,
        &dest_rpc_url,
        &sizing,
        gas_value,
    )
    .await?;

    rescale_token_sizing(&mut sizing, &source_rpc_url, token.token_addr).await?;

    if let Some(ref deploy_msg_id) = token.deploy_message_id {
        // Use axelar IDs (not the config keys) — the ITS Hub records messages
        // under the chain's axelarId, which differs from the key for consensus
        // chains (e.g. key "avalanche" vs axelarId "Avalanche"). Passing the key
        // makes the hub `executable_messages` query never match.
        super::verify::wait_for_its_remote_deploy(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            deploy_msg_id,
            dest_gateway_addr,
            &dest_rpc_url,
        )
        .await?;
    }

    let derived = derive_and_fund_keys(
        &evm_source.signer,
        &evm_source.main_key,
        &source_rpc_url,
        sizing.num_keys,
        hub_gas_extra_per_key(&sizing, super::units::Wei::from_u128(gas_value_wei)),
        &args.source_axelar_id,
    )
    .await?;

    let token_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(source_rpc_url.parse()?);
    distribute_tokens(
        &token_provider,
        token.token_addr,
        &derived,
        sizing.amount_per_key,
    )
    .await?;

    let receiver_bytes = Bytes::from(receiver.as_slice().to_vec());

    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id)
            .await;

    let targets = TransferTargets {
        its_proxy_addr: its.its_proxy_addr,
        token_id: token.token_id,
        gas_value,
        gas_arg_scaling_factor,
        receiver_bytes,
    };

    let mode = sizing.mode();
    let pipeline = PipelineContext {
        args,
        cfg,
        source_rpc_url,
        destination_rpc_url: dest_rpc_url,
        destination_gateway: dest_gateway_addr,
        derived,
        sizing,
        targets,
    };
    match mode {
        RunMode::Sustained(plan) => run_sustained_pipeline(pipeline, plan).await,
        RunMode::Burst { .. } => run_burst_pipeline(pipeline).await,
    }
}

async fn rescale_token_sizing(
    sizing: &mut RunSizing,
    source_rpc_url: &str,
    token: Address,
) -> eyre::Result<()> {
    let provider = ProviderBuilder::new().connect_http(source_rpc_url.parse()?);
    super::its_evm_source::rescale_sizing_for_decimals(
        &mut sizing.amount_per_tx,
        &mut sizing.amount_per_key,
        &mut sizing.total_supply,
        &provider,
        token,
    )
    .await
    .map(|_| ())
}

/// Resolved interchain token: cached, user-supplied, or freshly deployed.
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

/// Per-tx send parameters consumed by both the sustained and burst pipelines.
struct TransferTargets {
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    gas_value: U256,
    /// Exponent applied to the `gasValue` *function argument* (msg.value
    /// stays in EVM-wei). See `execute_interchain_transfer` and
    /// `read_gas_arg_scaling_factor` for the rationale — Hedera = 10,
    /// others = 0.
    gas_arg_scaling_factor: u32,
    receiver_bytes: Bytes,
}

/// Resolve the EVM AxelarGateway on the destination chain — used by both
/// the remote-deploy waiter and the per-tx verifier.
fn resolve_dest_gateway(cfg: &ChainsConfig, dest: &str) -> eyre::Result<Address> {
    let dest_cfg = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre!("destination chain '{dest}' not found in config"))?;
    let gw: Address = dest_cfg
        .contract_address(ChainContract::AxelarGateway, dest)?
        .parse()?;
    ui::address("AxelarGateway (destination)", &format!("{gw}"));
    Ok(gw)
}

/// Receiver wallet for the InterchainTransfer. Must be an EOA on the
/// destination chain — passing the ITS proxy reverts EVM estimation since
/// ITS won't transfer to its own address. Defaults to the source signer's
/// address so test runs accumulate balance at a wallet the user owns.
fn derive_receiver(evm_source: &EvmSource) -> Address {
    evm_source.deployer_address
}

/// Resolve the ITS token: honour `--token-id`, then fall back to the
/// source/dest cache, and finally deploy fresh if nothing reusable exists.
async fn resolve_or_deploy_token(
    args: &LoadTestArgs,
    evm_source: &EvmSource,
    its: &ItsContracts,
    evm_rpc_url: &str,
    dest_rpc_url: &str,
    sizing: &RunSizing,
    gas_value: U256,
) -> eyre::Result<TokenIdentity> {
    let src = &args.source_chain;
    let write_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    let needed = sizing.amount_per_tx * U256::from(sizing.num_keys);
    let config_axe = super::helpers::reusable_config_axe(
        &args.config,
        src,
        its.its_proxy_addr,
        &write_provider,
        evm_source.deployer_address,
        needed,
    )
    .await?;

    if let Some(token_id) = args.token_id.as_deref() {
        return resolve_provided_token(
            args,
            its,
            token_id,
            dest_rpc_url,
            gas_value,
            &write_provider,
        )
        .await;
    }
    if let Some((tid, addr)) = config_axe {
        ui::kv("token ID (chains-config)", &format!("{tid}"));
        ui::address("token address", &format!("{addr}"));
        return Ok((tid, addr, None).into());
    }
    resolve_cached_or_deploy(args, evm_source, its, sizing, gas_value, &write_provider).await
}

async fn resolve_provided_token<P: Provider>(
    args: &LoadTestArgs,
    its: &ItsContracts,
    value: &str,
    destination_rpc: &str,
    gas_value: U256,
    provider: &P,
) -> eyre::Result<TokenIdentity> {
    let token_id: FixedBytes<32> = value
        .parse()
        .map_err(|e| eyre!("invalid --token-id: {e}"))?;
    let token_addr = InterchainTokenService::new(its.its_proxy_addr, provider)
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
    ui::kv("token ID (provided)", &format!("{token_id}"));
    ui::address("token address", &format!("{token_addr}"));

    let destination = ProviderBuilder::new().connect_http(destination_rpc.parse()?);
    if !destination.get_code_at(token_addr).await?.is_empty() {
        return Ok((token_id, token_addr, None).into());
    }
    let salt_value = super::find_cached_salt(value).await.ok_or_else(|| {
        eyre!(
            "token {value} is not registered on '{}' and no deploy salt is cached; \
             run an ITS test once from the token's home chain so axe records its salt",
            args.destination_chain
        )
    })?;
    let salt = salt_value
        .parse()
        .map_err(|e| eyre!("cached salt for {value} is invalid: {e}"))?;
    let message_id = its_evm_source::remote_deploy_existing_token(
        provider,
        its.its_factory_addr,
        salt,
        &args.destination_axelar_id,
        gas_value,
    )
    .await?;
    Ok((token_id, token_addr, Some(message_id)).into())
}

async fn resolve_cached_or_deploy<P: Provider>(
    args: &LoadTestArgs,
    evm_source: &EvmSource,
    its: &ItsContracts,
    sizing: &RunSizing,
    gas_value: U256,
    provider: &P,
) -> eyre::Result<TokenIdentity> {
    if let Some(cached) =
        its_evm_source::cached_evm_token(&args.source_chain, &args.destination_chain).await?
    {
        let needed = sizing.amount_per_tx * U256::from(sizing.num_keys);
        let balance = its_evm_source::token_balance(
            provider,
            cached.token_address,
            evm_source.deployer_address,
        )
        .await;
        if balance >= needed {
            ui::info(&format!(
                "reusing cached ITS token: {}",
                cached.token_address
            ));
            ui::kv("token ID (cached)", &format!("{}", cached.token_id));
            return Ok((cached.token_id, cached.token_address, None).into());
        }
        ui::warn(&format!(
            "cached token has insufficient supply ({balance} < {needed}), deploying fresh..."
        ));
    }
    Ok(deploy_its_token(
        provider,
        its.its_factory_addr,
        evm_source.deployer_address,
        &args.destination_axelar_id,
        sizing.total_supply,
        &args.source_chain,
        gas_value,
    )
    .await?
    .into())
}

/// Compute the per-key hub-gas funding amount for this run. Burst mode fires
/// each key once; sustained mode fires each key `ceil(duration / key_cycle)`
/// times with a 20% buffer.
struct PipelineContext {
    args: LoadTestArgs,
    cfg: ChainsConfig,
    source_rpc_url: String,
    destination_rpc_url: String,
    destination_gateway: Address,
    derived: Vec<PrivateKeySigner>,
    sizing: RunSizing,
    targets: TransferTargets,
}

async fn run_sustained_pipeline(
    pipeline: PipelineContext,
    plan: SustainedPlan,
) -> eyre::Result<()> {
    let PipelineContext {
        args,
        cfg,
        source_rpc_url,
        destination_rpc_url: dest_rpc_url,
        destination_gateway: dest_gateway_addr,
        derived,
        sizing,
        targets,
    } = pipeline;
    let SustainedPlan {
        tps,
        duration_secs,
        key_cycle,
    } = plan;

    let nonce_provider = ProviderBuilder::new().connect_http(source_rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for signer in &derived {
        let n = nonce_provider
            .get_transaction_count(signer.address())
            .await?;
        nonces.push(n);
    }

    let has_voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, &args.source_chain)
        .is_ok();
    let submitter = its_evm_source::ItsEvmSubmitter {
        rpc_url: source_rpc_url.parse()?,
        its_proxy: targets.its_proxy_addr,
        token_id: targets.token_id.into(),
        destination_chain: args.destination_axelar_id.to_string(),
        receiver: targets.receiver_bytes.clone(),
        amount: sizing.amount_per_tx,
        gas_value: super::units::Wei::from_u256(targets.gas_value),
        gas_arg_scaling_factor: targets.gas_arg_scaling_factor,
    };
    let destination_address = targets.its_proxy_addr.to_string();
    its_verification::run_sustained(
        &args,
        EvmItsTarget {
            gateway_addr: dest_gateway_addr,
            rpc_url: dest_rpc_url,
        },
        &format!("[0/{duration_secs}s] starting sustained ITS send..."),
        &destination_address,
        sizing.total_expected,
        sizing.num_keys,
        |context| async move {
            let make_task = its_evm_source::its_sustained_tasks(
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

async fn run_burst_pipeline(pipeline: PipelineContext) -> eyre::Result<()> {
    let PipelineContext {
        args,
        source_rpc_url,
        destination_rpc_url: dest_rpc_url,
        destination_gateway: dest_gateway_addr,
        derived,
        sizing,
        targets,
        ..
    } = pipeline;
    let num_txs = sizing.num_keys;

    let test_start = Instant::now();
    let burst = its_evm_source::run_its_burst(
        its_evm_source::ItsEvmSubmitter {
            rpc_url: source_rpc_url.parse()?,
            its_proxy: targets.its_proxy_addr,
            token_id: targets.token_id.into(),
            destination_chain: args.destination_axelar_id.to_string(),
            receiver: targets.receiver_bytes.clone(),
            amount: sizing.amount_per_tx,
            gas_value: super::units::Wei::from_u256(targets.gas_value),
            gas_arg_scaling_factor: targets.gas_arg_scaling_factor,
        },
        &derived,
    )
    .await?;
    let total_failed = burst.metrics.iter().filter(|m| !m.is_success()).count() as u64;

    if total_failed > 0 {
        let mut error_counts: HashMap<String, u64> = HashMap::new();
        for m in burst.metrics.iter().filter(|m| !m.is_success()) {
            let reason = m
                .error()
                .unwrap_or("unknown")
                .chars()
                .take(120)
                .collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    finish_burst(
        &args,
        EvmItsTarget {
            gateway_addr: dest_gateway_addr,
            rpc_url: dest_rpc_url.to_string(),
        },
        burst,
        ItsBurstReport {
            destination_address: format!("{}", targets.its_proxy_addr),
            num_txs: sizing.total_expected,
            num_keys: num_txs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        test_start,
    )
    .await
}
