//! EVM -> EVM ITS load test.
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

use std::time::Instant;

use super::its_evm_source::{
    self, EvmSource, EvmTokenRunSizing as RunSizing, ItsContracts, deploy_its_token,
    derive_and_fund_keys, distribute_tokens, init_evm_source, resolve_its_contracts,
};
use super::its_prerequisites::{self, GatewayRequirement};
use super::its_verification::{
    EvmItsTarget, ItsBurstReport, ItsVerificationRoute, ItsVerificationSession, finish_burst,
};
use super::metrics::ComputeUnitSummary;
use super::run_sizing::RunSizing as ValidatedRunSizing;
use super::{LoadTestArgs, read_its_cache, validate_evm_rpc};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenService};
use crate::ui;
use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let source_rpc_url = args.source_rpc.clone();
    let dest_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&source_rpc_url).await?;
    validate_evm_rpc(&dest_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
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
    let mut sizing = RunSizing::standard(ValidatedRunSizing::new(&args)?);

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

    // compute_run_sizing assumes 18 decimals (the EVM-source convention). For
    // Hedera HTS-fork AXE the registered token is 6 decimals — 1e16 sub-units
    // there is 10^10 AXE, way over wallet balance and rejected by the source
    // burn. Rescale after we know the real on-chain decimals.
    {
        let read_provider = ProviderBuilder::new().connect_http(source_rpc_url.parse()?);
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
        hub_gas_extra_per_key(&sizing, gas_value_wei),
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
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);

    let targets = TransferTargets {
        its_proxy_addr: its.its_proxy_addr,
        token_id: token.token_id,
        gas_value,
        gas_arg_scaling_factor,
        receiver_bytes,
    };

    let pipeline = PipelineContext {
        args: &args,
        cfg: &cfg,
        source_rpc_url: &source_rpc_url,
        destination_rpc_url: &dest_rpc_url,
        destination_gateway: dest_gateway_addr,
        derived: &derived,
        sizing: &sizing,
        targets: &targets,
    };
    if !sizing.is_burst() {
        run_sustained_pipeline(&pipeline).await
    } else {
        run_burst_pipeline(&pipeline).await
    }
}

/// Resolved interchain token: cached, user-supplied, or freshly deployed.
struct TokenIdentity {
    token_id: FixedBytes<32>,
    token_addr: Address,
    deploy_message_id: Option<String>,
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
    let gw: Address = dest_cfg.contract_address("AxelarGateway", dest)?.parse()?;
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
    let dest = &args.destination_chain;
    // The ITS edge / hub identify the destination by its axelarId, which differs
    // from the config key for consensus chains ("avalanche" key vs "Avalanche"
    // axelarId). Passing the key to deployRemoteInterchainToken makes the source
    // revert with `UntrustedChain()`, so the on-chain destination name must be
    // the axelarId. (`dest` stays the key for cache/config lookups.)
    let dest_its = args.destination_axelar_id.as_str();
    let write_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    let its_service = InterchainTokenService::new(its.its_proxy_addr, &write_provider);

    // Resolution order: --token-id → chains-config `contracts.AXE.tokenId`
    // (only if the configured wallet actually holds enough of it) → local file
    // cache → fresh deploy. The chains-config layer lets CI runs (whose wallet
    // already holds the AXE supply) skip the source + remote deploy and
    // collapse to a single interchainTransfer; a different wallet with no AXE
    // balance falls through to a fresh deploy (see reusable_config_axe).
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

    let (token_id, token_addr, deploy_message_id) = if let Some(ref tid) = args.token_id {
        let token_id: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        let addr = its_service
            .interchainTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (provided)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        // Reuse an existing token on a chain it isn't on yet: if the destination
        // lacks this token, remote-deploy it there using the salt axe recorded
        // when it first deployed the token (its local cache) — no fresh mint.
        let dest_provider = ProviderBuilder::new().connect_http(dest_rpc_url.parse()?);
        let dest_has_token = !dest_provider.get_code_at(addr).await?.is_empty();
        let deploy_message_id = if dest_has_token {
            None
        } else {
            let salt_hex = super::find_cached_salt(tid).ok_or_else(|| {
                eyre!(
                    "token {tid} is not registered on '{dest}' and no deploy salt is cached; \
                     run an ITS test once from the token's home chain so axe records its salt"
                )
            })?;
            let salt: FixedBytes<32> = salt_hex
                .parse()
                .map_err(|e| eyre!("cached salt for {tid} is invalid: {e}"))?;
            let msg_id = super::its_evm_source::remote_deploy_existing_token(
                &write_provider,
                its.its_factory_addr,
                salt,
                dest_its,
                gas_value,
            )
            .await?;
            Some(msg_id)
        };
        (token_id, addr, deploy_message_id)
    } else if let Some((tid, addr)) = config_axe {
        ui::kv("token ID (chains-config)", &format!("{tid}"));
        ui::address("token address", &format!("{addr}"));
        (tid, addr, None)
    } else {
        let cache = read_its_cache(src, dest);
        let cached = cache
            .get("tokenId")
            .and_then(|v| v.as_str())
            .and_then(|tid| tid.parse::<FixedBytes<32>>().ok())
            .zip(
                cache
                    .get("tokenAddress")
                    .and_then(|v| v.as_str())
                    .and_then(|a| a.parse::<Address>().ok()),
            );

        if let Some((tid, addr)) = cached {
            let token = ERC20::new(addr, &write_provider);
            let needed = sizing.amount_per_tx * U256::from(sizing.num_keys);
            let balance = token
                .balanceOf(evm_source.deployer_address)
                .call()
                .await
                .unwrap_or_default();
            if balance >= needed {
                ui::info(&format!("reusing cached ITS token: {addr}"));
                ui::kv("token ID (cached)", &format!("{tid}"));
                (tid, addr, None)
            } else {
                ui::warn(&format!(
                    "cached token has insufficient supply ({balance} < {needed}), deploying fresh..."
                ));
                deploy_its_token(
                    &write_provider,
                    its.its_factory_addr,
                    evm_source.deployer_address,
                    dest_its,
                    sizing.total_supply,
                    src,
                    gas_value,
                )
                .await?
            }
        } else {
            deploy_its_token(
                &write_provider,
                its.its_factory_addr,
                evm_source.deployer_address,
                dest_its,
                sizing.total_supply,
                src,
                gas_value,
            )
            .await?
        }
    };

