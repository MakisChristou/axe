//! XRPL -> any EVM ITS load test.
//!
//! Scope: transfers native XRP from XRPL to an EVM destination (today this is
//! XRPL-EVM, which already has the canonical XRP interchain token registered
//! against the Axelar gateway). The sender side is pure XRPL `Payment` with
//! the standard `interchain_transfer` memos; verification reuses the existing
//! EVM destination checker.

use std::time::Instant;

use eyre::{Result, eyre};
use xrpl_types::AccountId;

use super::its_prerequisites::{self, GatewayRequirement};
use super::its_verification;
use super::its_verification::{EvmItsTarget, finish_batch};
use super::metrics::RunIdentity;
use super::run_sizing::{RunMode, RunSizing, SustainedPlan};
use super::units::Drops;
use super::{LoadTestArgs, validate_evm_rpc, xrpl_sender};
use crate::config::ChainsConfig;
use crate::ui;
use crate::xrpl::{
    XrplClient, XrplWallet, account_id_to_hex, faucet_url_for_network, parse_address,
};

pub async fn run(args: LoadTestArgs, _run_start: Instant, sizing: RunSizing) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    validate_evm_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    its_prerequisites::verify(&cfg, dest, GatewayRequirement::AmplifierOnly)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (XRP interchain_transfer via hub)");

    let (xrpl_rpc, xrpl_multisig_addr, xrpl_network_type) =
        read_xrpl_chain_config(&args.config, src)?;
    ui::address("XRPL multisig", &xrpl_multisig_addr);

    let (xrpl_client, main_wallet) =
        init_xrpl_client_and_main_wallet(&xrpl_rpc, args.private_key.as_deref()).await?;

    let evm_targets = resolve_evm_targets(&cfg, dest)?;

    let gas_fee = parse_gas_fee_drops(args.gas_value.as_deref())?;
    let wallets = fund_ephemeral_wallets(
        &xrpl_client,
        &main_wallet,
        &xrpl_rpc,
        &xrpl_network_type,
        &sizing,
        gas_fee,
    )
    .await?;
    let multisig = parse_address(&xrpl_multisig_addr)?;

    match sizing.mode() {
        RunMode::Sustained(plan) => {
            run_sustained_pipeline(SustainedPipeline {
                args,
                xrpl_client,
                wallets,
                multisig,
                evm: evm_targets,
                gas_fee,
                sizing,
                plan,
            })
            .await
        }
        RunMode::Burst { .. } => {
            run_burst_pipeline(
                &args,
                &xrpl_client,
                &wallets,
                &multisig,
                &evm_targets,
                gas_fee,
            )
            .await
        }
    }
}

/// EVM-side addresses resolved from config for the destination chain.
struct EvmTargets {
    its_proxy_addr: alloy::primitives::Address,
    dest_address_hex: String,
    evm_gateway_addr: alloy::primitives::Address,
    axelarnet_gw_addr: String,
}

/// Build the XRPL HTTP client and load the main funding wallet, logging the
/// wallet's address and current balance to the UI.
async fn init_xrpl_client_and_main_wallet(
    xrpl_rpc: &str,
    fallback_private_key: Option<&str>,
) -> Result<(XrplClient, XrplWallet)> {
    let main_wallet = load_xrpl_main_wallet(fallback_private_key)?;
    let xrpl_client = XrplClient::new(xrpl_rpc);
    let main_info = xrpl_client.account_info(&main_wallet.address()).await?;
    let main_balance_drops = main_info.map(|i| i.balance_drops).unwrap_or(0);
    ui::kv(
        "XRPL wallet",
        &format!(
            "{} ({:.4} XRP)",
            main_wallet.address(),
            main_balance_drops as f64 / 1_000_000.0
        ),
    );
    Ok((xrpl_client, main_wallet))
}

/// Resolve the EVM-destination addresses (ITS proxy, gateway, axelarnet
/// gateway) from config and emit the matching UI lines.
fn resolve_evm_targets(cfg: &ChainsConfig, dest: &str) -> Result<EvmTargets> {
    let dest_cfg = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre!("destination chain '{dest}' not found in config"))?;
    let its_proxy_addr: alloy::primitives::Address = dest_cfg
        .contract_address("InterchainTokenService", dest)?
        .parse()?;
    ui::address("destination ITS", &format!("{its_proxy_addr}"));
    // For XRPL → EVM interchain_transfer, the destination_address memo carries
    // the hex-encoded destination bytes (the ITS proxy on the EVM side).
    let dest_address_hex = format!("{its_proxy_addr:x}")
        .trim_start_matches("0x")
        .to_string();

    let evm_gateway_addr: alloy::primitives::Address =
        dest_cfg.contract_address("AxelarGateway", dest)?.parse()?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    let axelarnet_gw_addr = cfg
        .axelar
        .global_contract_address("AxelarnetGateway")?
        .to_string();

    Ok(EvmTargets {
        its_proxy_addr,
        dest_address_hex,
        evm_gateway_addr,
        axelarnet_gw_addr,
    })
}

