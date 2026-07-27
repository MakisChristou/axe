mod destination;
mod relay;
mod sender_receiver;
mod source;

use std::path::PathBuf;
use std::time::Instant;

use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner};
use eyre::Result;
use serde_json::json;
use solana_sdk::signer::Signer;

use destination::approve_and_execute_evm;
use sender_receiver::ensure_sender_receiver_deployed;
use source::send_evm_call_contract;

use crate::cli::resolve_axelar_id;
use crate::config::ChainsConfig;
use crate::cosmos::{check_axelar_balance, derive_axelar_wallet};
use crate::preflight;
use crate::state::read_state;
use crate::types::ChainType;
use crate::ui;

const TOTAL_STEPS: usize = 8;

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let mut state = read_state(&axelar_id)?;
    let gmp_start = Instant::now();

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();
    let cfg = ChainsConfig::load(&target_json)?;

    let private_key = state
        .deployer_private_key
        .clone()
        .ok_or_else(|| eyre::eyre!("no deployerPrivateKey in state"))?;

    let signer: PrivateKeySigner = private_key.parse()?;
    let deployer_address = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);

    preflight::check_deployer_balance(&rpc_url, deployer_address, &target_json, &axelar_id).await?;

    let chain_cfg = cfg
        .chains
        .get(&axelar_id)
        .ok_or_else(|| eyre::eyre!("chain '{axelar_id}' not found in config"))?;
    let gateway_addr: alloy::primitives::Address = chain_cfg
        .contract_address("AxelarGateway", &axelar_id)?
        .parse()?;
    let gas_service_addr: alloy::primitives::Address = chain_cfg
        .contract_address("AxelarGasService", &axelar_id)?
        .parse()?;

    ui::section(&format!("GMP Test: {axelar_id}"));
    ui::address("gateway", &format!("{gateway_addr}"));
    ui::address("gas service", &format!("{gas_service_addr}"));

    let sender_receiver_addr =
        ensure_sender_receiver_deployed(&provider, &mut state, gateway_addr, gas_service_addr)
            .await?;

    let sent =
        send_evm_call_contract(&provider, sender_receiver_addr, &axelar_id, 1, TOTAL_STEPS).await?;
    let source::SentGmp {
        destination_chain,
        destination_address,
        source_address,
        message_id,
        payload_bytes,
        payload_hash,
    } = sent;

    ui::section("Amplifier Routing");

    let (signing_key, axelar_address) = derive_axelar_wallet(&state.mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;

    let cosm_gateway = cfg.axelar.contract_address("Gateway", &axelar_id)?;
    let voting_verifier = cfg.axelar.contract_address("VotingVerifier", &axelar_id)?;
    let multisig_prover = cfg.axelar.contract_address("MultisigProver", &axelar_id)?;

    ui::address("cosmos gateway", cosm_gateway);
    ui::address("voting verifier", voting_verifier);
    ui::address("axelar address", &axelar_address);

    let gmp_msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": destination_chain,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": alloy::hex::encode(payload_hash.as_slice()),
    });

    let ctx = relay::AmplifierContext {
        axelar_address: axelar_address.clone(),
        lcd: lcd.clone(),
        chain_id: chain_id.clone(),
        fee_denom: fee_denom.clone(),
        gas_price,
        cosm_gateway: cosm_gateway.to_string(),
        voting_verifier: Some(voting_verifier.to_string()),
        multisig_prover: multisig_prover.to_string(),
    };
    let execute_data_hex = relay::run_full_sequence(
        &ctx,
        &signing_key,
        &gmp_msg,
        &axelar_id,
        &message_id,
        TOTAL_STEPS,
    )
    .await?;

    approve_and_execute_evm(destination::EvmExecutionRequest {
        provider: &provider,
        gateway: gateway_addr,
        sender_receiver: sender_receiver_addr,
        source_chain: &axelar_id,
        source_address: &source_address,
        message_id: &message_id,
        execute_data_hex: &execute_data_hex,
        payload_bytes: &payload_bytes,
        payload_hash,
        step_idx_approve: 7,
        step_idx_execute: 8,
        total_steps: TOTAL_STEPS,
    })
    .await?;

    ui::section("Complete");
    ui::success(&format!(
        "GMP flow complete ({})",
        ui::format_elapsed(gmp_start)
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Config-based GMP test (supports EVM + Solana)
// ---------------------------------------------------------------------------

pub async fn run_config(
    config: PathBuf,
    network: crate::types::Network,
    source_chain: Option<String>,
    destination_chain: Option<String>,
    destination_address: Option<String>,
    mnemonic_override: Option<String>,
) -> Result<()> {
    let cfg = ChainsConfig::load(&config)?;

    let src = source_chain.ok_or_else(|| eyre::eyre!("--source-chain required with --config"))?;
    let dst = destination_chain.unwrap_or_else(|| src.clone());

    let src_cfg = cfg
        .chains
        .get(&src)
        .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?;
    let dst_cfg = cfg
        .chains
        .get(&dst)
        .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?;

    let src_type: ChainType = src_cfg
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no chainType for source chain '{src}'"))?
        .parse()?;
    let dst_type: ChainType = dst_cfg
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no chainType for destination chain '{dst}'"))?
        .parse()?;

    let src_rpc = src_cfg
        .rpc
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC for source chain '{src}'"))?;

    let gmp_start = Instant::now();
    ui::section(&format!("GMP Test: {src} → {dst}"));
    ui::kv("source", &format!("{src} ({src_type})"));
    ui::kv("destination", &format!("{dst} ({dst_type})"));

    let mnemonic = mnemonic_override
        .clone()
        .or_else(|| std::env::var("MNEMONIC").ok())
        .ok_or_else(|| eyre::eyre!("MNEMONIC env var or --mnemonic required for relay"))?;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;

    ui::section("Preflight");
    ui::address("axelar address", &axelar_address);
    // Min: 4 relay txs at ~5k uaxl each + headroom = 0.1 AXL.
    const MIN_RELAY_BALANCE_UAXL: u128 = 100_000;
    check_axelar_balance(
        &lcd,
        &chain_id,
        &axelar_address,
        &fee_denom,
        MIN_RELAY_BALANCE_UAXL,
    )
    .await?;

    // Catch underfunded Solana keys here with a clear error rather than the
    // cryptic "Attempt to debit an account but found no record of a prior
    // credit" we'd otherwise get from the RPC at send-time.
    if src_type == ChainType::Svm || dst_type == ChainType::Svm {
        let keypair = crate::solana::load_keypair(None)?;
        if src_type == ChainType::Svm {
            crate::solana::check_solana_balance(
                src_rpc,
                "source",
                &keypair.pubkey(),
                crate::solana::MIN_SOL_SEND_LAMPORTS,
            )?;
        }
        if dst_type == ChainType::Svm {
            let dst_rpc = dst_cfg
                .rpc
                .as_deref()
                .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?;
            crate::solana::check_solana_balance(
                dst_rpc,
                "destination",
                &keypair.pubkey(),
                crate::solana::MIN_SOL_RELAY_LAMPORTS,
            )?;
        }
    }

    // For sol→evm without an explicit `--destination-address`, reuse the
    // load-test SenderReceiver cache and auto-deploy a fresh receiver on the
    // destination chain if needed. Same logic the sol-to-evm load-test
    // already uses, so the cache is shared between the two commands.
    let destination_address: Option<String> = if src_type == ChainType::Svm
        && dst_type == ChainType::Evm
        && destination_address.is_none()
    {
        let dst_rpc = dst_cfg
            .rpc
            .as_deref()
            .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?;
        let evm_pk = std::env::var("EVM_PRIVATE_KEY").map_err(|_| {
            eyre::eyre!(
                "EVM_PRIVATE_KEY env var required to deploy/reuse SenderReceiver on '{dst}'"
            )
        })?;
        let gateway_addr: alloy::primitives::Address =
            dst_cfg.contract_address("AxelarGateway", &dst)?.parse()?;
        let gas_service_addr: alloy::primitives::Address = dst_cfg
            .contract_address("AxelarGasService", &dst)?
            .parse()?;
        ui::section(&format!("Destination SenderReceiver ({dst})"));
        let addr = crate::commands::load_test::ensure_sender_receiver_on_evm_chain(
            &dst,
            dst_rpc,
            &evm_pk,
            gateway_addr,
            gas_service_addr,
        )
        .await?;
        ui::address("SenderReceiver", &format!("{addr}"));
        Some(format!("{addr}"))
    } else {
        destination_address
    };

    let sent = match src_type {
        ChainType::Svm => source::send_svm_call_contract(
            src_rpc,
            network,
            &dst,
            destination_address.as_deref(),
            1,
            8,
        )?,
        ChainType::Evm => {
            return Err(eyre::eyre!(
                "EVM source not yet supported in config mode. Use --axelar-id for EVM chains."
            ));
        }
    };
    let source::SentGmp {
        destination_chain: _,
        destination_address,
        source_address,
        message_id,
        payload_bytes,
        payload_hash,
    } = sent;
    let payload_hash_hex = alloy::hex::encode(payload_hash);

    let cosm_gateway = cfg.axelar.contract_address("Gateway", &src)?;
    let voting_verifier = cfg.axelar.contract_address("VotingVerifier", &src).ok();
    let multisig_prover = cfg.axelar.contract_address("MultisigProver", &dst)?;

    ui::section("Amplifier Routing");
    ui::address("cosmos gateway", cosm_gateway);
    if let Some(vv) = voting_verifier {
        ui::address("voting verifier", vv);
    }
    ui::address("axelar address", &axelar_address);

    let gmp_msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": src,
        },
        "destination_chain": dst,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": payload_hash_hex,
    });

    let ctx = relay::AmplifierContext {
        axelar_address: axelar_address.clone(),
        lcd: lcd.clone(),
        chain_id: chain_id.clone(),
        fee_denom: fee_denom.clone(),
        gas_price,
        cosm_gateway: cosm_gateway.to_string(),
        voting_verifier: voting_verifier.map(str::to_string),
        multisig_prover: multisig_prover.to_string(),
    };
    let execute_data_hex =
        relay::run_full_sequence(&ctx, &signing_key, &gmp_msg, &src, &message_id, 8).await?;

    match dst_type {
        ChainType::Svm => {
            let dst_rpc = dst_cfg
                .rpc
                .as_deref()
                .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?;
            destination::approve_and_execute_svm(destination::SvmExecutionRequest {
                dst_rpc,
                network,
                source_chain: &src,
                destination_chain: &dst,
                source_address: &source_address,
                destination_address: &destination_address,
                message_id: &message_id,
                payload_bytes: &payload_bytes,
                payload_hash,
                execute_data_hex: &execute_data_hex,
                step_idx_approve: 7,
                step_idx_execute: 8,
                total_steps: 8,
            })?;
        }
        ChainType::Evm => {
            let dst_rpc = dst_cfg
                .rpc
                .as_deref()
                .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{dst}'"))?;
            let sender_receiver: alloy::primitives::Address = destination_address
                .parse()
                .map_err(|e| eyre::eyre!("invalid --destination-address: {e}"))?;
            let evm_pk = std::env::var("EVM_PRIVATE_KEY").map_err(|_| {
                eyre::eyre!("EVM_PRIVATE_KEY env var required for sol→evm GMP destination")
            })?;
            let evm_signer: PrivateKeySigner = evm_pk.parse()?;
            let dst_provider = ProviderBuilder::new()
                .wallet(evm_signer)
                .connect_http(dst_rpc.parse()?);
            let gateway_addr: alloy::primitives::Address =
                dst_cfg.contract_address("AxelarGateway", &dst)?.parse()?;
            ui::address("destination EVM gateway", &format!("{gateway_addr}"));
            ui::address("destination SenderReceiver", &format!("{sender_receiver}"));

            approve_and_execute_evm(destination::EvmExecutionRequest {
                provider: &dst_provider,
                gateway: gateway_addr,
                sender_receiver,
                source_chain: &src,
                source_address: &source_address,
                message_id: &message_id,
                execute_data_hex: &execute_data_hex,
                payload_bytes: &payload_bytes,
                payload_hash,
                step_idx_approve: 7,
                step_idx_execute: 8,
                total_steps: 8,
            })
            .await?;
        }
    }

    ui::section("Complete");
    ui::success(&format!(
        "GMP flow complete ({})",
        ui::format_elapsed(gmp_start)
    ));

    Ok(())
}
