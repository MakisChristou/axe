//! Phase A — deploy an interchain token locally on Solana, fire the GMP that
//! deploys the same token remotely on the EVM destination, and drive both
//! relay legs (source → hub via [`relay_to_hub`], hub → destination EVM via
//! [`relay_to_destination`]). Caches the resulting `(token_id, dest_addr)`
//! so Phase B can be re-run without re-deploying.

use std::path::Path;
use std::time::Instant;

use alloy::{
    primitives::{Address, FixedBytes, keccak256},
    providers::Provider,
};
use eyre::Result;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use tokio::task::spawn_blocking;

use super::cache::{ItsTestCache, save_cache};
use super::encoding::{encode_inner_deploy, encode_receive_from_hub, encode_send_to_hub_deploy};
use super::relay::{DestinationRelayRequest, HubRelayRequest, relay_to_destination, relay_to_hub};
use super::{DEST_CHAIN, TOTAL_STEPS};
use crate::commands::event_extractors::generate_salt;
use crate::commands::test_helpers::CosmosTxContext;
use crate::evm::{ERC20, InterchainTokenService};
use crate::timing::{DEST_CHAIN_POLL_ATTEMPTS, DEST_CHAIN_POLL_INTERVAL};
use crate::types::Network;
use crate::ui;

const PHASE_A_STEPS: usize = 11;

// Initial supply for the config-mode test, in base units of `ITS_CONFIG_SPEC`.
// 1_000_000_000_000 = 1000 tokens at 9 decimals.
const INITIAL_SUPPLY: u64 = 1_000_000_000_000;

/// Returns `(token_id, dest_token_addr)` on success and writes the result to
/// `cache_file` so a subsequent run skips this entire phase.
pub(super) struct PhaseADeployRequest<'a, P> {
    pub network: crate::types::Network,
    pub src_axelar_id: &'a crate::types::ChainAxelarId,
    pub dst_axelar_id: &'a crate::types::ChainAxelarId,
    pub src_rpc: &'a str,
    pub sol_keypair: &'a solana_sdk::signature::Keypair,
    pub sol_pubkey: solana_sdk::pubkey::Pubkey,
    pub tx: CosmosTxContext<'a>,
    pub src_cosm_gateway: &'a str,
    pub voting_verifier: &'a str,
    pub axelarnet_gateway: &'a str,
    pub dst_cosm_gateway: &'a str,
    pub dst_multisig_prover: &'a str,
    pub axelar_rpc: &'a str,
    pub its_hub_address: &'a str,
    pub dst_its_proxy: Address,
    pub dst_evm_gateway: Address,
    pub dst_provider: &'a P,
    pub its: &'a InterchainTokenService::InterchainTokenServiceInstance<&'a P>,
    pub gas_value: u64,
    pub cache_file: &'a Path,
    pub phase_start: Instant,
}

struct SourceDeployment {
    salt: [u8; 32],
    token_id: [u8; 32],
    message_id: String,
    sender: String,
    payload: Vec<u8>,
    payload_hash: FixedBytes<32>,
}

struct SourceDeploymentRequest {
    network: Network,
    source_rpc: String,
    keypair: Keypair,
    public_key: Pubkey,
    destination_chain: String,
    gas_value: u64,
}