    Ok(TokenIdentity {
        token_id,
        token_addr,
        deploy_message_id,
    })
}

/// Compute the per-key hub-gas funding amount for this run. Burst mode fires
/// each key once; sustained mode fires each key `ceil(duration / key_cycle)`
/// times with a 20% buffer.
fn hub_gas_extra_per_key(sizing: &RunSizing, gas_value_wei: u128) -> u128 {
    let hub_gas_value_wei = gas_value_wei.saturating_mul(2);
    if sizing.is_burst() {
        hub_gas_value_wei
    } else {
        let rounds = sizing.transactions_per_key();
        let buffered = rounds + rounds / 5 + 1;
        hub_gas_value_wei.saturating_mul(buffered as u128)
    }
}

#[derive(Clone, Copy)]
struct PipelineContext<'a> {
    args: &'a LoadTestArgs,
    cfg: &'a ChainsConfig,
    source_rpc_url: &'a str,
    destination_rpc_url: &'a str,
    destination_gateway: Address,
    derived: &'a [PrivateKeySigner],
    sizing: &'a RunSizing,
    targets: &'a TransferTargets,
}

async fn run_sustained_pipeline(pipeline: &PipelineContext<'_>) -> eyre::Result<()> {
    let PipelineContext {
        args,
        cfg,
        source_rpc_url,
        destination_rpc_url: dest_rpc_url,
        destination_gateway: dest_gateway_addr,
        derived,
        sizing,
        targets,
    } = *pipeline;
    let (tps, duration_secs, key_cycle) = sizing.sustained().expect("sustained mode");

    let nonce_provider = ProviderBuilder::new().connect_http(source_rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for signer in derived {
        let n = nonce_provider
            .get_transaction_count(signer.address())
            .await?;
        nonces.push(n);
    }

    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_chain)
        .is_ok();
    let mut verification = ItsVerificationSession::start(
        ItsVerificationRoute::from_args(args),
        EvmItsTarget {
            gateway_addr: dest_gateway_addr,
            rpc_url: dest_rpc_url.to_string(),
        },
    );

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    verification.attach_spinner(spinner.clone())?;

    let test_start = Instant::now();
    let make_task = its_evm_source::its_sustained_tasks(
        its_evm_source::ItsEvmSubmitter {
            rpc_url: source_rpc_url.parse()?,
            its_proxy: targets.its_proxy_addr,
            token_id: targets.token_id,
            destination_chain: args.destination_axelar_id.clone(),
            receiver: targets.receiver_bytes.clone(),
            amount: sizing.amount_per_tx,
            gas_value: targets.gas_value,
            gas_arg_scaling_factor: targets.gas_arg_scaling_factor,
        },
        derived.to_vec(),
        Some(verification.sender()),
        has_voting_verifier,
    );

    let result = super::sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        Some(nonces),
        make_task,
        Some(verification.send_done()),
        spinner,
    )
    .await?;
    verification
        .finish_sustained(
            args,
            result,
            &format!("{}", targets.its_proxy_addr),
            sizing.total_expected,
            sizing.num_keys,
            test_start,
        )
        .await
}

async fn run_burst_pipeline(pipeline: &PipelineContext<'_>) -> eyre::Result<()> {
    let PipelineContext {
        args,
        source_rpc_url,
        destination_rpc_url: dest_rpc_url,
        destination_gateway: dest_gateway_addr,
        derived,
        sizing,
        targets,
        ..
    } = *pipeline;
    let num_txs = sizing
        .burst_count()
        .expect("burst pipeline requires burst sizing");

    let test_start = Instant::now();
    let burst = its_evm_source::run_its_burst(
        its_evm_source::ItsEvmSubmitter {
            rpc_url: source_rpc_url.parse()?,
            its_proxy: targets.its_proxy_addr,
            token_id: targets.token_id,
            destination_chain: args.destination_axelar_id.clone(),
            receiver: targets.receiver_bytes.clone(),
            amount: sizing.amount_per_tx,
            gas_value: targets.gas_value,
            gas_arg_scaling_factor: targets.gas_arg_scaling_factor,
        },
        derived,
    )
    .await?;
    let total_failed = burst.metrics.iter().filter(|m| !m.is_success()).count() as u64;

    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
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
        args,
        &EvmItsTarget {
            gateway_addr: dest_gateway_addr,
            rpc_url: dest_rpc_url.to_string(),
        },
        burst,
        ItsBurstReport {
            destination_address: format!("{}", targets.its_proxy_addr),
            num_txs: args.num_txs,
            num_keys: num_txs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        test_start,
    )
    .await
}
