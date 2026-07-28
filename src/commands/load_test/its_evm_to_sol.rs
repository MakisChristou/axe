use std::time::Instant;

use super::its_evm_source::{
    self, EvmSource, EvmTokenRunSizing as RunSizing, ItsContracts, deploy_its_token,
    derive_and_fund_keys, distribute_tokens, init_evm_source, resolve_its_contracts,
};
use super::its_prerequisites::{self, GatewayRequirement};
use super::its_verification::{
    ItsBurstReport, ItsVerificationRoute, ItsVerificationSession, SolanaItsTarget, finish_burst,
};
use super::metrics::ComputeUnitSummary;
use super::run_sizing::RunSizing as ValidatedRunSizing;
use super::{LoadTestArgs, read_its_cache, validate_evm_rpc, validate_solana_rpc};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenService};
use crate::ui;
use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use solana_sdk::signer::Signer;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::AmplifierOnly)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let evm_source = init_evm_source(&args, &evm_rpc_url).await?;
    let its = resolve_its_contracts(&cfg, src)?;
    let gas_value_wei = its_evm_source::standard_gas_value_wei(&args).await?;
    let gas_value = U256::from(gas_value_wei);
    let mut sizing = RunSizing::standard(ValidatedRunSizing::new(&args)?);

    let token =
        resolve_or_deploy_token(&args, &evm_source, &its, &evm_rpc_url, &sizing, gas_value).await?;

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
        super::verify::wait_for_its_remote_deploy_to_solana(
            &args.config,
            src,
            dest,
            deploy_msg_id,
            &args.destination_rpc,
            args.network,
        )
        .await?;
    }

    let derived = derive_and_fund_keys(
        &evm_source.signer,
        &evm_source.main_key,
        &evm_rpc_url,
        sizing.num_keys,
        hub_gas_extra_per_key(&sizing, gas_value_wei),
        &args.source_axelar_id,
    )
    .await?;

    let token_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    distribute_tokens(
        &token_provider,
        token.token_addr,
        &derived,
        sizing.amount_per_key,
    )
    .await?;

    let sol_keypair = crate::solana::load_keypair(args.keypair.as_deref())?;
    let receiver_bytes = Bytes::from(sol_keypair.pubkey().to_bytes().to_vec());

    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);

    let targets = TransferTargets {
        its_proxy_addr: its.its_proxy_addr,
        token_id: token.token_id,
        gas_value,
        gas_arg_scaling_factor,
        receiver_bytes,
    };

    if !sizing.is_burst() {
        run_sustained_pipeline(&args, &cfg, &evm_rpc_url, &derived, &sizing, &targets).await
    } else {
        run_burst_pipeline(&args, &evm_rpc_url, &derived, &sizing, &targets).await
    }
}

/// Resolved interchain token: cached, user-supplied, or freshly deployed.
/// `deploy_message_id` is `Some` only when the helper performed a remote
/// deploy in this run.
struct TokenIdentity {
    token_id: FixedBytes<32>,
    token_addr: Address,
    deploy_message_id: Option<String>,
}

/// Per-tx send parameters consumed by both the sustained and burst pipelines:
/// the ITS service to call, the token to push through it, the gas attached to
/// each interchain transfer, and the Solana recipient.
struct TransferTargets {
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    gas_value: U256,
    /// See `its_evm_source::read_gas_arg_scaling_factor` — Hedera = 10,
    /// other EVM chains = 0.
    gas_arg_scaling_factor: u32,
    receiver_bytes: Bytes,
}

