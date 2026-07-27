//! EVM -> XRPL ITS load test.
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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::{
    primitives::{Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::eyre;
use futures::future::join_all;
use tokio::sync::{Mutex, Semaphore};

use super::keypairs;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::{LoadTestArgs, check_evm_balance, finish_report, validate_evm_rpc};
use crate::config::ChainsConfig;
use crate::cosmos::lcd_cosmwasm_smart_query;
use crate::evm::InterchainTokenService;
use crate::ui;
use crate::xrpl::{XrplClient, faucet_url_for_network, parse_address};

/// Hard-coded default XRPL recipient.
///
/// Pinned per-network so the receiver is stable across runs and does not
/// depend on whichever signing key happens to be loaded. The sender (and
/// signer that pays for the EVM-side tx) is still derived live from
/// EVM_PRIVATE_KEY.
fn default_xrpl_recipient(network: crate::types::Network) -> &'static str {
    match network {
        crate::types::Network::Mainnet => "rhnu1DRT9AmPmz9C78WoAiEyXFdaGvxgfk",
        _ => "r3Xqy7SVtkNQyCU9TZx46BAHFMhcJRopQh",
    }
}

/// Default gas value: tries the Axelarscan `estimateGasFee` quote for the
/// route (× 1.5); falls back to a route-agnostic constant when the API
/// can't be reached.
async fn default_gas_value_wei(args: &LoadTestArgs) -> u128 {
    if let Some(quoted) = quote_route_gas(args).await {
        return quoted;
    }
    fallback_gas_value_wei(args.network, &args.source_chain)
}

async fn quote_route_gas(args: &LoadTestArgs) -> Option<u128> {
    let cfg = crate::config::ChainsConfig::load(&args.config).ok()?;
    let symbol = cfg
        .chains
        .get(&args.source_chain)?
        .token_symbol
        .as_deref()?;
    super::gas_estimate::estimate_route_gas(
        args.network,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        symbol,
        super::gas_estimate::DEFAULT_DEST_GAS_LIMIT,
    )
    .await
}

fn fallback_gas_value_wei(network: crate::types::Network, _source_chain: &str) -> u128 {
    match network {
        crate::types::Network::DevnetAmplifier => 0,
        _ => 10_000_000_000_000_000,
    }
}

const MAX_CONCURRENT_SENDS: usize = 100;
const MAX_RETRIES: u32 = 5;

/// Per-tx transfer amount (in token's smallest unit). Kept tiny so a
/// 3000-tx run costs minimal real funds on mainnet. axlXRP on EVM has 18
/// decimals; ITS truncates to 6 decimals on the XRPL side, so we use
/// 0.001 axlXRP = 1e15 wei → 1000 drops on XRPL.
const AMOUNT_PER_TX_WEI: u128 = 1_000_000_000_000_000;

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    verify_axelar_prerequisites(&cfg, dest, &args.destination_axelar_id)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (interchainTransfer via hub)");

    let token_id =
        resolve_token_id(&cfg, args.token_id.as_deref(), &args.destination_axelar_id).await?;

    let evm_src = init_evm_source(&cfg, src, &evm_rpc_url, args.private_key.as_deref()).await?;

    let token_addr = verify_token_on_its(&evm_src, &evm_rpc_url, token_id).await?;

    let xrpl = setup_xrpl_recipient(&args.config, dest, args.network).await?;

    let (gas_value_wei, gas_value) = parse_gas_value_wei(&args).await?;

    let sizing = compute_run_sizing(&args);

    let derived =
        derive_and_fund_evm_signers(&evm_src, &evm_rpc_url, gas_value_wei, &args, &sizing).await?;

    distribute_and_approve_tokens(
        &evm_src,
        &evm_rpc_url,
        token_addr,
        token_id,
        &derived,
        &sizing,
        &args,
    )
    .await?;

    // --- destination_address bytes for `interchainTransfer` ---
    // For XRPL destinations, ITS expects `asciiToBytes(r-address)` in the
    // destination_address arg. The relayer parses the bytes back to a
    // string and decodes the recipient AccountId.
    let receiver_bytes = Bytes::from(xrpl.recipient_addr.as_bytes().to_vec());

    let gas_arg_scaling_factor =
        super::its_evm_source::read_gas_arg_scaling_factor(&args.config, &args.source_axelar_id);

    let its_ctx = ItsCallCtx {
        its_proxy_addr: evm_src.its_proxy_addr,
        token_id,
        gas_value,
        gas_arg_scaling_factor,
        receiver_bytes,
        amount_per_tx: U256::from(AMOUNT_PER_TX_WEI),
    };

    if !sizing.burst_mode {
        run_sustained_pipeline(&args, &cfg, &evm_rpc_url, &xrpl, derived, &its_ctx, &sizing).await
    } else {
        run_burst_pipeline(&args, &evm_rpc_url, &xrpl, &derived, &its_ctx, &sizing).await
    }
}

