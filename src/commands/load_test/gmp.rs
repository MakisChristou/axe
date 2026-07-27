//! Per-pair GMP load-test orchestrators (the `run_*` functions).
//!
//! Each function drives one source × destination pair end-to-end:
//! deploy or reuse a `SenderReceiver`, fund derived signing keys, fire
//! the configured number of `callContract` txs, and then hand off to the
//! `verify` module for batch / streaming amplifier verification.
//!
//! Shape is consistent across pairs (validate RPCs → load wallet →
//! deploy/reuse SR → derive signers → distribute funds → fire → verify).
//! See the `// ===` headers for the four-quadrant layout: native pairs at
//! the top, then EVM↔Stellar, EVM↔Solana via Stellar, Sui sources, and
//! finally Sui destinations.

use std::time::Instant;

use alloy::{
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use serde_json::json;

use super::evm_sender;
use super::helpers::list_gateway_chains;
use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use super::sol_sender;
use super::stellar_sender;
use super::sustained;
use super::verify;
use super::{
    LoadTestArgs, check_evm_balance, deploy_or_reuse_sender_receiver, deploy_sender_receiver,
    ensure_evm_contract_deployed, ensure_sender_receiver, finalize_sui_dest_run, finish_report,
    load_stellar_main_wallet, load_sui_main_wallet, read_cache, read_stellar_contract_address,
    read_stellar_network_type, read_stellar_token_address, save_cache, sui_dest_lookup,
    validate_evm_rpc, validate_solana_rpc,
};
use crate::config::ChainsConfig;
use crate::ui;

pub(super) async fn run_sol_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let dest = &args.destination_chain;
    let src = &args.source_chain;

    let cfg = ChainsConfig::load(&args.config)?;

    let rpc_url = &args.destination_rpc;

    // Validate RPCs before doing any work
    validate_solana_rpc(&args.source_rpc).await?;
    validate_evm_rpc(rpc_url).await?;

    // Check that verification contracts exist for this chain pair before doing any work.
    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGateway", dest)?
        .parse()?;
    let gas_service_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGasService", dest)?
        .parse()?;

    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // Without this check, a missing/undeployed EVM gateway causes the verifier
    // to silently report 30/30 executed (eth_call on an EOA returns 0x →
    // alloy decodes it as `false` → our pipeline interprets that as "approval
    // consumed = executed"). Fail fast instead.
    ensure_evm_contract_deployed(rpc_url, "destination AxelarGateway", gateway_addr).await?;

    // --- Deploy/reuse SenderReceiver on destination EVM chain ---
    let cache = read_cache(dest);

    let (sender_receiver_addr, provider) = if let Some(addr_str) =
        cache.get("senderReceiverAddress").and_then(|v| v.as_str())
    {
        // Try to reuse cached address — only need a read-only provider for the check
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr: alloy::primitives::Address = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        // Check if code exists and gateway matches config
        let needs_redeploy = if code.is_empty() {
            ui::warn("cached SenderReceiver has no code, redeploying...");
            true
        } else {
            let sr = crate::evm::SenderReceiver::new(addr, &read_provider);
            match sr.gateway().call().await {
                Ok(onchain_gw) if onchain_gw != gateway_addr => {
                    ui::warn(&format!(
                        "cached SenderReceiver points to old gateway {onchain_gw}, expected {gateway_addr}, redeploying..."
                    ));
                    true
                }
                Err(_) => {
                    ui::warn("cached SenderReceiver gateway check failed, redeploying...");
                    true
                }
                _ => false,
            }
        };
        if needs_redeploy {
            let private_key = args.private_key.as_ref().ok_or_else(|| {
                eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
            })?;
            let signer: PrivateKeySigner = private_key.parse()?;
            check_evm_balance(&read_provider, signer.address()).await?;
            let write_provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            let new_addr =
                deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
            let mut cache = cache;
            cache["senderReceiverAddress"] = json!(format!("{new_addr}"));
            save_cache(dest, &cache)?;
            (new_addr, write_provider)
        } else {
            ui::info(&format!("SenderReceiver: reusing {addr}"));
            // Build a wallet provider against the user's key so the load test
            // can submit txs through it. We fall through to fail-loud when
            // no key is set rather than substituting a well-known low-entropy
            // key — that placeholder existed historically as a stand-in for
            // a "read-only" provider but the type signature returns a
            // wallet provider, so any tx submitted through it would have
            // been a footgun (the placeholder address has zero funds and
            // is sweepable by anyone who finds the same constant).
            let private_key = args.private_key.as_deref().ok_or_else(|| {
                eyre::eyre!(
                    "EVM private key required to use the cached SenderReceiver. \
                     Set EVM_PRIVATE_KEY env var or use --private-key"
                )
            })?;
            let signer: PrivateKeySigner = private_key.parse()?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            (addr, provider)
        }
    } else {
        let private_key = args.private_key.as_ref().ok_or_else(|| {
            eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
        })?;
        let signer: PrivateKeySigner = private_key.parse()?;
        let deployer_addr = signer.address();
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        check_evm_balance(&read_provider, deployer_addr).await?;
        ui::info("deploying SenderReceiver on destination chain...");
        let write_provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(rpc_url.parse()?);
        let addr = deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache;
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(dest, &cache)?;
        (addr, write_provider)
    };

    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Streaming verification: run concurrently with sends.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.clone();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let vgw = gateway_addr;
        let vnetwork = args.network;
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_evm_streaming(
                &vconfig,
                &vsource,
                &vdest,
                vnetwork,
                &vdest_addr,
                vgw,
                &vdest_rpc,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let mut report = sol_sender::run_sustained_load_test_with_metrics(
            &args,
            true,
            &destination_address,
            Some(verify_tx),
            Some(send_done),
            spinner_tx,
        )
        .await?;

        let (verification, timings) = verify_handle.await??;
        for (msg_id, timing) in timings {
            if let Some(tx) = report.transactions.iter_mut().find(|t| {
                t.signature == msg_id
                    || format!(
                        "{}-{}.1",
                        t.signature,
                        crate::solana::solana_call_contract_index(args.network)
                    ) == msg_id
            }) {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        report
    } else {
        let mut report =
            sol_sender::run_load_test_with_metrics(&args, &destination_address, true).await?;
        let verification = verify::verify_onchain(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            &destination_address,
            gateway_addr,
            &provider,
            &mut report.transactions,
            verify::SourceChainType::Svm,
            args.network,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_evm_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let cfg = ChainsConfig::load(&args.config)?;

    let evm_rpc_url = args.source_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&evm_rpc_url).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    // Check that verification contracts exist for this chain pair before doing any work
    if cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGateway", src)?
        .parse()?;
    let gas_service_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGasService", src)?
        .parse()?;
    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // --- Set up EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, signer_address).await?;

    let write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(evm_rpc_url.parse()?);

    // Extract 32-byte private key for deriving sub-wallets
    let main_key: [u8; 32] = signer.to_bytes().into();

    {
        let balance: u128 = read_provider.get_balance(signer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{signer_address} ({eth:.6} ETH)"));
    }

    // --- Deploy/reuse SenderReceiver on source chain ---
    let cache_key = &format!("{src}-evm-to-sol");
    let cache = read_cache(cache_key);
    let sender_receiver_addr = deploy_or_reuse_sender_receiver(
        &cache,
        cache_key,
        &read_provider,
        &write_provider,
        gateway_addr,
        gas_service_addr,
        "source",
    )
    .await?;
    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));

    // Destination on Solana: memo program (resolved from the target network)
    let destination_address = evm_sender::memo_program_id(args.network).to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Sustained mode: run verification concurrently with the send phase.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The MultiProgress + spinners are created inside the send function AFTER
        // funding completes, so they don't flicker during setup. The verify spinner
        // is sent to the verify task via a oneshot channel.
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        // Spawn verification in a background task.
        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.to_string();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let vnetwork = args.network;
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_solana_streaming(
                &vconfig,
                &vsource,
                &vdest,
                &vdest_addr,
                &vdest_rpc,
                vnetwork,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let mut report =
            evm_sender::run_sustained_load_test_with_metrics(evm_sender::SustainedLoadRequest {
                args: &args,
                sender_receiver: sender_receiver_addr,
                main_key: &main_key,
                evm_rpc_url: &evm_rpc_url,
                destination_address,
                verify_tx: Some(verify_tx),
                send_done: Some(send_done),
                verify_spinner_tx: spinner_tx,
                evm_destination: false,
            })
            .await?;

        // Wait for verification to finish.
        let (verification, timings) = verify_handle.await??;
        // Write amplifier timing back into per-tx records for JSON report & pipeline counts.
        // Timings are keyed by message_id (signature); match them to transactions.
        for (msg_id, timing) in timings {
            if let Some(tx) = report.transactions.iter_mut().find(|t| {
                t.signature == msg_id
                    || format!(
                        "{}-{}.1",
                        t.signature,
                        crate::solana::solana_call_contract_index(args.network)
                    ) == msg_id
            }) {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        report
    } else {
        let mut report = evm_sender::run_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &evm_rpc_url,
            destination_address,
            false,
        )
        .await?;

        let verification = verify::verify_onchain_solana(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            destination_address,
            &args.destination_rpc,
            &mut report.transactions,
            verify::SourceChainType::Evm,
            args.network,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_evm_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let cfg = ChainsConfig::load(&args.config)?;

    // Classify each end as legacy (consensus) or Amplifier. A legacy-touching
    // route is verified entirely on the destination gateway, so it does not
    // need the Amplifier Cosmos contracts the standard path requires.
    let (src_legacy, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    let legacy_route = src_legacy || dst_legacy;

    let source_rpc_url = args.source_rpc.clone();
    let dest_rpc_url = args.destination_rpc.clone();

    // Validate RPCs before doing any work
    validate_evm_rpc(&source_rpc_url).await?;
    validate_evm_rpc(&dest_rpc_url).await?;

    // Amplifier destinations need a Cosmos Gateway for verification; legacy
    // (consensus) destinations are verified directly on their on-chain gateway.
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    // --- Set up EVM signer ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key")
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let source_read_provider = ProviderBuilder::new().connect_http(source_rpc_url.parse()?);
    check_evm_balance(&source_read_provider, signer_address).await?;

    let source_write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(source_rpc_url.parse()?);

    let main_key: [u8; 32] = signer.to_bytes().into();

    {
        let balance: u128 = source_read_provider.get_balance(signer_address).await?.to();
        let eth = balance as f64 / 1e18;
        ui::kv("wallet", &format!("{signer_address} ({eth:.6} ETH)"));
    }

    // --- Source chain: deploy/reuse SenderReceiver (for sending) ---
    let src_gateway_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGateway", src)?
        .parse()?;
    let src_gas_service_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGasService", src)?
        .parse()?;
    ui::address("source gateway", &format!("{src_gateway_addr}"));

    let src_cache_key = &format!("{src}-evm-to-evm");
    let src_cache = read_cache(src_cache_key);
    let sender_receiver_addr = deploy_or_reuse_sender_receiver(
        &src_cache,
        src_cache_key,
        &source_read_provider,
        &source_write_provider,
        src_gateway_addr,
        src_gas_service_addr,
        "source",
    )
    .await?;
    ui::address(
        "SenderReceiver (source)",
        &format!("{sender_receiver_addr}"),
    );

    // --- Destination chain: deploy/reuse SenderReceiver (as receive target) ---
    let dest_gateway_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGateway", dest)?
        .parse()?;
    let dest_gas_service_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGasService", dest)?
        .parse()?;
    ui::address("destination gateway", &format!("{dest_gateway_addr}"));

    // Bail loudly if the configured gateway has no bytecode — otherwise the
    // verifier silently reports false-positive 30/30 executed.
    ensure_evm_contract_deployed(
        &dest_rpc_url,
        "destination AxelarGateway",
        dest_gateway_addr,
    )
    .await?;

    let dest_read_provider = ProviderBuilder::new().connect_http(dest_rpc_url.parse()?);
    let dest_write_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect_http(dest_rpc_url.parse()?);

    // Fund the signer on the destination chain if needed for SenderReceiver deployment
    let dest_balance: u128 = dest_read_provider.get_balance(signer_address).await?.to();
    if dest_balance == 0 {
        eyre::bail!(
            "EVM wallet {signer_address} has no funds on destination chain '{dest}'. \
             Fund it first."
        );
    }

    let dest_cache_key = &format!("{dest}-evm-to-evm-dest");
    let dest_cache = read_cache(dest_cache_key);
    let dest_sender_receiver = deploy_or_reuse_sender_receiver(
        &dest_cache,
        dest_cache_key,
        &dest_read_provider,
        &dest_write_provider,
        dest_gateway_addr,
        dest_gas_service_addr,
        "destination",
    )
    .await?;
    ui::address(
        "SenderReceiver (destination)",
        &format!("{dest_sender_receiver}"),
    );

    let destination_address = format!("{dest_sender_receiver}");

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Streaming verification: run concurrently with sends.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.clone();
        let vdest_rpc = dest_rpc_url.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let vgw = dest_gateway_addr;
        let vlegacy = legacy_route;
        let vnetwork = args.network;
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            if vlegacy {
                verify::verify_onchain_evm_legacy_streaming(
                    &vconfig,
                    &vsource,
                    &vdest,
                    vnetwork,
                    &vdest_addr,
                    vgw,
                    &vdest_rpc,
                    verify_rx,
                    vdone,
                    spinner,
                )
                .await
            } else {
                verify::verify_onchain_evm_streaming(
                    &vconfig,
                    &vsource,
                    &vdest,
                    vnetwork,
                    &vdest_addr,
                    vgw,
                    &vdest_rpc,
                    verify_rx,
                    vdone,
                    spinner,
                )
                .await
            }
        });

        let mut report =
            evm_sender::run_sustained_load_test_with_metrics(evm_sender::SustainedLoadRequest {
                args: &args,
                sender_receiver: sender_receiver_addr,
                main_key: &main_key,
                evm_rpc_url: &source_rpc_url,
                destination_address: &destination_address,
                verify_tx: Some(verify_tx),
                send_done: Some(send_done),
                verify_spinner_tx: spinner_tx,
                evm_destination: true,
            })
            .await?;

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
        report
    } else {
        let mut report = evm_sender::run_load_test_with_metrics(
            &args,
            sender_receiver_addr,
            &main_key,
            &source_rpc_url,
            &destination_address,
            true,
        )
        .await?;

        let verification = if legacy_route {
            verify::verify_onchain_evm_legacy(
                &args.config,
                &args.source_axelar_id,
                &args.destination_axelar_id,
                &destination_address,
                dest_gateway_addr,
                &dest_read_provider,
                &mut report.transactions,
                args.network,
                verify::SourceChainType::Evm,
            )
            .await?
        } else {
            verify::verify_onchain(
                &args.config,
                &args.source_axelar_id,
                &args.destination_axelar_id,
                &destination_address,
                dest_gateway_addr,
                &dest_read_provider,
                &mut report.transactions,
                verify::SourceChainType::Evm,
                args.network,
            )
            .await?
        };
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_sol_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    // Validate RPCs
    validate_solana_rpc(&args.source_rpc).await?;
    validate_solana_rpc(&args.destination_rpc).await?;

    let cfg = ChainsConfig::load(&args.config)?;

    // Check that verification contracts exist
    if cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — \
             verification would fail. Pick a chain that has a Gateway entry, e.g.:\n  {}",
            list_gateway_chains(&cfg).join(", ")
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);

    // Destination is the Solana memo program
    let destination_address = evm_sender::memo_program_id(args.network).to_string();
    let destination_address = destination_address.as_str();
    ui::kv("destination program", destination_address);

    let test_start = Instant::now();
    let sustained = args.tps.is_some() && args.duration_secs.is_some();

    let mut report = if sustained {
        // Sustained mode: run verification concurrently with the send phase.
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        // Spawn verification in a background task.
        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.to_string();
        let vdest_rpc = args.destination_rpc.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let vnetwork = args.network;
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_solana_streaming(
                &vconfig,
                &vsource,
                &vdest,
                &vdest_addr,
                &vdest_rpc,
                vnetwork,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let mut report = sol_sender::run_sustained_load_test_with_metrics(
            &args,
            false,
            destination_address,
            Some(verify_tx),
            Some(send_done),
            spinner_tx,
        )
        .await?;

        // Wait for verification to finish.
        let (verification, timings) = verify_handle.await??;
        for (msg_id, timing) in timings {
            if let Some(tx) = report.transactions.iter_mut().find(|t| {
                t.signature == msg_id
                    || format!(
                        "{}-{}.1",
                        t.signature,
                        crate::solana::solana_call_contract_index(args.network)
                    ) == msg_id
            }) {
                tx.amplifier_timing = Some(timing);
            }
        }
        report.verification = Some(verification);
        report
    } else {
        let mut report =
            sol_sender::run_load_test_with_metrics(&args, destination_address, false).await?;

        let verification = verify::verify_onchain_solana(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            destination_address,
            &args.destination_rpc,
            &mut report.transactions,
            verify::SourceChainType::Svm,
            args.network,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

// ---------------------------------------------------------------------------
// Stellar -> EVM (GMP)
// ---------------------------------------------------------------------------

pub(super) async fn run_stellar_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let evm_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt from this check.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (call_contract)");

    // --- Stellar source-side setup ---
    let stellar_rpc = &args.source_rpc;
    let network_type = read_stellar_network_type(&args.config, src)?;
    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, &network_type)?;
    ui::kv("network", &network_type);

    // GMP flow uses `AxelarExample.send(...)` — the high-level wrapper that
    // internally pays gas via `AxelarGasService` and emits from `AxelarGateway`.
    // Calling `AxelarGateway.call_contract` directly emits the message but
    // leaves gas unpaid, which the Axelar relayer rejects as "Insufficient Fee".
    let stellar_example = read_stellar_contract_address(&args.config, src, "AxelarExample")?;
    let stellar_gateway = read_stellar_contract_address(&args.config, src, "AxelarGateway")?;
    let stellar_gas_token = read_stellar_token_address(&args.config, src)?;
    ui::address("Stellar AxelarExample", &stellar_example);
    ui::address("Stellar AxelarGateway", &stellar_gateway);
    ui::address("Stellar XLM token", &stellar_gas_token);

    let gas_stroops: u64 = match &args.gas_value {
        Some(v) => v
            .parse()
            .map_err(|e| eyre::eyre!("invalid --gas-value: {e}"))?,
        None => stellar_sender::DEFAULT_GAS_STROOPS,
    };
    ui::kv(
        "gas",
        &format!(
            "{gas_stroops} stroops ({:.4} XLM)",
            gas_stroops as f64 / 10_000_000.0
        ),
    );

    // --- Stellar main wallet (for deriving ephemeral signers) ---
    let main_wallet = load_stellar_main_wallet(args.private_key.as_deref())?;
    let main_seed = main_wallet.signing_key.to_bytes();

    // --- EVM SenderReceiver deploy/reuse (same pattern as run_sol_to_evm) ---
    let gateway_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGateway", dest)?
        .parse()?;
    let gas_service_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGasService", dest)?
        .parse()?;
    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // Pre-flight: bail if the destination gateway has no bytecode.
    ensure_evm_contract_deployed(&evm_rpc_url, "destination AxelarGateway", gateway_addr).await?;

    let evm_private_key = args
        .private_key
        .clone()
        .or_else(|| std::env::var("EVM_PRIVATE_KEY").ok());
    let cache = read_cache(dest);
    let cached_sr = cache.get("senderReceiverAddress").and_then(|v| v.as_str());
    if cached_sr.is_none() && evm_private_key.is_none() {
        eyre::bail!(
            "no SenderReceiver cached for '{dest}' and no EVM private key available. \
             Set EVM_PRIVATE_KEY (in .env or via --private-key) so axe can deploy the destination \
             SenderReceiver on first run."
        );
    }
    let (sender_receiver_addr, provider) = ensure_sender_receiver(
        &args,
        &evm_rpc_url,
        gateway_addr,
        gas_service_addr,
        cache,
        evm_private_key.as_deref(),
    )
    .await?;
    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    // --- Burst vs sustained ---
    // Destructure the sustained-mode params once; later branches rely on
    // `Some(...)` here instead of brittle `.unwrap()` calls.
    let sustained_params = args.tps.zip(args.duration_secs);
    let num_keys = match sustained_params {
        Some((tps, _)) => tps as usize * args.key_cycle as usize,
        None => args.num_txs.max(1) as usize,
    };
    ui::info(&format!("deriving {num_keys} Stellar keys..."));
    let wallets = crate::commands::load_test::stellar_sender::derive_wallets(&main_seed, num_keys)?;
    let txs_per_key = match sustained_params {
        Some(_) => args.key_cycle,
        None => 1,
    };
    let mainnet_starting_balance =
        crate::commands::load_test::stellar_sender::mainnet_per_key_balance_stroops(
            gas_stroops,
            txs_per_key,
        );
    crate::commands::load_test::stellar_sender::ensure_funded(
        &stellar_client,
        &wallets,
        use_friendbot,
        &main_wallet,
        mainnet_starting_balance,
    )
    .await?;

    let payload_override: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };

    let test_start = Instant::now();
    let mut report = if let Some((tps, duration_secs)) = sustained_params {
        let (verify_tx, verify_rx) = tokio::sync::mpsc::unbounded_channel();
        let send_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = tokio::sync::oneshot::channel::<indicatif::ProgressBar>();

        let vconfig = args.config.clone();
        let vsource = args.source_axelar_id.clone();
        let vdest = args.destination_axelar_id.clone();
        let vdest_addr = destination_address.clone();
        let vdest_rpc = evm_rpc_url.clone();
        let vdone = std::sync::Arc::clone(&send_done);
        let vgw = gateway_addr;
        let vnetwork = args.network;
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx.await.expect("spinner channel dropped");
            verify::verify_onchain_evm_streaming(
                &vconfig,
                &vsource,
                &vdest,
                vnetwork,
                &vdest_addr,
                vgw,
                &vdest_rpc,
                verify_rx,
                vdone,
                spinner,
            )
            .await
        });

        let spinner = ui::wait_spinner(&format!(
            "[0/{duration_secs}s] starting sustained Stellar GMP send..."
        ));
        let _ = spinner_tx.send(spinner.clone());

        let has_voting_verifier = cfg
            .axelar
            .contract_address("VotingVerifier", &args.source_chain)
            .is_ok();

        let result = crate::commands::load_test::stellar_sender::run_sustained(
            crate::commands::load_test::stellar_sender::SustainedRequest {
                client: stellar_client.clone(),
                wallets,
                example_contract: stellar_example,
                gateway_contract: stellar_gateway,
                destination_chain: args.destination_axelar_id.clone(),
                destination_address: destination_address.clone(),
                payload_override,
                tps: tps as usize,
                duration_secs,
                key_cycle: args.key_cycle as usize,
                verify_tx: Some(verify_tx),
                send_done: Some(send_done),
                spinner,
                has_voting_verifier,
                destination_contract_addr: sender_receiver_addr,
                gas_token_contract: stellar_gas_token,
                gas_amount_stroops: gas_stroops,
            },
        )
        .await;

        let mut report = sustained::build_sustained_report(
            result,
            src,
            dest,
            &destination_address,
            tps * duration_secs,
            num_keys,
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
        report
    } else {
        let mut report = crate::commands::load_test::stellar_sender::run_burst(
            crate::commands::load_test::stellar_sender::BurstRequest {
                client: &stellar_client,
                wallets: &wallets,
                example_contract: stellar_example,
                gateway_contract: stellar_gateway,
                destination_chain: &args.destination_axelar_id,
                destination_address: &destination_address,
                payload_override,
                source_chain: src,
                gas_token_contract: stellar_gas_token,
                gas_amount_stroops: gas_stroops,
            },
        )
        .await?;
        let verification = verify::verify_onchain(
            &args.config,
            &args.source_axelar_id,
            &args.destination_axelar_id,
            &destination_address,
            gateway_addr,
            &provider,
            &mut report.transactions,
            verify::SourceChainType::Stellar,
            args.network,
        )
        .await?;
        report.verification = Some(verification);
        report
    };

    finish_report(&args, &mut report, test_start)
}

// ===========================================================================
// EVM <-> Stellar GMP and Solana <-> Stellar GMP
// ===========================================================================
//
// All four functions are burst-only for simplicity in the initial cut. They
// share the same skeleton: derive ephemeral source signers, fund them, send
// in parallel with a semaphore, then run the appropriate destination
// verifier.

pub(super) async fn run_evm_to_stellar(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;
    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // EVM source ITS proxy is reused for GMP — we send via the destination
    // contract address (Stellar AxelarExample). EVM emits ContractCall.
    let signer = args
        .private_key
        .as_ref()
        .ok_or_else(|| {
            eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY or use --private-key")
        })?
        .parse::<PrivateKeySigner>()?;
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, signer.address()).await?;

    let stellar_rpc = &args.destination_rpc;
    let stellar_network_type = read_stellar_network_type(&args.config, dest)?;
    let stellar_gateway_addr = read_stellar_contract_address(&args.config, dest, "AxelarGateway")?;
    let stellar_example_addr = read_stellar_contract_address(&args.config, dest, "AxelarExample")?;
    ui::address("Stellar gateway", &stellar_gateway_addr);
    ui::address("Stellar AxelarExample", &stellar_example_addr);

    // Reuse the EVM-source GMP runner (sol_sender for sol-to-X has its
    // EVM analogue in evm_sender). For sustained, we'd add streaming;
    // burst-only here.
    let evm_gateway_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGateway", src)?
        .parse()?;
    let evm_gas_service_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGasService", src)?
        .parse()?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    // Deploy/reuse SenderReceiver as the EVM-side caller (existing pattern).
    let cache = read_cache(src);
    let evm_pk = args.private_key.clone();
    let (sender_receiver_addr, _provider) = ensure_sender_receiver(
        &args,
        &evm_rpc_url,
        evm_gateway_addr,
        evm_gas_service_addr,
        cache,
        evm_pk.as_deref(),
    )
    .await?;
    ui::address("EVM SenderReceiver", &format!("{sender_receiver_addr}"));

    // The destination address for the EVM-side callContract is the Stellar
    // `AxelarExample` C-address as a UTF-8 string.
    let main_key: [u8; 32] = signer.to_bytes().into();
    let test_start = Instant::now();
    let mut report = evm_sender::run_load_test_with_metrics(
        &args,
        sender_receiver_addr,
        &main_key,
        &evm_rpc_url,
        &stellar_example_addr,
        true, // EVM-style ABI-encoded payload (Stellar AxelarExample.execute accepts raw bytes)
    )
    .await?;

    let signer_pk: [u8; 32] = alloy::primitives::keccak256(signer.address().as_slice()).into();
    let verification = verify::verify_onchain_stellar_gmp(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &stellar_example_addr,
        stellar_rpc,
        &stellar_network_type,
        &stellar_gateway_addr,
        signer_pk,
        &mut report.transactions,
        verify::SourceChainType::Evm,
        args.network,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_stellar_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let solana_rpc = args.destination_rpc.clone();
    validate_solana_rpc(&solana_rpc).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Stellar AxelarExample.send → Solana memo)");

    // Stellar source setup
    let stellar_rpc = &args.source_rpc;
    let network_type = read_stellar_network_type(&args.config, src)?;
    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, &network_type)?;
    let stellar_example = read_stellar_contract_address(&args.config, src, "AxelarExample")?;
    let stellar_gateway = read_stellar_contract_address(&args.config, src, "AxelarGateway")?;
    let stellar_xlm = read_stellar_token_address(&args.config, src)?;

    let main_wallet = load_stellar_main_wallet(args.private_key.as_deref())?;
    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    if stellar_client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
        && use_friendbot
    {
        ui::info("activating Stellar main wallet via Friendbot...");
        stellar_client
            .friendbot_fund(&main_wallet.address())
            .await?;
    }

    // Solana destination = the Solana memo program. The memo program decodes
    // an `ExecutablePayload`: a Borsh-shaped struct carrying the memo bytes
    // plus the `counter` PDA as a writable account. axe's existing
    // `evm_sender::make_executable_payload` produces exactly this shape, so
    // we reuse it and pass it through as the payload override (otherwise
    // stellar_sender's default EVM-ABI payload causes a Borsh deserialize
    // error on the destination side).
    let memo_program = evm_sender::memo_program_id(args.network);
    let destination_address = memo_program.to_string();
    ui::address("Solana memo program", &destination_address);

    let (counter_pda, _) =
        solana_sdk::pubkey::Pubkey::find_program_address(&[b"counter"], &memo_program);
    let user_payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };
    let payload_override: Option<Vec<u8>> = Some(evm_sender::make_executable_payload(
        &user_payload,
        &counter_pda,
    ));

    let num_keys = args.num_txs.max(1) as usize;
    let gas_stroops: u64 = match &args.gas_value {
        Some(v) => v
            .parse()
            .map_err(|e| eyre::eyre!("invalid --gas-value: {e}"))?,
        None => stellar_sender::DEFAULT_GAS_STROOPS,
    };
    ui::info(&format!("deriving {num_keys} Stellar keys..."));
    let main_seed = main_wallet.signing_key.to_bytes();
    let wallets = stellar_sender::derive_wallets(&main_seed, num_keys)?;
    let mainnet_starting_balance = stellar_sender::mainnet_per_key_balance_stroops(gas_stroops, 1);
    stellar_sender::ensure_funded(
        &stellar_client,
        &wallets,
        use_friendbot,
        &main_wallet,
        mainnet_starting_balance,
    )
    .await?;

    let test_start = Instant::now();
    let mut report = stellar_sender::run_burst(stellar_sender::BurstRequest {
        client: &stellar_client,
        wallets: &wallets,
        example_contract: stellar_example.clone(),
        gateway_contract: stellar_gateway,
        destination_chain: &args.destination_axelar_id,
        destination_address: &destination_address,
        payload_override,
        source_chain: src,
        gas_token_contract: stellar_xlm,
        gas_amount_stroops: gas_stroops,
    })
    .await?;
    report.destination_address = destination_address.clone();

    let verification = verify::verify_onchain_solana(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &destination_address,
        &solana_rpc,
        &mut report.transactions,
        verify::SourceChainType::Stellar,
        args.network,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

pub(super) async fn run_sol_to_stellar(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    validate_solana_rpc(&args.source_rpc).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Solana → Stellar AxelarExample)");

    let stellar_rpc = &args.destination_rpc;
    let stellar_network_type = read_stellar_network_type(&args.config, dest)?;
    let stellar_gateway_addr = read_stellar_contract_address(&args.config, dest, "AxelarGateway")?;
    let stellar_example_addr = read_stellar_contract_address(&args.config, dest, "AxelarExample")?;
    ui::address("Stellar AxelarGateway", &stellar_gateway_addr);
    ui::address("Stellar AxelarExample", &stellar_example_addr);

    let test_start = Instant::now();
    let mut report = sol_sender::run_load_test_with_metrics(
        &args,
        &stellar_example_addr,
        true, // evm_destination=true means use EVM-style payload encoding;
              // Stellar AxelarExample.execute also takes raw bytes so this
              // works for our purposes.
    )
    .await?;

    // Build a deterministic dummy ed25519 pk from the Solana keypair as the
    // Stellar simulate-only source account.
    let signer_pk: [u8; 32] = {
        let kp = crate::solana::load_keypair(args.keypair.as_deref())?;
        let pk = solana_sdk::signer::Signer::pubkey(&kp);
        let mut out = [0u8; 32];
        out.copy_from_slice(pk.as_ref());
        out
    };

    let verification = verify::verify_onchain_stellar_gmp(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &stellar_example_addr,
        stellar_rpc,
        &stellar_network_type,
        &stellar_gateway_addr,
        signer_pk,
        &mut report.transactions,
        verify::SourceChainType::Svm,
        args.network,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

// ===========================================================================
// Sui (Move) sources and destinations — GMP
// ===========================================================================

/// Cross-chain gas attached to a Sui GMP send. Empirically the relayer's
/// base fee for sui→xrpl-evm hits ~0.06 SUI, so 0.1 SUI gives a 1.5×
/// safety margin while staying cheap. Override per-run with `--gas-value`.
pub(super) const SUI_DEFAULT_GAS_VALUE_MIST: u64 = 100_000_000;
/// On-chain Sui gas budget for executing the PTB itself (separate from the
/// cross-chain gas payment).
pub(super) const SUI_DEFAULT_GAS_BUDGET_MIST: u64 = 50_000_000;

/// Sui source -> any EVM destination, GMP only (sequential burst).
pub(super) async fn run_sui_to_evm(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let evm_rpc_url = args.destination_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt from this check.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Example.gmp.send_call)");

    let (sui_rpc, sui_contracts) = crate::sui::read_sui_chain_config(&args.config, src)?;
    let sui_rpc = if args.source_rpc.is_empty() {
        sui_rpc
    } else {
        args.source_rpc.clone()
    };
    let sui_client = crate::sui::SuiClient::new(&sui_rpc);
    let chain_id = sui_client
        .get_chain_identifier()
        .await
        .unwrap_or_else(|_| "?".to_string());
    ui::kv("Sui chain id", &chain_id);

    let main_wallet = load_sui_main_wallet()?;
    ui::kv("Sui wallet", &main_wallet.address_hex());
    let bal = sui_client.get_balance(&main_wallet.address).await?;
    let sui_amount = bal as f64 / 1e9;
    ui::kv("Sui balance", &format!("{bal} mist ({sui_amount:.4} SUI)"));
    if bal < SUI_DEFAULT_GAS_VALUE_MIST + SUI_DEFAULT_GAS_BUDGET_MIST {
        eyre::bail!(
            "Sui wallet {} has insufficient SUI: {bal} mist. Need ≥ {} mist. \
             Get testnet SUI from `https://faucet.sui.io/?address={}`",
            main_wallet.address_hex(),
            SUI_DEFAULT_GAS_VALUE_MIST + SUI_DEFAULT_GAS_BUDGET_MIST,
            main_wallet.address_hex(),
        );
    }

    let gateway_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGateway", dest)?
        .parse()?;
    let gas_service_addr = cfg
        .chains
        .get(dest)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", dest))?
        .contract_address("AxelarGasService", dest)?
        .parse()?;
    ui::address("EVM gateway", &format!("{gateway_addr}"));
    ensure_evm_contract_deployed(&evm_rpc_url, "destination AxelarGateway", gateway_addr).await?;

    let evm_private_key = args
        .private_key
        .clone()
        .or_else(|| std::env::var("EVM_PRIVATE_KEY").ok());
    let cache = read_cache(dest);
    let cached_sr = cache.get("senderReceiverAddress").and_then(|v| v.as_str());
    if cached_sr.is_none() && evm_private_key.is_none() {
        eyre::bail!("no SenderReceiver cached for '{dest}' and no EVM private key available.");
    }
    let (sender_receiver_addr, provider) = ensure_sender_receiver(
        &args,
        &evm_rpc_url,
        gateway_addr,
        gas_service_addr,
        cache,
        evm_private_key.as_deref(),
    )
    .await?;
    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    let gas_value_mist: u64 = match &args.gas_value {
        Some(v) => v
            .parse()
            .map_err(|e| eyre::eyre!("invalid --gas-value: {e}"))?,
        None => SUI_DEFAULT_GAS_VALUE_MIST,
    };
    ui::kv(
        "cross-chain gas",
        &format!("{gas_value_mist} mist (paid via Sui GasService)"),
    );

    let payload_bytes: Vec<u8> = match &args.payload {
        Some(hex_str) => hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))
            .map_err(|e| eyre::eyre!("invalid --payload hex: {e}"))?,
        None => {
            let s: String = format!(
                "Hello from Sui axe load-test {}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            s.abi_encode()
        }
    };

    let num_txs = args.num_txs.max(1) as usize;

    let test_start = Instant::now();
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    let mut metrics: Vec<TxMetrics> = Vec::with_capacity(num_txs);

    // Sui's `Example::gmp::send_call` takes the destination address as a
    // String (e.g. `"0xd7f2…"`), not raw bytes. Match what `sui/gmp.js` does.
    let dest_addr_str = format!("{sender_receiver_addr}");
    for i in 0..num_txs {
        let send_start = Instant::now();
        let result = crate::sui::send_gmp_call(
            &sui_client,
            &main_wallet,
            &sui_contracts,
            &crate::sui::SuiGmpCall {
                destination_chain: args.destination_axelar_id.clone(),
                destination_address: dest_addr_str.clone(),
                payload: payload_bytes.clone(),
                gas_value_mist,
                gas_budget_mist: SUI_DEFAULT_GAS_BUDGET_MIST,
            },
        )
        .await;

        match result {
            Ok(r) if r.success => {
                let latency_ms = send_start.elapsed().as_millis() as u64;
                let message_id = format!("{}-{}", r.digest, r.event_index);
                metrics.push(TxMetrics {
                    signature: message_id,
                    submit_time_ms: latency_ms,
                    confirm_time_ms: Some(latency_ms),
                    latency_ms: Some(latency_ms),
                    compute_units: None,
                    slot: None,
                    success: true,
                    error: None,
                    payload: payload_bytes.clone(),
                    payload_hash: r.payload_hash_hex.clone(),
                    source_address: format!("0x{}", r.source_address_hex),
                    gmp_destination_chain: args.destination_axelar_id.clone(),
                    gmp_destination_address: destination_address.clone(),
                    send_instant: Some(send_start),
                    amplifier_timing: None,
                });
                spinner.set_message(format!("sending ({}/{num_txs} confirmed)...", i + 1));
            }
            Ok(r) => {
                metrics.push(TxMetrics {
                    signature: String::new(),
                    submit_time_ms: 0,
                    confirm_time_ms: None,
                    latency_ms: None,
                    compute_units: None,
                    slot: None,
                    success: false,
                    error: r.error.or_else(|| Some("Sui tx failed".to_string())),
                    payload: payload_bytes.clone(),
                    payload_hash: String::new(),
                    source_address: String::new(),
                    gmp_destination_chain: String::new(),
                    gmp_destination_address: String::new(),
                    send_instant: None,
                    amplifier_timing: None,
                });
            }
            Err(e) => {
                metrics.push(TxMetrics {
                    signature: String::new(),
                    submit_time_ms: 0,
                    confirm_time_ms: None,
                    latency_ms: None,
                    compute_units: None,
                    slot: None,
                    success: false,
                    error: Some(e.to_string()),
                    payload: payload_bytes.clone(),
                    payload_hash: String::new(),
                    source_address: String::new(),
                    gmp_destination_chain: String::new(),
                    gmp_destination_address: String::new(),
                    send_instant: None,
                    amplifier_timing: None,
                });
            }
        }
    }

    spinner.finish_and_clear();
    let total_submitted = metrics.len() as u64;
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    ui::success(&format!(
        "sent {total_confirmed}/{total_submitted} confirmed"
    ));

    let test_duration = test_start.elapsed().as_secs_f64();
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: destination_address.clone(),
            num_txs: total_submitted,
            num_keys: 1,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    let verification = verify::verify_onchain(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &destination_address,
        gateway_addr,
        &provider,
        &mut report.transactions,
        verify::SourceChainType::Sui,
        args.network,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

// ===========================================================================
// Sui -> Solana GMP
// ===========================================================================

/// Sui source -> Solana destination, GMP only (sequential burst).
///
/// Mirrors `run_sui_to_evm`: same Sui-side PTB (`example::gmp::send_call`),
/// but targets the Solana memo program and uses `verify_onchain_solana` for
/// the destination-side polling. Payload format matches what `run_evm_to_sol`
/// sends — the memo program accepts the same ABI-string framing.
pub(super) async fn run_sui_to_sol(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    let solana_rpc = args.destination_rpc.clone();
    validate_solana_rpc(&solana_rpc).await?;

    // A legacy (consensus) EVM destination has no Cosmos Gateway and is verified
    // on its on-chain gateway instead, so it's exempt from this check.
    let (_, dst_legacy) =
        verify::classify_route(&cfg, &args.source_axelar_id, &args.destination_axelar_id);
    if !dst_legacy && cfg.axelar.contract_address("Gateway", dest).is_err() {
        eyre::bail!(
            "destination chain '{dest}' has no Cosmos Gateway in the config — verification would fail."
        );
    }

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Sui → Solana via memo)");

    let (sui_rpc, sui_contracts) = crate::sui::read_sui_chain_config(&args.config, src)?;
    let sui_rpc = if args.source_rpc.is_empty() {
        sui_rpc
    } else {
        args.source_rpc.clone()
    };
    let sui_client = crate::sui::SuiClient::new(&sui_rpc);
    let chain_id = sui_client
        .get_chain_identifier()
        .await
        .unwrap_or_else(|_| "?".to_string());
    ui::kv("Sui chain id", &chain_id);

    let main_wallet = load_sui_main_wallet()?;
    ui::kv("Sui wallet", &main_wallet.address_hex());
    let bal = sui_client.get_balance(&main_wallet.address).await?;
    let sui_amount = bal as f64 / 1e9;
    ui::kv("Sui balance", &format!("{bal} mist ({sui_amount:.4} SUI)"));
    if bal < SUI_DEFAULT_GAS_VALUE_MIST + SUI_DEFAULT_GAS_BUDGET_MIST {
        eyre::bail!(
            "Sui wallet {} has insufficient SUI: {bal} mist. Need ≥ {} mist.",
            main_wallet.address_hex(),
            SUI_DEFAULT_GAS_VALUE_MIST + SUI_DEFAULT_GAS_BUDGET_MIST,
        );
    }

    // Destination on Solana: the memo program (resolved from the target network).
    // Same target `run_evm_to_sol` uses, so the receive-side flow on Solana
    // is identical (memo program acks via gateway).
    let destination_address = evm_sender::memo_program_id(args.network).to_string();
    ui::kv("destination program", &destination_address);

    // The Solana axelar gateway needs the executable-payload framing
    // (1-byte scheme prefix + ABI-encoded SolanaGatewayPayload with the
    // counter PDA as a writable account). Building the bytes here matches
    // what `run_evm_to_sol` sends so the Solana relayer can execute it.
    let memo_program_id = evm_sender::memo_program_id(args.network);
    let (counter_pda, _) =
        solana_sdk::pubkey::Pubkey::find_program_address(&[b"counter"], &memo_program_id);

    let gas_value_mist: u64 = match &args.gas_value {
        Some(v) => v
            .parse()
            .map_err(|e| eyre::eyre!("invalid --gas-value: {e}"))?,
        None => SUI_DEFAULT_GAS_VALUE_MIST,
    };
    ui::kv(
        "cross-chain gas",
        &format!("{gas_value_mist} mist (paid via Sui GasService)"),
    );

    // `--payload`, if provided, overrides the memo body inside the
    // executable-payload wrapper. Otherwise `make_executable_payload`
    // picks a random "hello from axe load test ..." string.
    let inner_payload: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(
            hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))
                .map_err(|e| eyre::eyre!("invalid --payload hex: {e}"))?,
        ),
        None => None,
    };

    let num_txs = args.num_txs.max(1) as usize;

    let test_start = Instant::now();
    let spinner = ui::wait_spinner(&format!("sending (0/{num_txs} confirmed)..."));
    let mut metrics: Vec<TxMetrics> = Vec::with_capacity(num_txs);

    for i in 0..num_txs {
        let send_start = Instant::now();
        let payload_bytes = evm_sender::make_executable_payload(&inner_payload, &counter_pda);
        let result = crate::sui::send_gmp_call(
            &sui_client,
            &main_wallet,
            &sui_contracts,
            &crate::sui::SuiGmpCall {
                destination_chain: args.destination_axelar_id.clone(),
                destination_address: destination_address.clone(),
                payload: payload_bytes.clone(),
                gas_value_mist,
                gas_budget_mist: SUI_DEFAULT_GAS_BUDGET_MIST,
            },
        )
        .await;

        match result {
            Ok(r) if r.success => {
                let latency_ms = send_start
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                let message_id = format!("{}-{}", r.digest, r.event_index);
                metrics.push(TxMetrics {
                    signature: message_id,
                    submit_time_ms: latency_ms,
                    confirm_time_ms: Some(latency_ms),
                    latency_ms: Some(latency_ms),
                    compute_units: None,
                    slot: None,
                    success: true,
                    error: None,
                    payload: payload_bytes.clone(),
                    payload_hash: r.payload_hash_hex.clone(),
                    source_address: format!("0x{}", r.source_address_hex),
                    gmp_destination_chain: args.destination_axelar_id.clone(),
                    gmp_destination_address: destination_address.clone(),
                    send_instant: Some(send_start),
                    amplifier_timing: None,
                });
                spinner.set_message(format!("sending ({}/{num_txs} confirmed)...", i + 1));
            }
            Ok(r) => {
                metrics.push(TxMetrics {
                    signature: String::new(),
                    submit_time_ms: 0,
                    confirm_time_ms: None,
                    latency_ms: None,
                    compute_units: None,
                    slot: None,
                    success: false,
                    error: r.error.or_else(|| Some("Sui tx failed".to_string())),
                    payload: payload_bytes.clone(),
                    payload_hash: String::new(),
                    source_address: String::new(),
                    gmp_destination_chain: String::new(),
                    gmp_destination_address: String::new(),
                    send_instant: None,
                    amplifier_timing: None,
                });
            }
            Err(e) => {
                metrics.push(TxMetrics {
                    signature: String::new(),
                    submit_time_ms: 0,
                    confirm_time_ms: None,
                    latency_ms: None,
                    compute_units: None,
                    slot: None,
                    success: false,
                    error: Some(e.to_string()),
                    payload: payload_bytes.clone(),
                    payload_hash: String::new(),
                    source_address: String::new(),
                    gmp_destination_chain: String::new(),
                    gmp_destination_address: String::new(),
                    send_instant: None,
                    amplifier_timing: None,
                });
            }
        }
    }

    spinner.finish_and_clear();
    let total_submitted = metrics.len() as u64;
    let total_confirmed = metrics.iter().filter(|m| m.success).count() as u64;
    ui::success(&format!(
        "sent {total_confirmed}/{total_submitted} confirmed"
    ));

    let test_duration = test_start.elapsed().as_secs_f64();
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: src.to_string(),
            destination_chain: dest.to_string(),
            destination_address: destination_address.clone(),
            num_txs: total_submitted,
            num_keys: 1,
            total_submitted,
            test_duration_secs: test_duration,
            compute_unit_summary: ComputeUnitSummary::Omit,
        },
        metrics,
    );

    let verification = verify::verify_onchain_solana(
        &args.config,
        &args.source_axelar_id,
        &args.destination_axelar_id,
        &destination_address,
        &solana_rpc,
        &mut report.transactions,
        verify::SourceChainType::Sui,
        args.network,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &mut report, test_start)
}

