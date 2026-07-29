//! EVM -> Sui ITS load test.
//!
//! Pre-conditions handled outside axe (one-time per network):
//!   1. A Sui-side AXE coin is registered on Sui ITS
//!      (`axelar-contract-deployments/sui/its.js register-coin-from-info`).
//!      The resulting tokenId is stored under
//!      `chains.sui.contracts.AXE.objects.TokenId`.
//!   2. The same tokenId is linked on the EVM source-chain ITS via the
//!      `axelar-contract-deployments` link-token flow. After linking, the
//!      source signer must hold a balance of the resulting ERC-20.
//!
//! axe verifies (1) by reading the chain config and (2) by calling
//! `InterchainTokenService.interchainTokenAddress(tokenId)` on the source
//! chain; either missing precondition triggers a clear bail.
//!
//! Destination-side execution is driven by the cgp-sui relayer auto-calling
//! `example::its::receive_interchain_transfer<T>` on `MessageApproved`. We
//! reuse `verify::verify_onchain_sui_gmp` against the `ItsChannelId` since
//! the gateway emits the same `MessageExecuted` event regardless of who
//! called `gateway::execute`.

use std::time::Instant;

use super::its_prerequisites::{self, GatewayRequirement};
use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity};
use super::run_sizing::{RunMode, RunSizing, SustainedPlan};
use super::{
    LoadTestArgs, check_evm_balance, finalize_sui_dest_run_its, load_sui_main_wallet,
    read_sui_axe_token_id, sui_its_dest_lookup, validate_evm_rpc,
};
use crate::config::ChainsConfig;
use crate::evm::{ERC20, InterchainTokenService};
use crate::ui;
use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;

/// Resolved EVM source-chain context: signer, ITS proxy + linked token, RPC.
struct EvmContext {
    main_signer: PrivateKeySigner,
    rpc_url: String,
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    linked_token_addr: Address,
    gas_value_wei: u128,
}

/// Resolved Sui destination context: recipient bytes (for interchainTransfer
/// payload), Sui RPC, display address. (The ITS channel is logged for
/// operators but the hub verifier keys off gateway events, not the channel.)
struct SuiContext {
    recipient_bytes: Bytes,
    recipient_display: String,
    rpc: String,
}

pub async fn run(args: LoadTestArgs, _run_start: Instant, sizing: RunSizing) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::Required)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv(
        "protocol",
        "ITS (interchainTransfer via hub, Sui destination)",
    );

    let evm = resolve_evm_context(&args, &cfg, evm_rpc_url.clone()).await?;
    let sui = resolve_sui_context(&args)?;

    let derived = derive_and_fund_signers(&evm, &sizing).await?;
    let amount_per_tx = U256::from(1u64);

    match sizing.mode() {
        RunMode::Burst { .. } => {
            run_burst_pipeline(&args, &evm, &sui, &derived, &sizing, amount_per_tx).await
        }
        RunMode::Sustained(plan) => {
            run_sustained_pipeline(&args, &evm, &sui, derived, &sizing, plan, amount_per_tx).await
        }
    }
}

async fn resolve_evm_context(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    rpc_url: String,
) -> eyre::Result<EvmContext> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let main_signer = parse_main_signer(args.private_key.as_deref())?;
    let main_addr = main_signer.address();
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    check_evm_balance(&read_provider, main_addr).await?;
    let balance: u128 = read_provider.get_balance(main_addr).await?.to();
    let eth = balance as f64 / 1e18;
    ui::kv("wallet", &format!("{main_addr} ({eth:.6} ETH)"));

    let evm_chain_cfg = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?;
    let its_proxy_addr: Address = evm_chain_cfg
        .contract_address("InterchainTokenService", src)?
        .parse()?;
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    let token_id_bytes = read_sui_axe_token_id(&args.config, dest, args.token_id.as_deref())?;
    let token_id = FixedBytes::<32>::from(token_id_bytes);
    ui::kv(
        "Sui token id",
        &format!("0x{}", hex::encode(token_id_bytes)),
    );

    let its = InterchainTokenService::new(its_proxy_addr, &read_provider);
    let linked_token_addr = its
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre!("ITS.interchainTokenAddress({token_id}) failed: {e}"))?;
    if linked_token_addr == Address::ZERO {
        eyre::bail!(
            "EVM source ITS at {its_proxy_addr} has no token linked to Sui AXE tokenId 0x{}. \
             Run the one-time off-axe link-token step from axelar-contract-deployments, then \
             ensure the source signer {main_addr} holds a balance of the resulting ERC-20.",
            hex::encode(token_id_bytes),
        );
    }
    ui::address("EVM linked ERC-20", &format!("{linked_token_addr}"));

    let token = ERC20::new(linked_token_addr, &read_provider);
    let main_token_balance = token.balanceOf(main_addr).call().await.unwrap_or_default();
    if main_token_balance == U256::ZERO {
        eyre::bail!(
            "source signer {main_addr} has zero balance of linked ERC-20 {linked_token_addr}. \
             Mint or transfer some AXE to it before re-running."
        );
    }
    ui::kv(
        "linked ERC-20 balance (main)",
        &main_token_balance.to_string(),
    );

    let gas_value_wei = super::its_evm_source::parse_gas_value_wei(
        args,
        super::its_evm_source::standard_gas_fallback(args.network),
    )
    .await?;
    ui::kv(
        "gas value",
        &format!("{gas_value_wei} wei (per-leg, x2 for hub)"),
    );

    Ok(EvmContext {
        main_signer,
        rpc_url,
        its_proxy_addr,
        token_id,
        linked_token_addr,
        gas_value_wei,
    })
}