/// EVM source-side context: signer, raw key bytes for derivation, and the
/// resolved ITS proxy on the source chain.
struct EvmSource {
    signer: PrivateKeySigner,
    main_key: [u8; 32],
    its_proxy_addr: alloy::primitives::Address,
}

/// XRPL destination-side context: RPC URL and the recipient r-address that
/// all transfers target.
struct XrplDest {
    xrpl_rpc: String,
    recipient_addr: String,
}

/// Sizing parameters derived from CLI flags: chooses burst vs sustained,
/// number of ephemeral keys, expected total tx count, and per-key tx counts.
struct RunSizing {
    burst_mode: bool,
    sustained_params: Option<(u64, u64)>,
    num_keys: usize,
    total_expected: u64,
}

/// Per-call inputs for `interchainTransfer`: ITS proxy, token id, gas value
/// (msg.value), pre-encoded recipient bytes, and per-tx token amount.
struct ItsCallCtx {
    its_proxy_addr: alloy::primitives::Address,
    token_id: FixedBytes<32>,
    gas_value: U256,
    /// See `its_evm_source::read_gas_arg_scaling_factor` — Hedera = 10,
    /// other EVM chains = 0.
    gas_arg_scaling_factor: u32,
    receiver_bytes: Bytes,
    amount_per_tx: U256,
}