/// Parse the user-supplied gas fee (XRP drops), defaulting to
/// `xrpl_sender::DEFAULT_GAS_FEE_DROPS`, and emit the matching UI line.
/// ITS routes via the hub (two commands: source→hub, hub→destination), so
/// we pay 2× the per-command gas value.
fn parse_gas_fee_drops(gas_value: Option<&str>) -> Result<Drops> {
    let gas_fee_drops = match gas_value {
        Some(v) => v
            .parse::<u64>()
            .map_err(|e| eyre!("invalid --gas-value: {e}"))?,
        None => xrpl_sender::DEFAULT_GAS_FEE_DROPS,
    }
    .saturating_mul(2);
    ui::kv(
        "gas fee",
        &format!(
            "{gas_fee_drops} drops ({:.4} XRP)",
            gas_fee_drops as f64 / 1_000_000.0
        ),
    );
    Ok(Drops::new(gas_fee_drops))
}

/// Derive ephemeral wallets and ensure each one is funded for the planned
/// number of transfers.
async fn fund_ephemeral_wallets(
    xrpl_client: &XrplClient,
    main_wallet: &XrplWallet,
    xrpl_rpc: &str,
    xrpl_network_type: &str,
    sizing: &RunSizing,
    gas_fee: Drops,
) -> Result<Vec<XrplWallet>> {
    // Each wallet needs: base reserve (~10 XRP) + txs_per_key * (gas + net transfer + base fee).
    // The on-wire payment is `gas_fee_drops + NET_TRANSFER_DROPS` (relayer subtracts
    // `gas_fee_drops` and forwards the remainder); +100 covers the XRPL base txn fee.
    let per_wallet_drops: u64 = 10_000_000u64
        + sizing
            .transactions_per_key()
            .saturating_mul(gas_fee.get() + xrpl_sender::NET_TRANSFER_DROPS + 100);

    // Pass RPC URL so devnet vs testnet vs mainnet is inferred from the
    // actual endpoint (devnet-amplifier mislabels its xrpl networkType).
    let faucet_url =
        faucet_url_for_network(xrpl_rpc).or_else(|| faucet_url_for_network(xrpl_network_type));
    let main_seed = main_wallet.secret_key.serialize();
    xrpl_sender::prepare_wallets(
        xrpl_client,
        &main_seed,
        Some(main_wallet),
        sizing.num_keys,
        per_wallet_drops,
        faucet_url,
    )
    .await
}

/// Drive the sustained-mode pipeline: spawn the streaming verifier, run the
/// XRPL sustained sender, stitch amplifier timings back into the report, and
/// hand off to `finish_report`.
struct SustainedPipeline {
    args: LoadTestArgs,
    xrpl_client: XrplClient,
    wallets: Vec<XrplWallet>,
    multisig: AccountId,
    evm: EvmTargets,
    gas_fee: Drops,
    sizing: RunSizing,
    plan: SustainedPlan,
}

async fn run_sustained_pipeline(pipeline: SustainedPipeline) -> Result<()> {
    let SustainedPipeline {
        args,
        xrpl_client,
        wallets,
        multisig,
        evm,
        gas_fee,
        sizing,
        plan,
    } = pipeline;
    let SustainedPlan {
        tps: tps_n,
        duration_secs,
        key_cycle,
    } = plan;

    // XRPL source has a separate VotingVerifier (XrplVotingVerifier). The
    // existing streaming verifier uses `VotingVerifier/{source}` — which
    // doesn't exist for XRPL — so we run without a voting check and let
    // the Routed → HubApproved → ... stages drive the pipeline.
    let has_voting_verifier = false;
    let destination_chain = args.destination_chain.to_string();
    let destination_address = evm.its_proxy_addr.to_string();
    its_verification::run_sustained(
        &args,
        EvmItsTarget {
            gateway_addr: evm.evm_gateway_addr,
            rpc_url: args.destination_rpc.to_string(),
        },
        &format!("[0/{duration_secs}s] starting sustained XRPL ITS send..."),
        &destination_address,
        tps_n as u64 * duration_secs,
        sizing.num_keys,
        |context| {
            xrpl_sender::run_sustained(xrpl_sender::SustainedRequest {
                client: xrpl_client,
                wallets,
                destination_multisig: multisig,
                destination_chain,
                destination_address_hex: evm.dest_address_hex.clone(),
                gas_fee,
                gmp_dest_chain: "axelar".to_string(),
                gmp_dest_address: evm.axelarnet_gw_addr.clone(),
                tps: tps_n,
                duration_secs,
                key_cycle,
                verify_tx: Some(context.verify_tx),
                send_done: Some(context.send_done),
                spinner: context.spinner,
                has_voting_verifier,
            })
        },
    )
    .await
}

