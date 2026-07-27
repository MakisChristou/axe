//! XRPL -> any EVM ITS load test.
//!
//! Scope: transfers native XRP from XRPL to an EVM destination (today this is
//! XRPL-EVM, which already has the canonical XRP interchain token registered
//! against the Axelar gateway). The sender side is pure XRPL `Payment` with
//! the standard `interchain_transfer` memos; verification reuses the existing
//! EVM destination checker.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use eyre::{Result, eyre};
use xrpl_types::AccountId;

use super::{LoadTestArgs, finish_report, validate_evm_rpc, xrpl_sender};
use crate::config::ChainsConfig;
use crate::ui;
use crate::xrpl::{
    XrplClient, XrplWallet, account_id_to_hex, faucet_url_for_network, parse_address,
};

pub async fn run(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    validate_evm_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;
    verify_axelar_prerequisites(&cfg, dest)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "ITS (XRP interchain_transfer via hub)");

    let (xrpl_rpc, xrpl_multisig_addr, xrpl_network_type) =
        read_xrpl_chain_config(&args.config, src)?;
    ui::address("XRPL multisig", &xrpl_multisig_addr);

    let (xrpl_client, main_wallet) =
        init_xrpl_client_and_main_wallet(&xrpl_rpc, args.private_key.as_deref()).await?;

    let evm_targets = resolve_evm_targets(&cfg, dest)?;

    let gas_fee_drops = parse_gas_fee_drops(args.gas_value.as_deref())?;
    let sizing = compute_run_sizing(&args);
    let wallets = fund_ephemeral_wallets(
        &xrpl_client,
        &main_wallet,
        &xrpl_rpc,
        &xrpl_network_type,
        &sizing,
        gas_fee_drops,
    )
    .await?;
    let multisig = parse_address(&xrpl_multisig_addr)?;

    if !sizing.burst_mode {
        run_sustained_pipeline(
            &args,
            &xrpl_client,
            wallets,
            multisig,
            &evm_targets,
            gas_fee_drops,
            &sizing,
        )
        .await
    } else {
        run_burst_pipeline(
            &args,
            &xrpl_client,
            &wallets,
            &multisig,
            &evm_targets,
            gas_fee_drops,
        )
        .await
    }
}

/// EVM-side addresses resolved from config for the destination chain.
struct EvmTargets {
    its_proxy_addr: alloy::primitives::Address,
    dest_address_hex: String,
    evm_gateway_addr: alloy::primitives::Address,
    axelarnet_gw_addr: String,
}

/// Sizing parameters derived from CLI flags: chooses burst vs sustained,
/// number of ephemeral wallets, and per-wallet tx count.
struct RunSizing {
    burst_mode: bool,
    sustained_params: Option<(u64, u64)>,
    num_keys: usize,
    txs_per_key: u64,
}