// ===========================================================================
// EVM -> Sui GMP
// ===========================================================================

pub(super) async fn run_evm_to_sui(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;
    let cfg = ChainsConfig::load(&args.config)?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (EVM SenderReceiver → Sui memo)");

    let evm_rpc_url = args.source_rpc.clone();
    validate_evm_rpc(&evm_rpc_url).await?;

    let signer = args
        .private_key
        .as_ref()
        .ok_or_else(|| {
            eyre::eyre!("EVM private key required. Set EVM_PRIVATE_KEY or use --private-key")
        })?
        .parse::<PrivateKeySigner>()?;
    let read_provider = ProviderBuilder::new().connect_http(evm_rpc_url.parse()?);
    check_evm_balance(&read_provider, signer.address()).await?;

    // Destination = Sui's Example.objects.GmpChannelId. The EVM
    // ContractCall payload is delivered to that channel; on Sui, the
    // executor calls the channel's execute path, gateway emits
    // MessageExecuted, and we observe via events.
    let (sui_channel, sui_rpc) = sui_dest_lookup(&args.config, dest, Some(&args.destination_rpc))?;
    ui::address("Sui GmpChannel (destination)", &sui_channel);

    let evm_gateway_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGateway", src)?
        .parse()?;
    let evm_gas_service_addr = cfg
        .chains
        .get(src)
        .ok_or_else(|| eyre::eyre!("chain '{}' not found in config", src))?
        .contract_address("AxelarGasService", src)?
        .parse()?;
    ui::address("EVM gateway", &format!("{evm_gateway_addr}"));

    let cache = read_cache(src);
    let evm_pk = args.private_key.clone();
    let (sender_receiver_addr, _provider) = ensure_sender_receiver(
        &args,
        &evm_rpc_url,
        evm_gateway_addr,
        evm_gas_service_addr,
        cache,
        evm_pk.as_deref(),
    )
    .await?;
    ui::address("EVM SenderReceiver", &format!("{sender_receiver_addr}"));

    let main_key: [u8; 32] = signer.to_bytes().into();
    let test_start = Instant::now();
    let mut report = evm_sender::run_load_test_with_metrics(
        &args,
        sender_receiver_addr,
        &main_key,
        &evm_rpc_url,
        &sui_channel,
        true,
    )
    .await?;

    finalize_sui_dest_run(
        &args,
        &mut report,
        &sui_channel,
        &sui_rpc,
        verify::SourceChainType::Evm,
        test_start,
    )
    .await
}