/// Parse the user-supplied gas value (wei), defaulting via the relayer-aware
/// `estimateGasFee` quote for the route, and emit the matching UI line.
/// Resolve the ITS token to use this run: honour `--token-id`, then fall back
/// to the source/dest cache (deploying fresh if the cached token has
/// insufficient supply or no longer exists), and finally deploy a brand-new
/// token if no cache hit.
async fn resolve_or_deploy_token(
    args: &LoadTestArgs,
    evm_source: &EvmSource,
    its: &ItsContracts,
    evm_rpc_url: &str,
    sizing: &RunSizing,
    gas_value: U256,
) -> eyre::Result<TokenIdentity> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let write_provider = ProviderBuilder::new()
        .wallet(evm_source.signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    let its_service = InterchainTokenService::new(its.its_proxy_addr, &write_provider);

    // Resolution order: --token-id (CLI override) → chains-config
    // `contracts.AXE.tokenId` (per-source pre-registration, lets CI skip the
    // deploy + hub-routed remote-deploy and collapse to a single
    // interchainTransfer — but only when the configured wallet actually holds
    // the AXE; a wallet with no balance falls through to a fresh deploy) →
    // local file cache (per src+dst) → fresh deploy.
    //
    // Hedera special-cases: auto-deploy reverts with
    // `InitialSupplyUnsupported` (and the broader path is currently broken
    // upstream — see TODOs in the workflow + script). For Hedera-source we
    // require an explicit pre-registered token; the error message points at
    // the deployments-repo Hedera setup.
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

    let (token_id, token_addr, deploy_message_id) = if args.source_axelar_id == "hedera" {
        // Hedera ITS uses `registeredTokenAddress` (HTS tokens lack
        // deterministic addresses, so `interchainTokenAddress` was removed
        // in the fork — see contracts/hedera/README.md in commonprefix's
        // interchain-token-service@hedera-its).
        let token_id =
            super::helpers::resolve_hedera_axe_token(&args.config, src, args.token_id.as_deref())?;
        let addr = its_service
            .registeredTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (Hedera, pre-registered)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        (token_id, addr, None)
    } else if let Some(ref tid) = args.token_id {
        // User provided a token ID
        let token_id: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        let addr = its_service
            .interchainTokenAddress(token_id)
            .call()
            .await
            .map_err(|e| eyre!("failed to look up token address for {token_id}: {e}"))?;
        ui::kv("token ID (provided)", &format!("{token_id}"));
        ui::address("token address", &format!("{addr}"));
        (token_id, addr, None)
    } else if let Some((tid, addr)) = config_axe {
        ui::kv("token ID (chains-config)", &format!("{tid}"));
        ui::address("token address", &format!("{addr}"));
        (tid, addr, None)
    } else {
        // Check cache
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
            // Verify token still exists and deployer has enough balance
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
            } else if balance > U256::ZERO {
                ui::warn(&format!(
                    "cached token has insufficient supply ({balance} < {needed}), deploying fresh..."
                ));
                deploy_its_token(
                    &write_provider,
                    its.its_factory_addr,
                    evm_source.deployer_address,
                    dest,
                    sizing.total_supply,
                    src,
                    gas_value,
                )
                .await?
            } else {
                ui::warn("cached token no longer exists, deploying fresh...");
                deploy_its_token(
                    &write_provider,
                    its.its_factory_addr,
                    evm_source.deployer_address,
                    dest,
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
                dest,
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

/// Drive the sustained-mode pipeline: pre-fetch nonces, spawn the streaming
/// Solana ITS verifier, run the EVM sustained sender loop, stitch amplifier
/// timings back into the report, and hand off to `finish_report`.
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    targets: &TransferTargets,
) -> eyre::Result<()> {
    let dest = &args.destination_chain;
    let (tps, duration_secs, key_cycle) = sizing.sustained().expect("sustained mode");

    // Pre-fetch nonces.
    let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
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
        SolanaItsTarget {
            rpc_url: args.destination_rpc.clone(),
        },
    );

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    verification.attach_spinner(spinner.clone())?;

    let test_start = Instant::now();
    let make_task = its_evm_source::its_sustained_tasks(
        its_evm_source::ItsEvmSubmitter {
            rpc_url: evm_rpc_url.parse()?,
            its_proxy: targets.its_proxy_addr,
            token_id: targets.token_id.into(),
            destination_chain: dest.to_string(),
            receiver: targets.receiver_bytes.clone(),
            amount: sizing.amount_per_tx,
            gas_value: super::units::Wei::from_u256(targets.gas_value),
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

/// Drive the burst-mode pipeline: fan out parallel ITS interchain transfers
/// (with retry on rate limits), batch-verify on the Solana destination, and
/// hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm_rpc_url: &str,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    targets: &TransferTargets,
) -> eyre::Result<()> {
    let dest = &args.destination_chain;
    let num_txs = sizing
        .burst_count()
        .expect("burst pipeline requires burst sizing");

    let test_start = Instant::now();
    let burst = its_evm_source::run_its_burst(
        its_evm_source::ItsEvmSubmitter {
            rpc_url: evm_rpc_url.parse()?,
            its_proxy: targets.its_proxy_addr,
            token_id: targets.token_id.into(),
            destination_chain: dest.to_string(),
            receiver: targets.receiver_bytes.clone(),
            amount: sizing.amount_per_tx,
            gas_value: super::units::Wei::from_u256(targets.gas_value),
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
        &SolanaItsTarget {
            rpc_url: args.destination_rpc.clone(),
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
