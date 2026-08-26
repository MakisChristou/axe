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
use tokio::task::spawn_blocking;

use destination::approve_and_execute_evm;
use sender_receiver::ensure_sender_receiver_deployed;
use source::send_evm_call_contract;

use crate::cli::resolve_axelar_id;
use crate::config::{AxelarChainContract, ChainConfig, ChainContract, ChainsConfig};
use crate::cosmos::{check_axelar_balance, derive_axelar_wallet};
use crate::preflight;
use crate::solana::{
    MIN_SOL_RELAY_LAMPORTS, MIN_SOL_SEND_LAMPORTS, check_solana_balance, load_keypair,
};
use crate::state::read_state;
use crate::types::ChainType;
use crate::ui;

const TOTAL_STEPS: usize = 8;
const MIN_RELAY_BALANCE_UAXL: u128 = 100_000;

async fn relay_message(
    cfg: &ChainsConfig,
    state: &crate::state::State,
    axelar_id: &str,
    sent: &source::SentGmp,
) -> Result<String> {
    ui::section("Amplifier Routing");
    let (signing_key, axelar_address) = derive_axelar_wallet(&state.mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;
    let cosm_gateway = cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, axelar_id)?;
    let voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, axelar_id)?;
    let multisig_prover = cfg
        .axelar
        .contract_address(AxelarChainContract::MultisigProver, axelar_id)?;
    ui::address("cosmos gateway", cosm_gateway);
    ui::address("voting verifier", voting_verifier);
    ui::address("axelar address", &axelar_address);

    let message = json!({
        "cc_id": {
            "message_id": sent.message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": sent.destination_chain,
        "destination_address": sent.destination_address,
        "source_address": sent.source_address,
        "payload_hash": alloy::hex::encode(sent.payload_hash.as_slice()),
    });
    relay::run_full_sequence(
        &relay::AmplifierContext {
            axelar_address,
            lcd,
            chain_id,
            fee_denom,
            gas_price,
            cosm_gateway: cosm_gateway.to_string(),
            voting_verifier: Some(voting_verifier.to_string()),
            multisig_prover: multisig_prover.to_string(),
        },
        &signing_key,
        &message,
        axelar_id,
        &sent.message_id,
        TOTAL_STEPS,
    )
    .await
}