fn deploy_source_token(request: SourceDeploymentRequest) -> Result<SourceDeployment> {
    let SourceDeploymentRequest {
        network,
        source_rpc,
        keypair,
        public_key,
        destination_chain,
        gas_value,
    } = request;

    let salt = generate_salt().0;
    let token_id = crate::solana::interchain_token_id(network, &public_key, &salt);
    ui::step_header(1, PHASE_A_STEPS, "Generate salt + tokenId");
    ui::kv("salt", &format!("0x{}", alloy::hex::encode(salt)));
    ui::kv("tokenId", &format!("0x{}", alloy::hex::encode(token_id)));

    ui::step_header(2, PHASE_A_STEPS, "Deploy local interchain token (Solana)");
    let spec = crate::types::ITS_CONFIG_SPEC;
    let local_signature = crate::solana::send_its_deploy_interchain_token(
        crate::solana::DeployInterchainTokenRequest {
            rpc_url: &source_rpc,
            keypair: &keypair,
            network,
            salt: &salt,
            name: spec.name,
            symbol: spec.symbol,
            decimals: spec.decimals,
            initial_supply: INITIAL_SUPPLY,
            minter: None,
        },
    )?;
    ui::tx_hash("solana tx", &local_signature);
    ui::success(&format!(
        "local mint deployed (initial supply {INITIAL_SUPPLY})"
    ));

    ui::step_header(
        3,
        PHASE_A_STEPS,
        "Deploy remote interchain token (Solana → hub)",
    );
    let remote_signature = crate::solana::send_its_deploy_remote_interchain_token(
        &source_rpc,
        &keypair,
        network,
        &salt,
        &destination_chain,
        gas_value,
    )?;
    ui::tx_hash("solana tx", &remote_signature);
    let message_id =
        crate::solana::extract_its_message_id(&source_rpc, network, &remote_signature)?;
    ui::kv("first-leg message_id", &message_id);
    let gateway =
        crate::solana::extract_gateway_call_contract_payload(&source_rpc, &remote_signature)?;
    ui::kv("gateway sender", &gateway.sender);
    ui::kv("gateway destination_chain", &gateway.destination_chain);
    ui::kv("gateway destination_address", &gateway.destination_address);
    ui::kv(
        "gateway payload_hash",
        &format!("0x{}", alloy::hex::encode(gateway.payload_hash)),
    );
    ui::kv(
        "gateway payload (len)",
        &format!("{} bytes", gateway.payload.len()),
    );
    let reconstructed = encode_send_to_hub_deploy(
        &destination_chain,
        &token_id,
        spec.name,
        spec.symbol,
        spec.decimals,
        None,
    )?;
    let reconstructed_hash = keccak256(&reconstructed);
    if reconstructed_hash.as_slice() != gateway.payload_hash {
        ui::warn("local payload reconstruction does not match on-chain payload:");
        ui::warn(&format!(
            "  local  : 0x{}",
            alloy::hex::encode(reconstructed_hash.as_slice())
        ));
        ui::warn(&format!(
            "  on-chain: 0x{}",
            alloy::hex::encode(gateway.payload_hash)
        ));
    }
    Ok(SourceDeployment {
        salt,
        token_id,
        message_id,
        sender: gateway.sender,
        payload: gateway.payload,
        payload_hash: gateway.payload_hash.into(),
    })
}

async fn verify_and_cache_destination<P: Provider>(
    request: &PhaseADeployRequest<'_, P>,
    deployment: &SourceDeployment,
) -> Result<Address> {
    ui::step_header(11, PHASE_A_STEPS, "Verify destination token deployed");
    let dest_token_addr = request
        .its
        .interchainTokenAddress(deployment.token_id.into())
        .call()
        .await?;
    ui::address("dest token address", &format!("{dest_token_addr}"));
    let token = ERC20::new(dest_token_addr, request.dst_provider);
    match token.name().call().await {
        Ok(name) => ui::success(&format!("dest token responds to name() → \"{name}\"")),
        Err(error) => {
            ui::warn(&format!("dest token name() failed: {error}"));
            ui::info("token may still be propagating — try again or check explorer");
        }
    }

    let cache = ItsTestCache {
        deployer: request.sol_pubkey.to_string(),
        salt_hex: format!("0x{}", alloy::hex::encode(deployment.salt)),
        token_id_hex: format!("0x{}", alloy::hex::encode(deployment.token_id)),
        dest_token_address: format!("{dest_token_addr}"),
    };
    if let Err(error) = save_cache(request.cache_file, &cache).await {
        ui::warn(&format!(
            "failed to write cache to {}: {error}",
            request.cache_file.display()
        ));
    } else {
        ui::info(&format!(
            "cached tokenId at {}",
            request.cache_file.display()
        ));
    }
    Ok(dest_token_addr)
}