/// Verify Axelar-side prerequisites for the EVM → XRPL hop. XRPL uses
/// `XrplGateway/{xrpl_axelar_id}` rather than the standard
/// `Gateway/{chain}` — accept either. Also requires a global
/// `AxelarnetGateway`. Bails with the existing error strings if either is
/// missing.
fn verify_axelar_prerequisites(
    cfg: &ChainsConfig,
    dest: &str,
    destination_axelar_id: &str,
) -> eyre::Result<()> {
    // XRPL uses `XrplGateway/{xrpl_axelar_id}` instead of the standard
    // `Gateway/{chain}` — accept either.
    let has_dest_gateway = cfg.axelar.contract_address("Gateway", dest).is_ok()
        || cfg
            .axelar
            .contract_address("XrplGateway", destination_axelar_id)
            .is_ok();
    if !has_dest_gateway {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway (or XrplGateway) in the config — verification would fail."
        );
    }
    if cfg
        .axelar
        .global_contract_address("AxelarnetGateway")
        .is_err()
    {
        eyre::bail!("no AxelarnetGateway address in config — required for ITS load test");
    }
    Ok(())
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
    evm_rpc_url: &str,
    private_key: Option<&str>,
) -> eyre::Result<EvmSource> {
    let private_key = private_key.ok_or_else(|| {
        eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, deployer_address).await?;
    let main_key: [u8; 32] = signer.to_bytes().into();

    let its_proxy_addr: alloy::primitives::Address = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre!("source chain '{src}' not found in config"))?
        .contract_address("InterchainTokenService", src)?
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
    evm_rpc_url: &str,
    token_id: FixedBytes<32>,
) -> eyre::Result<alloy::primitives::Address> {
    let write_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    let its_service = InterchainTokenService::new(evm_src.its_proxy_addr, &write_provider);
    let token_addr = its_service
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| {
            eyre!(
                "token id 0x{} not registered on EVM ITS: {e}",
                hex::encode(token_id)
            )
        })?;
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
    config: &std::path::Path,
    dest: &str,
    network: crate::types::Network,
) -> eyre::Result<XrplDest> {
    let (xrpl_rpc, _xrpl_multisig, xrpl_network_type) =
        super::its_xrpl_to_evm::read_xrpl_chain_config(config, dest)?;
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
async fn parse_gas_value_wei(args: &LoadTestArgs) -> eyre::Result<(u128, U256)> {
    let gas_value_wei: u128 = match args.gas_value.as_deref() {
        Some(v) => v.parse().map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => default_gas_value_wei(args).await,
    };
    let gas_value = U256::from(gas_value_wei);
    {
        ui::kv(
            "gas value",
            &format!(
                "{gas_value_wei} wei ({:.6} ETH)",
                gas_value_wei as f64 / 1e18
            ),
        );
    }
    Ok((gas_value_wei, gas_value))
}

/// Decide burst vs sustained, ephemeral key count, and total expected txs.
fn compute_run_sizing(args: &LoadTestArgs) -> RunSizing {
    let sustained_params = args.tps.zip(args.duration_secs);
    let burst_mode = sustained_params.is_none();
    let (num_keys, total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let (tps, dur) = sustained_params.expect("burst_mode is false");
        let tps = tps as usize;
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };
    RunSizing {
        burst_mode,
        sustained_params,
        num_keys,
        total_expected,
    }
}

/// Derive ephemeral EVM signers and ensure each is funded for the planned
/// number of `interchainTransfer` calls (gas + msg.value × txs_per_key, ×2
/// safety multiplier).
async fn derive_and_fund_evm_signers(
    evm_src: &EvmSource,
    evm_rpc_url: &str,
    gas_value_wei: u128,
    args: &LoadTestArgs,
    sizing: &RunSizing,
) -> eyre::Result<Vec<PrivateKeySigner>> {
    let derived = keypairs::derive_evm_signers(&evm_src.main_key, sizing.num_keys)?;
    ui::info(&format!("derived {} EVM signing keys", derived.len()));
    let funding_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
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
    let txs_per_key: u128 = if sizing.burst_mode {
        1
    } else {
        let dur = sizing.sustained_params.expect("burst_mode is false").1;
        let rounds = dur.div_ceil(args.key_cycle);
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
        gas_extra_per_key,
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
    evm_rpc_url: &str,
    token_addr: alloy::primitives::Address,
    token_id: FixedBytes<32>,
    derived: &[PrivateKeySigner],
    sizing: &RunSizing,
    args: &LoadTestArgs,
) -> eyre::Result<()> {
    let amount_per_tx = U256::from(AMOUNT_PER_TX_WEI);
    let amount_per_key = if sizing.burst_mode {
        amount_per_tx
    } else {
        let txs_per_key = sizing
            .sustained_params
            .expect("burst_mode is false")
            .1
            .div_ceil(args.key_cycle)
            + 1;
        amount_per_tx * U256::from(txs_per_key)
    };
    let token_provider = ProviderBuilder::new()
        .wallet(evm_src.signer.clone())
        .connect_http(evm_rpc_url.parse()?);
    super::its_evm_source::distribute_tokens(&token_provider, token_addr, derived, amount_per_key)
        .await?;

    super::its_evm_source::approve_its_for_keys(
        evm_rpc_url,
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
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    cfg: &ChainsConfig,
    evm_rpc_url: &str,
    xrpl: &XrplDest,
    derived: Vec<PrivateKeySigner>,
    its_ctx: &ItsCallCtx,
    sizing: &RunSizing,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let tps = sizing.sustained_params.expect("burst_mode is false").0 as usize;
    let duration_secs = sizing.sustained_params.expect("burst_mode is false").1;
    let key_cycle = args.key_cycle as usize;
    let rpc_url_str = evm_rpc_url.to_string();

    let nonce_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    let mut nonces: Vec<u64> = Vec::with_capacity(sizing.num_keys);
    for s in &derived {
        let n = nonce_provider.get_transaction_count(s.address()).await?;
        nonces.push(n);
    }

    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    let has_voting_verifier = cfg
        .axelar
        .contract_address("VotingVerifier", &args.source_axelar_id)
        .is_ok();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vxrpl_rpc = xrpl.xrpl_rpc.clone();
    let vrecipient = xrpl.recipient_addr.clone();
    let vdone = std::sync::Arc::clone(&send_done);
    let vnetwork = args.network;
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_xrpl_its_streaming(
            &vconfig,
            &vsource,
            &vdest,
            vnetwork,
            &vxrpl_rpc,
            &vrecipient,
            verify_rx,
            vdone,
            spinner,
        )
        .await
    });

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained ITS send..."
    ));
    let _ = spinner_tx.send(spinner.clone());

    let test_start = Instant::now();
    let dest_chain_s = args.destination_axelar_id.clone();
    let gas_value = its_ctx.gas_value;
    let gas_arg_scaling_factor = its_ctx.gas_arg_scaling_factor;
    let receiver_bytes = its_ctx.receiver_bytes.clone();
    let amount_per_tx = its_ctx.amount_per_tx;
    let its_proxy_addr = its_ctx.its_proxy_addr;
    let token_id = its_ctx.token_id;

    let make_task: super::sustained::MakeTask =
        Box::new(move |key_idx: usize, nonce: Option<u64>| {
            let dc = dest_chain_s.clone();
            let gv = gas_value;
            let gsf = gas_arg_scaling_factor;
            let rb = receiver_bytes.clone();
            let amt = amount_per_tx;
            let its_proxy = its_proxy_addr;
            let tid = token_id;
            let url = rpc_url_str.clone();
            let vtx = verify_tx.clone();
            let has_vv = has_voting_verifier;

            let provider = ProviderBuilder::new()
                .wallet(derived[key_idx].clone())
                .connect_http(url.parse().expect("invalid RPC URL"));

            Box::pin(async move {
                let mut result = super::its_evm_source::execute_interchain_transfer(
                    super::its_evm_source::InterchainTransferRequest {
                        provider: &provider,
                        its_proxy,
                        token_id: tid,
                        destination_chain: &dc,
                        receiver: &rb,
                        amount: amt,
                        gas_value: gv,
                        gas_arg_scaling_factor: gsf,
                        explicit_nonce: nonce,
                    },
                )
                .await;
                if result.success {
                    match super::verify::tx_to_pending_its(&result, has_vv) {
                        Ok(pending) => {
                            let _ = vtx.send(pending);
                        }
                        Err(e) => {
                            result.success = false;
                            result.error = Some(format!("failed to build verification state: {e}"));
                        }
                    }
                }
                result
            })
        });

    let result = super::sustained::run_sustained_loop(
        tps,
        duration_secs,
        key_cycle,
        Some(nonces),
        make_task,
        Some(send_done),
        spinner,
    )
    .await;

    let mut report = super::sustained::build_sustained_report(
        result,
        src,
        dest,
        &xrpl.recipient_addr,
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

/// Drive the burst-mode pipeline: fan out parallel `interchainTransfer`
/// calls with retry-on-429, build the report, batch-verify on XRPL, and
/// hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    evm_rpc_url: &str,
    xrpl: &XrplDest,
    derived: &[PrivateKeySigner],
    its_ctx: &ItsCallCtx,
    sizing: &RunSizing,
) -> eyre::Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let num_keys = sizing.num_keys;

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let confirmed_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SENDS));
    let spinner = ui::wait_spinner(&format!("sending (0/{num_keys} confirmed)..."));
    let test_start = Instant::now();

    let mut tasks = Vec::with_capacity(num_keys);
    let dest_chain = args.destination_axelar_id.clone();

    for derived_signer in derived {
        let metrics_clone = Arc::clone(&metrics_list);
        let counter = Arc::clone(&confirmed_counter);
        let sem = Arc::clone(&semaphore);
        let sp = spinner.clone();
        let total = num_keys;
        let dc = dest_chain.clone();
        let gv = its_ctx.gas_value;
        let gsf = its_ctx.gas_arg_scaling_factor;
        let rb = its_ctx.receiver_bytes.clone();
        let amt = its_ctx.amount_per_tx;
        let its_proxy = its_ctx.its_proxy_addr;
        let tid = its_ctx.token_id;

        let provider = ProviderBuilder::new()
            .wallet(derived_signer.clone())
            .connect_http(evm_rpc_url.parse()?);

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let mut m = None;
            for attempt in 0..=MAX_RETRIES {
                let result = super::its_evm_source::execute_interchain_transfer(
                    super::its_evm_source::InterchainTransferRequest {
                        provider: &provider,
                        its_proxy,
                        token_id: tid,
                        destination_chain: &dc,
                        receiver: &rb,
                        amount: amt,
                        gas_value: gv,
                        gas_arg_scaling_factor: gsf,
                        explicit_nonce: None,
                    },
                )
                .await;
                if result.success || attempt == MAX_RETRIES {
                    m = Some(result);
                    break;
                }
                let is_rate_limited = result.error.as_deref().is_some_and(|e| e.contains("429"));
                if !is_rate_limited {
                    m = Some(result);
                    break;
                }
                let backoff = Duration::from_secs(1 << attempt);
                tokio::time::sleep(backoff).await;
            }
            let m = m.unwrap();
            if m.success {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                sp.set_message(format!("sending ({done}/{total} confirmed)..."));
            }
            metrics_clone.lock().await.push(m);
        });
        tasks.push(handle);
    }

    let total_submitted = tasks.len() as u64;
    join_all(tasks).await;
    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed_counter.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    let metrics = metrics_list.lock().await.clone();
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: xrpl.recipient_addr.clone(),
            num_txs: args.num_txs,
            num_keys,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    let verification = super::verify::verify_onchain_xrpl_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        args.network,
        &xrpl.xrpl_rpc,
        &xrpl.recipient_addr,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(args, &mut report, test_start)
}

/// Query the `XrplGateway/{xrpl_axelar_id}` contract for the canonical XRP
/// token id via the `XrpTokenId` view. Matches the TS `xrpl-token-id.js`
/// reference. Returns the raw 32 bytes.
async fn fetch_xrp_token_id(cfg: &ChainsConfig, xrpl_axelar_id: &str) -> eyre::Result<[u8; 32]> {
    let (lcd, _, _, _) = cfg.axelar.cosmos_tx_params()?;
    let xrpl_gateway = cfg
        .axelar
        .contract_address("XrplGateway", xrpl_axelar_id)
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
    let bytes = hex::decode(s.trim_start_matches("0x"))
        .map_err(|e| eyre!("XrpTokenId hex decode failed: {e} (got {s:?})"))?;
    if bytes.len() != 32 {
        return Err(eyre!(
            "XrpTokenId returned {} bytes, expected 32: {s}",
            bytes.len()
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}