/// Verify Axelar-side prerequisites (cosmos Gateway for `dest`, global
/// AxelarnetGateway). Bails with the existing error strings if either is
/// missing.
fn verify_axelar_prerequisites(cfg: &ChainsConfig, dest: &str) -> Result<()> {
    // A legacy (consensus) destination has no Cosmos Gateway and is verified on
    // its on-chain gateway instead; only amplifier dests need the Cosmos Gateway.
    let dest_amplifier = cfg.axelar.contract_address("VotingVerifier", dest).is_ok();
    if dest_amplifier && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
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
fn parse_gas_fee_drops(gas_value: Option<&str>) -> Result<u64> {
    let gas_fee_drops: u64 = match gas_value {
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
    Ok(gas_fee_drops)
}

/// Decide burst vs sustained, ephemeral wallet count, and per-wallet tx count.
fn compute_run_sizing(args: &LoadTestArgs) -> RunSizing {
    let sustained_params = args.tps.zip(args.duration_secs);
    let burst_mode = sustained_params.is_none();
    let (num_keys, _total_expected) = if burst_mode {
        let n = args.num_txs.max(1) as usize;
        (n, args.num_txs.max(1))
    } else {
        let (tps, dur) = sustained_params.expect("burst_mode is false");
        let tps = tps as usize;
        (tps * args.key_cycle as usize, tps as u64 * dur)
    };

    let txs_per_key = if burst_mode {
        1u64
    } else {
        sustained_params
            .expect("burst_mode is false")
            .1
            .div_ceil(args.key_cycle)
    };

    RunSizing {
        burst_mode,
        sustained_params,
        num_keys,
        txs_per_key,
    }
}

/// Derive ephemeral wallets and ensure each one is funded for the planned
/// number of transfers.
async fn fund_ephemeral_wallets(
    xrpl_client: &XrplClient,
    main_wallet: &XrplWallet,
    xrpl_rpc: &str,
    xrpl_network_type: &str,
    sizing: &RunSizing,
    gas_fee_drops: u64,
) -> Result<Vec<XrplWallet>> {
    // Each wallet needs: base reserve (~10 XRP) + txs_per_key * (gas + net transfer + base fee).
    // The on-wire payment is `gas_fee_drops + NET_TRANSFER_DROPS` (relayer subtracts
    // `gas_fee_drops` and forwards the remainder); +100 covers the XRPL base txn fee.
    let per_wallet_drops: u64 = 10_000_000u64
        + sizing
            .txs_per_key
            .saturating_mul(gas_fee_drops + xrpl_sender::NET_TRANSFER_DROPS + 100);

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
async fn run_sustained_pipeline(
    args: &LoadTestArgs,
    xrpl_client: &XrplClient,
    wallets: Vec<XrplWallet>,
    multisig: AccountId,
    evm: &EvmTargets,
    gas_fee_drops: u64,
    sizing: &RunSizing,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let tps_n = sizing.sustained_params.expect("burst_mode is false").0 as usize;
    let duration_secs = sizing.sustained_params.expect("burst_mode is false").1;
    let key_cycle = args.key_cycle as usize;

    let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
    let send_done = Arc::new(AtomicBool::new(false));
    let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

    let vconfig = args.config.clone();
    let vsource = args.source_axelar_id.clone();
    let vdest = args.destination_axelar_id.clone();
    let vdest_rpc = args.destination_rpc.clone();
    let vdone = Arc::clone(&send_done);
    let vgw = evm.evm_gateway_addr;
    let vnetwork = args.network;
    let verify_handle = tokio::spawn(async move {
        let spinner = spinner_rx.await.expect("spinner channel dropped");
        super::verify::verify_onchain_evm_its_streaming(
            &vconfig, &vsource, &vdest, vnetwork, vgw, &vdest_rpc, verify_rx, vdone, spinner,
        )
        .await
    });

    let spinner = ui::wait_spinner(&format!(
        "[0/{duration_secs}s] starting sustained XRPL ITS send..."
    ));
    let _ = spinner_tx.send(spinner.clone());

    let test_start = Instant::now();

    // XRPL source has a separate VotingVerifier (XrplVotingVerifier). The
    // existing streaming verifier uses `VotingVerifier/{source}` — which
    // doesn't exist for XRPL — so we run without a voting check and let
    // the Routed → HubApproved → ... stages drive the pipeline.
    let has_voting_verifier = false;

    let result = xrpl_sender::run_sustained(xrpl_sender::SustainedRequest {
        client: xrpl_client.clone(),
        wallets,
        destination_multisig: multisig,
        destination_chain: dest.clone(),
        destination_address_hex: evm.dest_address_hex.clone(),
        gas_fee_drops,
        gmp_dest_chain: "axelar".to_string(),
        gmp_dest_address: evm.axelarnet_gw_addr.clone(),
        tps: tps_n,
        duration_secs,
        key_cycle,
        verify_tx: Some(verify_tx),
        send_done: Some(send_done),
        spinner,
        has_voting_verifier,
    })
    .await;

    let mut report = super::sustained::build_sustained_report(
        result,
        src,
        dest,
        &format!("{}", evm.its_proxy_addr),
        tps_n as u64 * duration_secs,
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

/// Drive the burst-mode pipeline: fan out the XRPL transfers, batch-verify on
/// the EVM destination, and hand off to `finish_report`.
async fn run_burst_pipeline(
    args: &LoadTestArgs,
    xrpl_client: &XrplClient,
    wallets: &[XrplWallet],
    multisig: &AccountId,
    evm: &EvmTargets,
    gas_fee_drops: u64,
) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let test_start = Instant::now();
    let mut report = xrpl_sender::run_burst(xrpl_sender::BurstRequest {
        client: xrpl_client,
        wallets,
        destination_multisig: multisig,
        destination_chain: dest,
        destination_address_hex: &evm.dest_address_hex,
        gas_fee_drops,
        gmp_dest_chain: "axelar",
        gmp_dest_address: &evm.axelarnet_gw_addr,
        source_chain: src,
        destination_chain_label: dest,
    })
    .await?;
    report.destination_address = format!("{}", evm.its_proxy_addr);

    // Reuse the existing EVM-destination ITS verifier on the batch of confirmed txs.
    let verification = super::verify::verify_onchain_evm_its(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        args.network,
        &format!("{}", evm.its_proxy_addr),
        evm.evm_gateway_addr,
        &args.destination_rpc,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    // Suppress unused warnings while the remaining hooks take shape.
    let _ = account_id_to_hex;

    finish_report(args, &mut report, test_start)
}

/// Read `(rpc, multisig_address, network_type)` for an XRPL chain from config.
pub(super) fn read_xrpl_chain_config(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<(String, String, String)> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    let chain = root
        .pointer(&format!("/chains/{chain_id}"))
        .ok_or_else(|| eyre!("chain '{chain_id}' not found in config"))?;
    // Prefer `rpc` (HTTP JSON-RPC); fall back to `wssRpc` if only WS is present.
    let rpc = chain
        .get("rpc")
        .and_then(|v| v.as_str())
        .or_else(|| chain.get("wssRpc").and_then(|v| v.as_str()))
        .ok_or_else(|| eyre!("no rpc for XRPL chain '{chain_id}'"))?
        .to_string();
    let multisig = chain
        .pointer("/contracts/InterchainTokenService/address")
        .or_else(|| chain.pointer("/contracts/AxelarGateway/address"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("no InterchainTokenService/AxelarGateway address for '{chain_id}'"))?
        .to_string();
    let network_type = chain
        .get("networkType")
        .and_then(|v| v.as_str())
        .unwrap_or("testnet")
        .to_string();
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
