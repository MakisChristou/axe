//! EVM to XRPL ITS route.
//!
//! Source-side flow is identical to `its_evm_to_sol.rs` (deploy/cache the
//! AXE token on the EVM source, distribute to derived signers, fire
//! `interchainTransfer` calls). The destination side polls the recipient
//! XRPL account's `account_tx` for an inbound `Payment` whose `message_id`
//! memo matches the second-leg id (the XRPL relayer attaches that memo).
//!
//! Token: requires the user to supply `--token-id <hex>` for the
//! interchain token registered between the EVM source and the XRPL gateway
//! (typically the canonical XRP token id, or a custom-registered IOU).
//! Native XRP works with no trust-line setup needed on the recipient side.

use alloy::primitives::Address;
use std::path::Path;
use std::time::Instant;

use super::its_prerequisites::{self, GatewayRequirement};
use super::its_verification;
use super::its_verification::{ItsBurstReport, XrplItsTarget, finish_burst};
use super::keypairs;
use super::metrics::ComputeUnitSummary;
use super::run_sizing::{RunMode, RunSizing, SustainedPlan};
use super::{LoadTestArgs, check_evm_balance, validate_evm_rpc};
use crate::config::AxelarChainContract;
use crate::config::ChainContract;
use crate::config::ChainsConfig;
use crate::cosmos::lcd_cosmwasm_smart_query;
use crate::evm::{
    EvmEndpoints, InterchainTokenService, connect_evm_signed, deterministic_view_failure,
};
use crate::retry::{retry_with_fallback, retry_with_fallback_all};
use crate::types::Network;
use crate::ui;
use crate::xrpl::{XrplClient, faucet_url_for_network, parse_address};
use alloy::{
    primitives::{Bytes, FixedBytes, U256},
    providers::Provider,
    signers::local::PrivateKeySigner,
};
use eyre::eyre;

/// Hard-coded default XRPL recipient.
///
/// Pinned per-network so the receiver is stable across runs and does not
/// depend on whichever signing key happens to be loaded. The sender (and
/// signer that pays for the EVM-side tx) is still derived live from
/// EVM_PRIVATE_KEY.
fn default_xrpl_recipient(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "rhnu1DRT9AmPmz9C78WoAiEyXFdaGvxgfk",
        _ => "r3Xqy7SVtkNQyCU9TZx46BAHFMhcJRopQh",
    }
}

/// Per-tx transfer amount (in token's smallest unit). Kept tiny so a
/// 3000-tx run costs minimal real funds on mainnet. axlXRP on EVM has 18
/// decimals; ITS truncates to 6 decimals on the XRPL side, so we use
/// 0.001 axlXRP = 1e15 wei → 1000 drops on XRPL.
const AMOUNT_PER_TX_WEI: u128 = 1_000_000_000_000_000;

pub async fn run(args: LoadTestArgs, sizing: RunSizing) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.to_string();
    // Private endpoint first, chains-config public RPC as failover.
    let source_rpc_urls = super::its_evm_source::source_rpc_candidates(&args).await;
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config).await?;
    its_prerequisites::verify(
        &cfg,
        dest,
        GatewayRequirement::Xrpl {
            axelar_id: &args.destination_axelar_id,
        },
    )?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let token_id =
        resolve_token_id(&cfg, args.token_id.as_deref(), &args.destination_axelar_id).await?;

    let evm_src = init_evm_source(&cfg, src, &source_rpc_urls, args.private_key.as_deref()).await?;

    let token_addr = verify_token_on_its(&evm_src, &source_rpc_urls, token_id).await?;

    let xrpl = setup_xrpl_recipient(&args.config, dest, args.network).await?;

    let (gas_value_wei, gas_value) = super::its_evm_source::standard_gas_value(&args).await?;

    let derived =
        derive_and_fund_evm_signers(&evm_src, &source_rpc_urls, gas_value_wei, &sizing).await?;

    distribute_and_approve_tokens(
        &evm_src,
        &source_rpc_urls,
        token_addr,
        token_id,
        &derived,
        &sizing,
    )
    .await?;

    // --- destination_address bytes for `interchainTransfer` ---
    // For XRPL destinations, ITS expects `asciiToBytes(r-address)` in the
    // destination_address arg. The relayer parses the bytes back to a
    // string and decodes the recipient AccountId.
    let receiver_bytes = Bytes::from(xrpl.recipient_addr.as_bytes().to_vec());

    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id)
            .await;

    let its_ctx = ItsCallCtx {
        its_proxy_addr: evm_src.its_proxy_addr,
        token_id,
        gas_value,
        gas_arg_scaling_factor,
        receiver_bytes,
        amount_per_tx: U256::from(AMOUNT_PER_TX_WEI),
    };

    match sizing.mode() {
        RunMode::Sustained(plan) => {
            run_sustained_pipeline(SustainedPipeline {
                args,
                cfg,
                source_rpc_urls,
                xrpl,
                derived,
                its_ctx,
                sizing,
                plan,
            })
            .await
        }
        RunMode::Burst { .. } => {
            run_burst_pipeline(&args, &source_rpc_urls, &xrpl, &derived, &its_ctx, &sizing).await
        }
    }
}