// ===========================================================================
// Solana -> Sui GMP
// ===========================================================================

pub(super) async fn run_sol_to_sui(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    let solana_rpc = args.source_rpc.clone();
    validate_solana_rpc(&solana_rpc).await?;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Solana → Sui via memo example)");

    let (sui_channel, sui_rpc) = sui_dest_lookup(&args.config, dest, Some(&args.destination_rpc))?;
    ui::address("Sui GmpChannel (destination)", &sui_channel);

    let test_start = Instant::now();
    // sol_sender's `run_load_test_with_metrics` handles signer load,
    // ephemeral key derivation, sustained vs burst from args.tps. Pass
    // evm_destination=true so the payload is ABI-string-encoded — Sui's
    // memo example accepts that the same way EVM SenderReceiver does.
    let mut report = sol_sender::run_load_test_with_metrics(&args, &sui_channel, true).await?;
    report.destination_address = sui_channel.clone();

    finalize_sui_dest_run(
        &args,
        &mut report,
        &sui_channel,
        &sui_rpc,
        verify::SourceChainType::Svm,
        test_start,
    )
    .await
}

// ===========================================================================
// Stellar -> Sui GMP
// ===========================================================================

pub(super) async fn run_stellar_to_sui(args: LoadTestArgs, _run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    ui::kv("source", src);
    ui::kv("destination", dest);
    ui::kv("protocol", "GMP (Stellar AxelarExample.send → Sui memo)");

    let (sui_channel, sui_rpc) = sui_dest_lookup(&args.config, dest, Some(&args.destination_rpc))?;
    ui::address("Sui GmpChannel (destination)", &sui_channel);

    let stellar_rpc = &args.source_rpc;
    let network_type = read_stellar_network_type(&args.config, src)?;
    let stellar_client = crate::stellar::StellarClient::new(stellar_rpc, &network_type)?;
    let stellar_example = read_stellar_contract_address(&args.config, src, "AxelarExample")?;
    let stellar_gateway = read_stellar_contract_address(&args.config, src, "AxelarGateway")?;
    let stellar_xlm = read_stellar_token_address(&args.config, src)?;

    let main_wallet = load_stellar_main_wallet(args.private_key.as_deref())?;
    let use_friendbot = matches!(network_type.as_str(), "testnet" | "futurenet");
    if stellar_client
        .account_sequence(&main_wallet.address())
        .await?
        .is_none()
        && use_friendbot
    {
        ui::info("activating Stellar main wallet via Friendbot...");
        stellar_client
            .friendbot_fund(&main_wallet.address())
            .await?;
    }

    let payload_override: Option<Vec<u8>> = match &args.payload {
        Some(hex_str) => Some(hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?),
        None => None,
    };

    let num_keys = args.num_txs.max(1) as usize;
    let gas_stroops: u64 = match &args.gas_value {
        Some(v) => v
            .parse()
            .map_err(|e| eyre::eyre!("invalid --gas-value: {e}"))?,
        None => stellar_sender::DEFAULT_GAS_STROOPS,
    };
    let main_seed = main_wallet.signing_key.to_bytes();
    let wallets = stellar_sender::derive_wallets(&main_seed, num_keys)?;
    let mainnet_starting_balance = stellar_sender::mainnet_per_key_balance_stroops(gas_stroops, 1);
    stellar_sender::ensure_funded(
        &stellar_client,
        &wallets,
        use_friendbot,
        &main_wallet,
        mainnet_starting_balance,
    )
    .await?;

    let test_start = Instant::now();
    let mut report = stellar_sender::run_burst(stellar_sender::BurstRequest {
        client: &stellar_client,
        wallets: &wallets,
        example_contract: stellar_example.clone(),
        gateway_contract: stellar_gateway,
        destination_chain: &args.destination_axelar_id,
        destination_address: &sui_channel,
        payload_override,
        source_chain: src,
        gas_token_contract: stellar_xlm,
        gas_amount_stroops: gas_stroops,
    })
    .await?;
    report.destination_address = sui_channel.clone();

    finalize_sui_dest_run(
        &args,
        &mut report,
        &sui_channel,
        &sui_rpc,
        verify::SourceChainType::Stellar,
        test_start,
    )
    .await
}