pub(super) async fn run_phase_a_deploy<P: Provider>(
    request: PhaseADeployRequest<'_, P>,
) -> Result<([u8; 32], Address)> {
    ui::section("Phase A: deploy local + remote (manual relay)");

    let deployment_request = SourceDeploymentRequest {
        network: request.network,
        source_rpc: request.src_rpc.to_string(),
        keypair: crate::solana::clone_keypair(request.sol_keypair),
        public_key: request.sol_pubkey,
        destination_chain: request.dst_axelar_id.to_string(),
        gas_value: request.gas_value,
    };
    let deployment = spawn_blocking(move || deploy_source_token(deployment_request)).await??;

    let spec = crate::types::ITS_CONFIG_SPEC;

    // Step A4: drive source → hub via existing relay_to_hub helper
    ui::step_header(
        4,
        PHASE_A_STEPS,
        "Source → hub (verify, route, hub-execute)",
    );
    relay_to_hub(HubRelayRequest {
        axelar_id: request.src_axelar_id.as_str(),
        message_id: &deployment.message_id,
        source_address: &deployment.sender,
        destination_chain: crate::types::HubChain::NAME,
        destination_address: request.its_hub_address,
        payload_hash: &deployment.payload_hash,
        payload: &deployment.payload,
        tx: request.tx,
        cosm_gateway: request.src_cosm_gateway,
        voting_verifier: request.voting_verifier,
        axelarnet_gateway: request.axelarnet_gateway,
    })
    .await?;

    // Step A5..10: hub → destination EVM, manual proof + execute
    let deploy_inner = encode_inner_deploy(
        &deployment.token_id,
        spec.name,
        spec.symbol,
        spec.decimals,
        &[],
    );
    let dest_payload_deploy = encode_receive_from_hub(request.src_axelar_id, &deploy_inner);

    relay_to_destination(DestinationRelayRequest {
        first_leg_message_id: &deployment.message_id,
        src_axelar_id: request.src_axelar_id,
        dest_payload: &dest_payload_deploy,
        dst_its_proxy: request.dst_its_proxy,
        dst_evm_gateway: request.dst_evm_gateway,
        dst_provider: request.dst_provider,
        tx: request.tx,
        dst_cosm_gateway: request.dst_cosm_gateway,
        dst_multisig_prover: request.dst_multisig_prover,
        axelarnet_gateway: request.axelarnet_gateway,
        axelar_rpc: request.axelar_rpc,
        step_base: 5,
        step_total: PHASE_A_STEPS,
    })
    .await?;

    let dest_token_addr = verify_and_cache_destination(&request, &deployment).await?;

    ui::section("Phase A complete");
    ui::success(&format!(
        "deploy + manual relay finished ({})",
        ui::format_elapsed(request.phase_start)
    ));

    Ok((deployment.token_id, dest_token_addr))
}

/// Wait for the destination-chain ITS to deploy the predicted token contract
/// (post hub relay). Uses `name()` instead of `get_code_at` since the latter
/// is unreliable on some EVMs. Returns the predicted address either
/// way; the caller can decide what to do if name() never responds.
pub(super) async fn poll_for_remote_token_deploy<P: Provider>(
    dest_provider: &P,
    dest_its_addr: Address,
    token_id: FixedBytes<32>,
) -> Result<Address> {
    let dest_its = InterchainTokenService::new(dest_its_addr, dest_provider);

    ui::step_header(
        7,
        TOTAL_STEPS,
        &format!("Poll {DEST_CHAIN} for token deployment"),
    );
    ui::address(&format!("{DEST_CHAIN} ITS"), &format!("{dest_its_addr}"));
    ui::kv("tokenId", &format!("{token_id}"));

    let predicted_addr = dest_its
        .interchainTokenAddress(token_id)
        .call()
        .await
        .map_err(|e| eyre::eyre!("failed to query interchainTokenAddress on {DEST_CHAIN}: {e}"))?;
    ui::address("predicted token addr", &format!("{predicted_addr}"));

    let spinner = ui::wait_spinner(&format!("Waiting for token to appear on {DEST_CHAIN}..."));
    let mut deployed = false;

    for i in 0..DEST_CHAIN_POLL_ATTEMPTS {
        if i > 0 {
            tokio::time::sleep(DEST_CHAIN_POLL_INTERVAL).await;
        }
        let token = ERC20::new(predicted_addr, dest_provider);
        match token.name().call().await {
            Ok(name) => {
                spinner.finish_and_clear();
                ui::success(&format!("Token responds to name() → \"{name}\""));
                deployed = true;
                break;
            }
            Err(_) => {
                spinner.set_message(format!(
                    "Token not yet deployed (attempt {}/30, addr={predicted_addr})...",
                    i + 1
                ));
            }
        }
    }
    spinner.finish_and_clear();

    if deployed {
        ui::success(&format!("Token deployed on {DEST_CHAIN}!"));
        ui::address(
            &format!("token address ({DEST_CHAIN})"),
            &format!("{predicted_addr}"),
        );
    } else {
        ui::warn(&format!(
            "Token not yet deployed on {DEST_CHAIN} after 5 minutes"
        ));
        ui::info("The relayer may still be processing. Check axelarscan for status.");
        ui::kv("tokenId", &format!("{token_id}"));
    }

    Ok(predicted_addr)
}