/// EVM source-side context: signer, raw key bytes for derivation, and the
/// resolved ITS proxy on the source chain.
struct EvmSource {
    signer: PrivateKeySigner,
    main_key: [u8; 32],
    its_proxy_addr: Address,
}

/// XRPL destination-side context: RPC URL and the recipient r-address that
/// all transfers target.
struct XrplDest {
    xrpl_rpc: String,
    recipient_addr: String,
}

/// Per-call inputs for `interchainTransfer`: ITS proxy, token id, gas value
/// (msg.value), pre-encoded recipient bytes, and per-tx token amount.
struct ItsCallCtx {
    its_proxy_addr: Address,
    token_id: FixedBytes<32>,
    gas_value: U256,
    /// See `its_evm_source::read_gas_arg_scaling_factor` — Hedera = 10,
    /// other EVM chains = 0.
    gas_arg_scaling_factor: u32,
    receiver_bytes: Bytes,
    amount_per_tx: U256,
}

/// Resolve the interchain token id: prefer the user-supplied `--token-id`,
/// else auto-discover from `XrplGateway`, else fall back to the canonical
/// XRP token id. Emits the matching UI lines for whichever path was taken.
async fn resolve_token_id(
    cfg: &ChainsConfig,
    user_token_id: Option<&str>,
    destination_axelar_id: &str,
) -> eyre::Result<FixedBytes<32>> {
    if let Some(tid) = user_token_id {
        let parsed: FixedBytes<32> = tid.parse().map_err(|e| eyre!("invalid --token-id: {e}"))?;
        ui::kv("token ID (provided)", tid);
        return Ok(parsed);
    }
    ui::info("looking up canonical XRP token id on XrplGateway...");
    match fetch_xrp_token_id(cfg, destination_axelar_id).await {
        Ok(id) => {
            ui::kv(
                "token ID (XrplGateway → XRP)",
                &format!("0x{}", hex::encode(id)),
            );
            Ok(FixedBytes::<32>::from(id))
        }
        Err(e) => {
            let canonical_hex = "ba5a21ca88ef6bba2bfff5088994f90e1077e2a1cc3dcc38bd261f00fce2824f";
            let canonical: FixedBytes<32> = format!("0x{canonical_hex}").parse()?;
            ui::warn(&format!(
                "XrplGateway lookup failed ({e}); falling back to canonical XRP token id 0x{canonical_hex}"
            ));
            ui::kv(
                "token ID (canonical fallback)",
                &format!("0x{canonical_hex}"),
            );
            Ok(canonical)
        }
    }
}

/// Build the EVM signer, sanity-check its native balance, and resolve the
/// source-chain ITS proxy address (logging it via UI).
async fn init_evm_source(
    cfg: &ChainsConfig,
    src: &str,
    rpc_urls: &[String],
    private_key: Option<&str>,
) -> eyre::Result<EvmSource> {
    let private_key = private_key.ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let endpoints = EvmEndpoints::connect(rpc_urls)?;
    check_evm_balance(endpoints.primary(), deployer_address).await?;
    let main_key: [u8; 32] = signer.to_bytes().into();

    let its_proxy_addr: Address = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?
        .contract_address(ChainContract::InterchainTokenService, src)?
        .parse()?;
    ui::address("ITS service", &format!("{its_proxy_addr}"));

    Ok(EvmSource {
        signer,
        main_key,
        its_proxy_addr,
    })
}

