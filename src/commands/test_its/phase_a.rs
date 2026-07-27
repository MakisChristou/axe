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

use super::cache::{ItsTestCache, save_cache};
use super::encoding::{encode_inner_deploy, encode_receive_from_hub, encode_send_to_hub_deploy};
use super::relay::{DestinationRelayRequest, HubRelayRequest, relay_to_destination, relay_to_hub};
use crate::commands::event_extractors::generate_salt;
use crate::commands::test_helpers::CosmosTxContext;
use crate::evm::{ERC20, InterchainTokenService};
use crate::timing::{DEST_CHAIN_POLL_ATTEMPTS, DEST_CHAIN_POLL_INTERVAL};
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

pub(super) async fn run_phase_a_deploy<P: Provider>(
    request: PhaseADeployRequest<'_, P>,
) -> Result<([u8; 32], Address)> {
    let PhaseADeployRequest {
        network,
        src_axelar_id,
        dst_axelar_id,
        src_rpc,
        sol_keypair,
        sol_pubkey,
        tx,
        src_cosm_gateway,
        voting_verifier,
        axelarnet_gateway,
        dst_cosm_gateway,
        dst_multisig_prover,
        axelar_rpc,
        its_hub_address,
        dst_its_proxy,
        dst_evm_gateway,
        dst_provider,
        its,
        gas_value,
        cache_file,
        phase_start,
    } = request;

    ui::section("Phase A: deploy local + remote (manual relay)");

    // Step A1: generate salt, derive token id
    let salt = generate_salt();
    let salt_bytes: [u8; 32] = salt.0;
    let token_id = crate::solana::interchain_token_id(network, &sol_pubkey, &salt_bytes);
    let token_id_b32 = FixedBytes::<32>::from(token_id);

    ui::step_header(1, PHASE_A_STEPS, "Generate salt + tokenId");
    ui::kv("salt", &format!("0x{}", alloy::hex::encode(salt_bytes)));
    ui::kv("tokenId", &format!("0x{}", alloy::hex::encode(token_id)));

    // Step A2: Solana — deploy local interchain token
    ui::step_header(2, PHASE_A_STEPS, "Deploy local interchain token (Solana)");
    let spec = crate::types::ITS_CONFIG_SPEC;
    let local_sig = crate::solana::send_its_deploy_interchain_token(
        crate::solana::DeployInterchainTokenRequest {
            rpc_url: src_rpc,
            keypair: sol_keypair,
            network,
            salt: &salt_bytes,
            name: spec.name,
            symbol: spec.symbol,
            decimals: spec.decimals,
            initial_supply: INITIAL_SUPPLY,
            minter: None,
        },
    )?;
    ui::tx_hash("solana tx", &local_sig);
    ui::success(&format!(
        "local mint deployed (initial supply {INITIAL_SUPPLY})"
    ));

    // Step A3: Solana — deploy remote interchain token (fires GMP)
    ui::step_header(
        3,
        PHASE_A_STEPS,
        "Deploy remote interchain token (Solana → hub)",
    );
    let remote_sig = crate::solana::send_its_deploy_remote_interchain_token(
        src_rpc,
        sol_keypair,
        network,
        &salt_bytes,
        dst_axelar_id.as_str(),
        gas_value,
    )?;
    ui::tx_hash("solana tx", &remote_sig);

    let first_leg_id = crate::solana::extract_its_message_id(src_rpc, network, &remote_sig)?;
    ui::kv("first-leg message_id", &first_leg_id);

    // Read the actual on-chain CallContractEvent. The verifiers will look up
    // the same fields; using on-chain values eliminates encoding-mismatch risk.
    let gw = crate::solana::extract_gateway_call_contract_payload(src_rpc, &remote_sig)?;
    ui::kv("gateway sender", &gw.sender);
    ui::kv("gateway destination_chain", &gw.destination_chain);
    ui::kv("gateway destination_address", &gw.destination_address);
    ui::kv(
        "gateway payload_hash",
        &format!("0x{}", alloy::hex::encode(gw.payload_hash)),
    );
    ui::kv(
        "gateway payload (len)",
        &format!("{} bytes", gw.payload.len()),
    );

    // Sanity: the local reconstruction should match what the gateway actually saw.
    let local_payload = encode_send_to_hub_deploy(
        dst_axelar_id.as_str(),
        &token_id,
        spec.name,
        spec.symbol,
        spec.decimals,
        None,
    )?;
    let local_hash = keccak256(&local_payload);
    if local_hash.as_slice() != gw.payload_hash {
        ui::warn("local payload reconstruction does not match on-chain payload:");
        ui::warn(&format!(
            "  local  : 0x{}",
            alloy::hex::encode(local_hash.as_slice())
        ));
        ui::warn(&format!(
            "  on-chain: 0x{}",
            alloy::hex::encode(gw.payload_hash)
        ));
    }

    let first_leg_payload = gw.payload.clone();
    let first_leg_payload_hash = FixedBytes::<32>::from(gw.payload_hash);
    let gw_sender = gw.sender.clone();

    // Step A4: drive source → hub via existing relay_to_hub helper
    ui::step_header(
        4,
        PHASE_A_STEPS,
        "Source → hub (verify, route, hub-execute)",
    );
    relay_to_hub(HubRelayRequest {
        axelar_id: src_axelar_id.as_str(),
        message_id: &first_leg_id,
        source_address: &gw_sender,
        destination_chain: crate::types::HubChain::NAME,
        destination_address: its_hub_address,
        payload_hash: &first_leg_payload_hash,
        payload: &first_leg_payload,
        tx,
        cosm_gateway: src_cosm_gateway,
        voting_verifier,
        axelarnet_gateway,
    })
    .await?;

    // Step A5..10: hub → destination EVM, manual proof + execute
    let deploy_inner = encode_inner_deploy(&token_id, spec.name, spec.symbol, spec.decimals, &[]);
    let dest_payload_deploy = encode_receive_from_hub(src_axelar_id, &deploy_inner);

    relay_to_destination(DestinationRelayRequest {
        first_leg_message_id: &first_leg_id,
        src_axelar_id,
        dest_payload: &dest_payload_deploy,
        dst_its_proxy,
        dst_evm_gateway,
        dst_provider,
        tx,
        dst_cosm_gateway,
        dst_multisig_prover,
        axelarnet_gateway,
        axelar_rpc,
        step_base: 5,
        step_total: PHASE_A_STEPS,
    })
    .await?;

    // Step A11: verify destination token is deployed
    ui::step_header(11, PHASE_A_STEPS, "Verify destination token deployed");
    let dest_token_addr = its.interchainTokenAddress(token_id_b32).call().await?;
    ui::address("dest token address", &format!("{dest_token_addr}"));
    let token = ERC20::new(dest_token_addr, dst_provider);
    match token.name().call().await {
        Ok(name) => {
            ui::success(&format!("dest token responds to name() → \"{name}\""));
        }
        Err(e) => {
            ui::warn(&format!("dest token name() failed: {e}"));
            ui::info("token may still be propagating — try again or check explorer");
        }
    }

    // Persist for next run.
    let cache = ItsTestCache {
        deployer: sol_pubkey.to_string(),
        salt_hex: format!("0x{}", alloy::hex::encode(salt_bytes)),
        token_id_hex: format!("0x{}", alloy::hex::encode(token_id)),
        dest_token_address: format!("{dest_token_addr}"),
    };
    if let Err(e) = save_cache(cache_file, &cache) {
        ui::warn(&format!(
            "failed to write cache to {}: {e}",
            cache_file.display()
        ));
    } else {
        ui::info(&format!("cached tokenId at {}", cache_file.display()));
    }

    ui::section("Phase A complete");
    ui::success(&format!(
        "deploy + manual relay finished ({})",
        ui::format_elapsed(phase_start)
    ));

    Ok((token_id, dest_token_addr))
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
    use super::DEST_CHAIN;
    use super::TOTAL_STEPS;

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