pub async fn run(axelar_id: Option<String>) -> Result<()> {
    let axelar_id = resolve_axelar_id(axelar_id)?;
    let mut state = read_state(&axelar_id).await?;
    let gmp_start = Instant::now();

    let rpc_url = state.rpc_url.clone();
    let target_json = state.target_json.clone();
    let cfg = ChainsConfig::load(&target_json).await?;

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
        .contract_address(ChainContract::AxelarGateway, &axelar_id)?
        .parse()?;
    let gas_service_addr: alloy::primitives::Address = chain_cfg
        .contract_address(ChainContract::AxelarGasService, &axelar_id)?
        .parse()?;

    ui::section(&format!("GMP Test: {axelar_id}"));
    ui::address("gateway", &format!("{gateway_addr}"));
    ui::address("gas service", &format!("{gas_service_addr}"));

    let sender_receiver_addr =
        ensure_sender_receiver_deployed(&provider, &mut state, gateway_addr, gas_service_addr)
            .await?;

    let sent =
        send_evm_call_contract(&provider, sender_receiver_addr, &axelar_id, 1, TOTAL_STEPS).await?;
    let execute_data_hex = relay_message(&cfg, &state, &axelar_id, &sent).await?;

    approve_and_execute_evm(destination::EvmExecutionRequest {
        provider: &provider,
        gateway: gateway_addr,
        sender_receiver: sender_receiver_addr,
        source_chain: &axelar_id,
        source_address: &sent.source_address,
        message_id: &sent.message_id,
        execute_data_hex: &execute_data_hex,
        payload_bytes: &sent.payload_bytes,
        payload_hash: sent.payload_hash,
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

async fn check_svm_balances(
    source_type: ChainType,
    destination_type: ChainType,
    source_rpc: &str,
    destination: &str,
    destination_config: &ChainConfig,
) -> Result<()> {
    if source_type != ChainType::Svm && destination_type != ChainType::Svm {
        return Ok(());
    }

    let destination_rpc = match destination_type {
        ChainType::Svm => Some(
            destination_config
                .rpc
                .as_deref()
                .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{destination}'"))?
                .to_string(),
        ),
        _ => None,
    };

    let pubkey = load_keypair(None).await?.pubkey();
    let source_rpc = source_rpc.to_string();

    spawn_blocking(move || -> Result<()> {
        if source_type == ChainType::Svm {
            check_solana_balance(&source_rpc, "source", &pubkey, MIN_SOL_SEND_LAMPORTS)?;
        }

        if let Some(destination_rpc) = destination_rpc {
            check_solana_balance(
                &destination_rpc,
                "destination",
                &pubkey,
                MIN_SOL_RELAY_LAMPORTS,
            )?;
        }

        Ok(())
    })
    .await??;

    Ok(())
}

async fn resolve_config_destination_address(
    source_type: ChainType,
    destination_type: ChainType,
    destination: &str,
    destination_config: &crate::config::ChainConfig,
    provided: Option<String>,
) -> Result<Option<String>> {
    if source_type != ChainType::Svm || destination_type != ChainType::Evm || provided.is_some() {
        return Ok(provided);
    }
    let rpc = destination_config
        .rpc
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{destination}'"))?;
    let private_key = std::env::var("EVM_PRIVATE_KEY").map_err(|_| {
        eyre::eyre!(
            "EVM_PRIVATE_KEY env var required to deploy/reuse SenderReceiver on '{destination}'"
        )
    })?;
    let gateway = destination_config
        .contract_address(ChainContract::AxelarGateway, destination)?
        .parse()?;
    let gas_service = destination_config
        .contract_address(ChainContract::AxelarGasService, destination)?
        .parse()?;
    ui::section(&format!("Destination SenderReceiver ({destination})"));
    let address = crate::commands::load_test::ensure_sender_receiver_on_evm_chain(
        destination,
        rpc,
        &private_key,
        gateway,
        gas_service,
    )
    .await?;
    ui::address("SenderReceiver", &format!("{address}"));
    Ok(Some(format!("{address}")))
}

async fn execute_config_destination(
    destination_type: ChainType,
    destination_config: &crate::config::ChainConfig,
    source: &str,
    destination: &str,
    network: crate::types::Network,
    sent: &source::SentGmp,
    execute_data_hex: &str,
) -> Result<destination::SubmittedTransactions> {
    let rpc = destination_config
        .rpc
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no RPC for destination chain '{destination}'"))?;
    match destination_type {
        ChainType::Svm => {
            destination::approve_and_execute_svm(destination::SvmExecutionRequest {
                dst_rpc: rpc,
                network,
                source_chain: source,
                destination_chain: destination,
                source_address: &sent.source_address,
                destination_address: &sent.destination_address,
                message_id: &sent.message_id,
                payload_bytes: &sent.payload_bytes,
                payload_hash: sent.payload_hash,
                execute_data_hex,
                step_idx_approve: 7,
                step_idx_execute: 8,
                total_steps: 8,
            })
            .await
        }
        ChainType::Evm => {
            let sender_receiver = sent
                .destination_address
                .parse()
                .map_err(|e| eyre::eyre!("invalid --destination-address: {e}"))?;
            let private_key = std::env::var("EVM_PRIVATE_KEY").map_err(|_| {
                eyre::eyre!("EVM_PRIVATE_KEY env var required for sol→evm GMP destination")
            })?;
            let provider = ProviderBuilder::new()
                .wallet(private_key.parse::<PrivateKeySigner>()?)
                .connect_http(rpc.parse()?);
            let gateway = destination_config
                .contract_address(ChainContract::AxelarGateway, destination)?
                .parse()?;
            ui::address("destination EVM gateway", &format!("{gateway}"));
            ui::address("destination SenderReceiver", &format!("{sender_receiver}"));
            approve_and_execute_evm(destination::EvmExecutionRequest {
                provider: &provider,
                gateway,
                sender_receiver,
                source_chain: source,
                source_address: &sent.source_address,
                message_id: &sent.message_id,
                execute_data_hex,
                payload_bytes: &sent.payload_bytes,
                payload_hash: sent.payload_hash,
                step_idx_approve: 7,
                step_idx_execute: 8,
                total_steps: 8,
            })
            .await
        }
    }
}

fn config_gmp_message(
    sent: &source::SentGmp,
    source: &str,
    destination: &str,
) -> serde_json::Value {
    json!({
        "cc_id": {
            "message_id": sent.message_id,
            "source_chain": source,
        },
        "destination_chain": destination,
        "destination_address": sent.destination_address,
        "source_address": sent.source_address,
        "payload_hash": alloy::hex::encode(sent.payload_hash),
    })
}

fn print_gmp_completion(started_at: Instant) {
    ui::section("Complete");
    ui::success(&format!(
        "GMP flow complete ({})",
        ui::format_elapsed(started_at)
    ));
}

fn print_config_header(
    source: &str,
    source_type: ChainType,
    destination: &str,
    destination_type: ChainType,
) {
    ui::section(&format!("GMP Test: {source} → {destination}"));
    ui::kv("source", &format!("{source} ({source_type})"));
    ui::kv(
        "destination",
        &format!("{destination} ({destination_type})"),
    );
}

struct ConfigGmpRoute {
    source_type: ChainType,
    destination_type: ChainType,
    source_rpc: String,
}

fn resolve_config_gmp_route(
    source: &str,
    source_config: &ChainConfig,
    destination: &str,
    destination_config: &ChainConfig,
) -> Result<ConfigGmpRoute> {
    let source_type = source_config
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no chainType for source chain '{source}'"))?
        .parse()?;
    let destination_type = destination_config
        .chain_type
        .as_deref()
        .ok_or_else(|| eyre::eyre!("no chainType for destination chain '{destination}'"))?
        .parse()?;
    let source_rpc = source_config
        .rpc
        .clone()
        .ok_or_else(|| eyre::eyre!("no RPC for source chain '{source}'"))?;

    Ok(ConfigGmpRoute {
        source_type,
        destination_type,
        source_rpc,
    })
}

pub async fn run_config(
    config: PathBuf,
    network: crate::types::Network,
    source_chain: Option<String>,
    destination_chain: Option<String>,
    destination_address: Option<String>,
    mnemonic_override: Option<String>,
) -> Result<destination::SubmittedTransactions> {
    let cfg = ChainsConfig::load(&config).await?;

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

    let ConfigGmpRoute {
        source_type: src_type,
        destination_type: dst_type,
        source_rpc,
    } = resolve_config_gmp_route(&src, src_cfg, &dst, dst_cfg)?;
    let src_rpc = source_rpc.as_str();

    let gmp_start = Instant::now();
    print_config_header(&src, src_type, &dst, dst_type);

    let mnemonic = mnemonic_override
        .clone()
        .or_else(|| std::env::var("MNEMONIC").ok())
        .ok_or_else(|| eyre::eyre!("MNEMONIC env var or --mnemonic required for relay"))?;
    let (signing_key, axelar_address) = derive_axelar_wallet(&mnemonic)?;
    let (lcd, chain_id, fee_denom, gas_price) = cfg.axelar.cosmos_tx_params()?;

    ui::section("Preflight");
    ui::address("axelar address", &axelar_address);
    check_axelar_balance(
        &lcd,
        &chain_id,
        &axelar_address,
        &fee_denom,
        MIN_RELAY_BALANCE_UAXL,
    )
    .await?;

    check_svm_balances(src_type, dst_type, src_rpc, &dst, dst_cfg).await?;
    let destination_address =
        resolve_config_destination_address(src_type, dst_type, &dst, dst_cfg, destination_address)
            .await?;

    let sent = match src_type {
        ChainType::Svm => {
            source::send_svm_call_contract(
                src_rpc,
                network,
                &dst,
                destination_address.as_deref(),
                1,
                8,
            )
            .await?
        }
        ChainType::Evm => {
            return Err(eyre::eyre!(
                "EVM source not yet supported in config mode. Use --axelar-id for EVM chains."
            ));
        }
    };
    let cosm_gateway = cfg
        .axelar
        .contract_address(AxelarChainContract::Gateway, &src)?;
    let voting_verifier = cfg
        .axelar
        .contract_address(AxelarChainContract::VotingVerifier, &src)
        .ok();
    let multisig_prover = cfg
        .axelar
        .contract_address(AxelarChainContract::MultisigProver, &dst)?;

    ui::section("Amplifier Routing");
    ui::address("cosmos gateway", cosm_gateway);
    if let Some(vv) = voting_verifier {
        ui::address("voting verifier", vv);
    }
    ui::address("axelar address", &axelar_address);

    let gmp_msg = config_gmp_message(&sent, &src, &dst);

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
        relay::run_full_sequence(&ctx, &signing_key, &gmp_msg, &src, &sent.message_id, 8).await?;
    let submitted = execute_config_destination(
        dst_type,
        dst_cfg,
        &src,
        &dst,
        network,
        &sent,
        &execute_data_hex,
    )
    .await?;

    print_gmp_completion(gmp_start);
    Ok(submitted)
}