/// Verify the token id is registered on the source EVM ITS and report the
/// manager type if available. Returns the resolved EVM token address.
///
/// For an older ITS deployment (no `validTokenAddress` getter),
/// `interchainTokenAddress` is the only public lookup that doesn't revert
/// for unregistered token ids — so a successful return doesn't actually
/// prove registration; the resulting `interchainTransfer` may still revert
/// with `TakeTokenFailed` because no `TokenManager` was deployed for the
/// id. That's a runtime failure mode the load-test treats as a 0/N report.
async fn verify_token_on_its(
    evm_src: &EvmSource,
    rpc_urls: &[String],
    token_id: FixedBytes<32>,
) -> eyre::Result<Address> {
    let endpoints = EvmEndpoints::connect(rpc_urls)?;
    let its_proxy = evm_src.its_proxy_addr;
    let token_addr = retry_with_fallback(
        "token address lookup",
        endpoints.providers(),
        |e| !deterministic_view_failure(e),
        |p| async move {
            InterchainTokenService::new(its_proxy, p)
                .interchainTokenAddress(token_id)
                .call()
                .await
        },
    )
    .await
    .map_err(|e| {
        eyre!(
            "token id 0x{} not registered on EVM ITS: {e}",
            hex::encode(token_id)
        )
    })?;
    let its_service = InterchainTokenService::new(its_proxy, endpoints.primary());
    // Best-effort: report the manager type if the ITS deployment exposes the
    // getter (modern ITS does). Older deployments revert here for
    // unregistered token ids — which is itself diagnostic, so we surface the
    // condition as a warn rather than failing the whole run.
    match its_service.tokenManagerType(token_id).call().await {
        Ok(t) => ui::kv(
            "token manager type",
            &format!(
                "{t} ({})",
                match t {
                    0 => "NATIVE_INTERCHAIN_TOKEN",
                    1 => "MINT_BURN_FROM",
                    2 => "LOCK_UNLOCK",
                    3 => "LOCK_UNLOCK_FEE",
                    4 => "MINT_BURN",
                    _ => "unknown",
                }
            ),
        ),
        Err(_) => ui::warn(
            "ITS.tokenManagerType reverted — token may not be registered on this chain's ITS \
             (interchainTransfer will likely revert with TakeTokenFailed)",
        ),
    }
    ui::address("token address (EVM)", &format!("{token_addr}"));
    Ok(token_addr)
}

/// Resolve XRPL RPC + recipient and faucet-activate the recipient if the
/// account isn't already funded (testnet/devnet only).
async fn setup_xrpl_recipient(
    config: &Path,
    dest: &str,
    network: Network,
) -> eyre::Result<XrplDest> {
    let (xrpl_rpc, _xrpl_multisig, xrpl_network_type) =
        super::its_xrpl_to_evm::read_xrpl_chain_config(config, dest).await?;
    let xrpl_client = XrplClient::new(&xrpl_rpc);

    // Recipient is fixed, not derived: see `default_xrpl_recipient` above.
    let recipient_addr = default_xrpl_recipient(network).to_string();
    // Sanity-check the constant parses as an XRPL address; bail loudly if it
    // ever gets mistyped.
    parse_address(&recipient_addr)?;
    ui::address("XRPL recipient", &recipient_addr);

    // Activate the recipient if needed (testnet/devnet only).
    // Detect faucet from the actual RPC URL — devnet-amplifier mislabels
    // its xrpl networkType as "testnet" but uses a different ledger.
    if xrpl_client.account_info(&recipient_addr).await?.is_none() {
        if let Some(faucet) =
            faucet_url_for_network(&xrpl_rpc).or_else(|| faucet_url_for_network(&xrpl_network_type))
        {
            ui::info("activating XRPL recipient via faucet...");
            xrpl_client
                .fund_from_faucet(&recipient_addr, faucet)
                .await?;
            ui::success("recipient activated");
        } else {
            eyre::bail!(
                "XRPL recipient {recipient_addr} is not activated. Fund it with at least the \
                 base reserve (~10 XRP) before running on this network."
            );
        }
    }

    Ok(XrplDest {
        xrpl_rpc,
        recipient_addr,
    })
}