fn resolve_sui_context(args: &LoadTestArgs) -> eyre::Result<SuiContext> {
    let dest = &args.destination_chain;
    let sui_wallet = load_sui_main_wallet()?;
    let recipient_bytes = Bytes::from(sui_wallet.address.as_bytes().to_vec());
    let recipient_display = sui_wallet.address_hex();
    ui::address("destination Sui address", &recipient_display);

    let (its_channel, rpc) = sui_its_dest_lookup(&args.config, dest, Some(&args.destination_rpc))?;
    ui::address("Sui ITS channel (destination)", &its_channel);

    Ok(SuiContext {
        recipient_bytes,
        recipient_display,
        rpc,
    })
}

async fn derive_and_fund_signers(
    evm: &EvmContext,
    sizing: &RunSizing,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let main_key: [u8; 32] = evm.main_signer.to_bytes().into();
    let derived = keypairs::derive_evm_signers(&main_key, sizing.num_keys)?;

    // Each tx forwards 2 * gas_value as msg.value (one hub leg, one
    // destination leg). Fund derived keys with 4x that headroom so each
    // can cover its share of sends plus tx fees.
    let read_provider = ProviderBuilder::new().connect_http(evm.rpc_url.parse()?);
    let per_call_msg_value_wei = evm.gas_value_wei.saturating_mul(2);
    keypairs::ensure_funded_evm_with_extra(
        &read_provider,
        &evm.main_signer,
        &derived,
        super::units::Wei::from_u128(per_call_msg_value_wei.saturating_mul(2)),
    )
    .await?;

    // Distribute linked AXE: each derived signer receives enough for its
    // share of the run. Burst: 1 per tx. Sustained: `key_cycle` per key,
    // since each key serves `key_cycle` rotations.
    let txs_per_key = sizing.transactions_per_key();
    let amount_per_key = U256::from(txs_per_key);

    let write_provider = ProviderBuilder::new()
        .wallet(evm.main_signer.clone())
        .connect_http(evm.rpc_url.parse()?);
    super::its_evm_source::distribute_tokens(
        &write_provider,
        evm.linked_token_addr,
        &derived,
        amount_per_key,
    )
    .await?;

    Ok(derived)
}

async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm: &EvmContext,
    sui: &SuiContext,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    amount_per_tx: U256,
) -> eyre::Result<()> {
    let num_txs = sizing.num_keys;
    let test_start = Instant::now();
    let gas_value = U256::from(evm.gas_value_wei);
    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);
    let burst = super::its_evm_source::run_its_burst(
        super::its_evm_source::ItsEvmSubmitter {
            rpc_url: evm.rpc_url.parse()?,
            its_proxy: evm.its_proxy_addr,
            token_id: evm.token_id.into(),
            destination_chain: args.destination_axelar_id.clone(),
            receiver: sui.recipient_bytes.clone(),
            amount: amount_per_tx,
            gas_value: super::units::Wei::from_u256(gas_value),
            gas_arg_scaling_factor,
        },
        derived,
    )
    .await?;
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            run: RunIdentity::burst(args),
            destination_address: sui.recipient_display.clone(),
            num_txs: sizing.total_expected,
            num_keys: num_txs,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        burst.metrics,
    );

    finalize_sui_dest_run_its(args, &mut report, &sui.rpc, test_start).await
}

async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    evm: &EvmContext,
    sui: &SuiContext,
    derived: Vec<PrivateKeySigner>,
    sizing: &RunSizing,
    plan: SustainedPlan,
    amount_per_tx: U256,
) -> eyre::Result<()> {
    let SustainedPlan {
        tps: tps_usize,
        duration_secs,
        key_cycle,
    } = plan;

    // Pre-fetch each derived signer's nonce so the rate-limited loop can
    // bump them locally per dispatch (avoids RPC round-trips on each tx).
    let read_provider = ProviderBuilder::new().connect_http(evm.rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for s in &derived {
        let n = read_provider.get_transaction_count(s.address()).await?;
        nonces.push(n);
    }

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    let test_start = Instant::now();
    let gas_value = U256::from(evm.gas_value_wei);
    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);
    let make_task = super::its_evm_source::its_sustained_tasks(
        super::its_evm_source::ItsEvmSubmitter {
            rpc_url: evm.rpc_url.parse()?,
            its_proxy: evm.its_proxy_addr,
            token_id: evm.token_id.into(),
            destination_chain: args.destination_axelar_id.clone(),
            receiver: sui.recipient_bytes.clone(),
            amount: amount_per_tx,
            gas_value: super::units::Wei::from_u256(gas_value),
            gas_arg_scaling_factor,
        },
        derived,
        None,
        false,
    );

    let result = super::sustained::run_sustained_loop(
        SustainedPlan {
            tps: tps_usize,
            duration_secs,
            key_cycle,
        },
        Some(nonces),
        make_task,
        None,
        spinner,
    )
    .await?;

    let mut report = super::sustained::build_sustained_report(
        result,
        RunIdentity::sustained(args, plan),
        &sui.recipient_display,
        sizing.total_expected,
        sizing.num_keys,
    );

    finalize_sui_dest_run_its(args, &mut report, &sui.rpc, test_start).await
}

fn parse_main_signer(private_key: Option<&str>) -> eyre::Result<PrivateKeySigner> {
    let key = private_key.ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    key.parse::<PrivateKeySigner>()
        .map_err(|e| eyre!("invalid EVM private key: {e}"))
}