/// Drive the burst-mode pipeline: fan out the XRPL transfers, batch-verify on
/// the EVM destination, and hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    xrpl_client: &XrplClient,
    wallets: &[XrplWallet],
    multisig: &AccountId,
    evm: &EvmTargets,
    gas_fee: Drops,
) -> Result<()> {
    let dest = &args.destination_chain;

    let test_start = Instant::now();
    let mut report = xrpl_sender::run_burst(xrpl_sender::BurstRequest {
        client: xrpl_client.clone(),
        wallets: wallets.to_vec(),
        destination_multisig: *multisig,
        destination_chain: dest.to_string(),
        destination_address_hex: evm.dest_address_hex.clone(),
        gas_fee,
        gmp_dest_chain: "axelar".to_string(),
        gmp_dest_address: evm.axelarnet_gw_addr.clone(),
        run: RunIdentity::burst(args),
    })
    .await?;
    report.destination_address = format!("{}", evm.its_proxy_addr);

    // Keep the shared XRPL encoding helper available to future route variants.
    let _ = account_id_to_hex;

    finish_batch(
        args,
        EvmItsTarget {
            gateway_addr: evm.evm_gateway_addr,
            rpc_url: args.destination_rpc.to_string(),
        },
        &mut report,
        test_start,
    )
    .await
}

/// Read `(rpc, multisig_address, network_type)` for an XRPL chain from config.
pub(super) fn read_xrpl_chain_config(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<(String, String, String)> {
    let config = ChainsConfig::load(config)?;
    let chain = config.chain(chain_id)?;
    // Prefer `rpc` (HTTP JSON-RPC); fall back to `wssRpc` if only WS is present.
    let rpc = chain
        .rpc
        .as_ref()
        .or(chain.wss_rpc.as_ref())
        .ok_or_else(|| eyre!("no rpc for XRPL chain '{chain_id}'"))?
        .clone();
    let multisig = chain
        .contracts
        .as_ref()
        .and_then(|contracts| {
            contracts
                .get("InterchainTokenService")
                .or_else(|| contracts.get("AxelarGateway"))
        })
        .and_then(|contract| contract.address.as_ref())
        .ok_or_else(|| eyre!("no InterchainTokenService/AxelarGateway address for '{chain_id}'"))?
        .clone();
    let network_type = chain
        .network_type
        .clone()
        .unwrap_or_else(|| "testnet".to_string());
    Ok((rpc, multisig, network_type))
}

/// Load the XRPL main wallet for the SOURCE side of an XRPL → EVM transfer.
///
/// Resolution order:
/// 1. `XRPL_PRIVATE_KEY` env (preferred — supports both 32-byte hex and the
///    canonical XRPL family seed `s...` format).
/// 2. `--private-key` / `EVM_PRIVATE_KEY` interpreted as a 32-byte secp256k1
///    seed (legacy fallback so existing testnet flows still work).
fn load_xrpl_main_wallet(fallback_private_key: Option<&str>) -> Result<XrplWallet> {
    if let Ok(key) = std::env::var("XRPL_PRIVATE_KEY") {
        return XrplWallet::from_secret_str(&key)
            .map_err(|e| eyre!("XRPL_PRIVATE_KEY parse failed: {e}"));
    }
    if let Some(k) = fallback_private_key {
        return XrplWallet::from_hex(k).map_err(|e| {
            eyre!(
                "no XRPL_PRIVATE_KEY set; tried interpreting --private-key as a 32-byte hex \
                 secp256k1 seed but: {e}. Set XRPL_PRIVATE_KEY (s-prefix family seed or 64-char hex) \
                 to fix."
            )
        });
    }
    Err(eyre!(
        "XRPL main wallet required. Set XRPL_PRIVATE_KEY (s-prefix family seed or 64-char hex)."
    ))
}