/// Parse the user-supplied gas value (wei), defaulting via
/// `default_gas_value_wei`. Returns both the raw `u128` and `U256`
/// representations; emits the matching UI line.
/// Derive ephemeral EVM signers and ensure each is funded for the planned
/// number of `interchainTransfer` calls (gas + msg.value × txs_per_key, ×2
/// safety multiplier).
async fn derive_and_fund_evm_signers(
    evm_src: &EvmSource,
    rpc_urls: &[String],
    gas_value_wei: u128,
    sizing: &RunSizing,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let derived = keypairs::derive_evm_signers(&evm_src.main_key, sizing.num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));
    // Multi-transport provider: funding reads/sends fail over to the public
    // RPC on transport-level errors.
    let funding_provider = connect_evm_signed(rpc_urls, evm_src.signer.clone())?;
    // Compute funding dynamically: each interchainTransfer costs roughly
    // GAS_LIMIT × gas_price for the call plus `gas_value` (msg.value to gas
    // service). xrpl-evm runs at ~137 gwei vs ~1 gwei on most EVM testnets,
    // so the static defaults underfund by 5-10×.
    let gas_price_wei: u128 = funding_provider
        .get_gas_price()
        .await
        .unwrap_or(1_000_000_000); // 1 gwei fallback
    const ITS_GAS_LIMIT: u128 = 1_000_000; // generous upper bound for ITS
    // ITS hub routing pays 2× gas_value per transfer (two commands).
    let hub_gas_value_wei = gas_value_wei.saturating_mul(2);
    let per_tx_native_cost = gas_price_wei.saturating_mul(ITS_GAS_LIMIT) + hub_gas_value_wei;
    let txs_per_key: u128 = if sizing.is_burst() {
        1
    } else {
        let rounds = sizing.transactions_per_key();
        (rounds + rounds / 5 + 1) as u128
    };
    // 2× safety multiplier in case gas price doubles mid-test.
    let gas_extra_per_key = per_tx_native_cost
        .saturating_mul(txs_per_key)
        .saturating_mul(2);
    {
        ui::kv(
            "per-key budget",
            &format!(
                "{:.6} ETH (gas-price {:.1} gwei × {ITS_GAS_LIMIT} × {txs_per_key} txs + {:.6} ETH msg.value × {txs_per_key}, ×2 buffer)",
                gas_extra_per_key as f64 / 1e18,
                gas_price_wei as f64 / 1e9,
                gas_value_wei as f64 / 1e18,
            ),
        );
    }
    keypairs::ensure_funded_evm_with_extra(
        &funding_provider,
        &evm_src.signer,
        &derived,
        super::units::Wei::from_u128(gas_extra_per_key),
    )
    .await?;
    Ok(derived)
}

/// Distribute the interchain token to derived signers and pre-approve the
/// ITS for each key.
///
/// XRPL canonical XRP wraps to a lock/unlock-managed ERC20 on the EVM
/// side, so ITS does `transferFrom(sender, token_manager, amount)` which
/// requires an allowance from the user to the **token manager** (not the
/// ITS proxy). Pre-approve the token manager from each derived key before
/// the burst — without this the ITS reverts with
/// `TakeTokenFailed(bytes)` (selector 0x1a59c9bd).
async fn distribute_and_approve_tokens(
    evm_src: &EvmSource,
    rpc_urls: &[String],
    token_addr: Address,
    token_id: FixedBytes<32>,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
) -> eyre::Result<()> {
    let amount_per_tx = U256::from(AMOUNT_PER_TX_WEI);
    let amount_per_key = if sizing.is_burst() {
        amount_per_tx
    } else {
        let txs_per_key = sizing.transactions_per_key() + 1;
        amount_per_tx * U256::from(txs_per_key)
    };
    let endpoints = EvmEndpoints::connect(rpc_urls)?;
    super::its_evm_source::distribute_tokens(
        &endpoints,
        &evm_src.signer,
        token_addr,
        derived,
        amount_per_key,
    )
    .await?;

    super::its_evm_source::approve_its_for_keys(
        rpc_urls,
        token_addr,
        evm_src.its_proxy_addr,
        token_id,
        derived,
        amount_per_key,
    )
    .await?;
    Ok(())
}

/// Drive the sustained-mode pipeline: spawn the streaming verifier, run the
/// EVM sustained loop, stitch amplifier timings back into the report, and
/// hand off to `finish_report`.
struct SustainedPipeline {
    args: LoadTestArgs,
    cfg: ChainsConfig,
    source_rpc_urls: Vec<String>,
    xrpl: XrplDest,
    derived: Vec<PrivateKeySigner>,
    its_ctx: ItsCallCtx,
    sizing: RunSizing,
    plan: SustainedPlan,
}

async fn run_sustained_pipeline(pipeline: SustainedPipeline) -> eyre::Result<()> {
    let SustainedPipeline {
        args,
        cfg,
        source_rpc_urls,
        xrpl,
        derived,
        its_ctx,
        sizing,
        plan,
    } = pipeline;
    let SustainedPlan {
        tps,
        duration_secs,
        key_cycle,
    } = plan;

    let nonce_endpoints = EvmEndpoints::connect(&source_rpc_urls)?;
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for s in &derived {
        let address = s.address();
        let n = retry_with_fallback_all(
            "starting nonce",
            nonce_endpoints.providers(),
            |p| async move { p.get_transaction_count(address).await },
        )
        .await?;
        nonces.push(n);
    }

    let has_voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, &args.source_axelar_id)
        .is_ok();
    let submitter = super::its_evm_source::ItsEvmSubmitter {
        rpc_urls: source_rpc_urls,
        its_proxy: its_ctx.its_proxy_addr,
        token_id: its_ctx.token_id.into(),
        destination_chain: args.destination_axelar_id.to_string(),
        receiver: its_ctx.receiver_bytes.clone(),
        amount: its_ctx.amount_per_tx,
        gas_value: super::units::Wei::from_u256(its_ctx.gas_value),
        gas_arg_scaling_factor: its_ctx.gas_arg_scaling_factor,
    };
    its_verification::run_sustained(
        &args,
        XrplItsTarget {
            rpc_url: xrpl.xrpl_rpc.clone(),
            recipient: xrpl.recipient_addr.clone(),
        },
        &format!("[0/{duration_secs}s] starting sustained ITS send..."),
        &xrpl.recipient_addr,
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

/// Drive the burst-mode pipeline: fan out parallel `interchainTransfer`
/// calls with retry-on-429, build the report, batch-verify on XRPL, and
/// hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    source_rpc_urls: &[String],
    xrpl: &XrplDest,
    derived: &[PrivateKeySigner],
    its_ctx: &ItsCallCtx,
    sizing: &RunSizing,
) -> eyre::Result<()> {
    let num_keys = sizing.num_keys;

    let test_start = Instant::now();
    let burst = super::its_evm_source::run_its_burst(
        super::its_evm_source::ItsEvmSubmitter {
            rpc_urls: source_rpc_urls.to_vec(),
            its_proxy: its_ctx.its_proxy_addr,
            token_id: its_ctx.token_id.into(),
            destination_chain: args.destination_axelar_id.to_string(),
            receiver: its_ctx.receiver_bytes.clone(),
            amount: its_ctx.amount_per_tx,
            gas_value: super::units::Wei::from_u256(its_ctx.gas_value),
            gas_arg_scaling_factor: its_ctx.gas_arg_scaling_factor,
        },
        derived,
    )
    .await?;
    finish_burst(
        args,
        XrplItsTarget {
            rpc_url: xrpl.xrpl_rpc.clone(),
            recipient: xrpl.recipient_addr.clone(),
        },
        burst,
        ItsBurstReport {
            destination_address: xrpl.recipient_addr.clone(),
            num_txs: sizing.total_expected,
            num_keys,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        test_start,
    )
    .await
}

/// Query the `XrplGateway/{xrpl_axelar_id}` contract for the canonical XRP
/// token id via the `XrpTokenId` view. Matches the TS `xrpl-token-id.js`
/// reference. Returns the raw 32 bytes.
async fn fetch_xrp_token_id(cfg: &ChainsConfig, xrpl_axelar_id: &str) -> eyre::Result<[u8; 32]> {
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let xrpl_gateway = cfg
        .axelar
        .contract_address(AxelarChainContract::XrplGateway, xrpl_axelar_id)
        .map_err(|e| {
            eyre!(
                "no XrplGateway/{xrpl_axelar_id} address in config — required to auto-discover \
                 the canonical XRP token id. Pass --token-id <hex> explicitly to skip this lookup. \
                 ({e})"
            )
        })?;
    // `cw_serde` serializes unit enum variants as a plain JSON string (NOT
    // `{"variant": {}}`), so the smart-query body for `XrpTokenId` is just
    // the JSON string `"xrp_token_id"`.
    let q = serde_json::Value::String("xrp_token_id".to_string());
    let resp = lcd_cosmwasm_smart_query(&lcd, xrpl_gateway, &q).await?;
    let s = resp
        .as_str()
        .ok_or_else(|| eyre!("XrpTokenId response was not a string: {resp}"))?;
    super::helpers::parse_token_id_hex(s, "XrpTokenId")
}
