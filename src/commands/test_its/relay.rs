//! Amplifier relay sequences. Two flavours:
//!
//! - [`relay_to_hub`]: source → hub. Submits `verify_messages`, waits for the
//!   poll, ends it, calls `route_messages`, and executes on the
//!   AxelarnetGateway. Used by both Phase A's deploy hop and Phase B's
//!   transfer hop, plus the legacy `run` flow.
//! - [`relay_to_destination`]: hub → destination EVM. Discovers the
//!   second-leg cc_id, polls until the destination cosm gateway has it,
//!   constructs a proof on the destination MultisigProver, submits it to
//!   the EVM gateway, and finally executes on the destination ITS proxy.

use alloy::{
    primitives::{Address, Bytes, FixedBytes, keccak256},
    providers::Provider,
    rpc::types::TransactionRequest,
};
use eyre::Result;
use serde_json::json;

use crate::commands::test_helpers::{
    CosmosTxContext, end_poll_with_retry, execute_on_axelarnet_gateway, extract_event_attr,
    route_messages_with_retry, submit_verify_messages_amplifier, wait_for_poll_votes,
    wait_for_proof,
};
use crate::cosmos::{
    SecondLegInfo, build_execute_msg_any, check_cosmos_routed, check_hub_approved,
    discover_second_leg, sign_and_broadcast_cosmos_tx,
};
use crate::evm::{AxelarAmplifierGateway, InterchainTokenService};
use crate::timing::{
    AMPLIFIER_POLL_ATTEMPTS_5MIN, AMPLIFIER_POLL_ATTEMPTS_10MIN, AMPLIFIER_POLL_INTERVAL,
};
use crate::ui;

pub(super) struct HubRelayRequest<'a> {
    pub axelar_id: &'a str,
    pub message_id: &'a str,
    pub source_address: &'a str,
    pub destination_chain: &'a str,
    pub destination_address: &'a str,
    pub payload_hash: &'a FixedBytes<32>,
    pub payload: &'a [u8],
    pub tx: CosmosTxContext<'a>,
    pub cosm_gateway: &'a str,
    pub voting_verifier: &'a str,
    pub axelarnet_gateway: &'a str,
}

/// Relay a message through the Amplifier pipeline: verify → poll → route → execute on hub.
pub(super) async fn relay_to_hub(request: HubRelayRequest<'_>) -> Result<()> {
    let HubRelayRequest {
        axelar_id,
        message_id,
        source_address,
        destination_chain,
        destination_address,
        payload_hash,
        payload,
        tx,
        cosm_gateway,
        voting_verifier,
        axelarnet_gateway,
    } = request;
    let msg = json!({
        "cc_id": {
            "message_id": message_id,
            "source_chain": axelar_id,
        },
        "destination_chain": destination_chain,
        "destination_address": destination_address,
        "source_address": source_address,
        "payload_hash": alloy::hex::encode(payload_hash.as_slice()),
    });

    ui::info("verify_messages...");
    let poll_id = submit_verify_messages_amplifier(&msg, tx, cosm_gateway).await?;

    if let Some(poll_id) = poll_id {
        ui::kv("poll_id", &poll_id);
        wait_for_poll_votes(tx.lcd, voting_verifier, &poll_id).await?;
        end_poll_with_retry(&poll_id, tx, voting_verifier).await?;
    } else {
        ui::info("no new poll — already being verified by active verifiers");
    }

    ui::info("route_messages...");
    route_messages_with_retry(&msg, tx, cosm_gateway).await?;

    ui::info("execute on AxelarnetGateway...");
    execute_on_axelarnet_gateway(
        message_id,
        axelar_id,
        destination_chain,
        payload,
        tx,
        axelarnet_gateway,
    )
    .await?;

    Ok(())
}

pub(super) struct DestinationRelayRequest<'a, P> {
    pub first_leg_message_id: &'a str,
    pub src_axelar_id: &'a crate::types::ChainAxelarId,
    pub dest_payload: &'a [u8],
    pub dst_its_proxy: Address,
    pub dst_evm_gateway: Address,
    pub dst_provider: &'a P,
    pub tx: CosmosTxContext<'a>,
    pub dst_cosm_gateway: &'a str,
    pub dst_multisig_prover: &'a str,
    pub axelarnet_gateway: &'a str,
    pub axelar_rpc: &'a str,
    pub step_base: usize,
    pub step_total: usize,
}

async fn wait_for_hub_approval(lcd: &str, gateway: &str, source_chain: &str, message_id: &str) {
    let spinner = ui::wait_spinner("Polling hub for approval...");
    for attempt in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
        if attempt > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        if check_hub_approved(lcd, gateway, source_chain, message_id)
            .await
            .unwrap_or(false)
        {
            spinner.finish_and_clear();
            ui::success("hub approved first-leg message");
            return;
        }
        spinner.set_message(format!(
            "Waiting for hub approval (attempt {}/60)...",
            attempt + 1
        ));
    }
    spinner.finish_and_clear();
    ui::warn(
        "hub never reported the message as approved — proceeding anyway since it may have already been forwarded",
    );
}

fn validate_second_leg_payload(payload: &[u8], second_leg: &SecondLegInfo) -> Result<()> {
    let local_hash = alloy::hex::encode(keccak256(payload).as_slice());
    let expected_hash = second_leg
        .payload_hash
        .strip_prefix("0x")
        .unwrap_or(&second_leg.payload_hash)
        .to_lowercase();
    if local_hash != expected_hash {
        ui::warn("payload hash mismatch between local reconstruction and hub event:");
        ui::warn(&format!("  local:    0x{local_hash}"));
        ui::warn(&format!("  expected: 0x{expected_hash}"));
        return Err(eyre::eyre!(
            "payload hash mismatch — would cause ITS.execute to revert"
        ));
    }
    ui::success("payload hash matches second-leg event");
    Ok(())
}

async fn wait_for_destination_route(lcd: &str, gateway: &str, message_id: &str) -> Result<()> {
    let spinner = ui::wait_spinner("Polling destination cosm gateway...");
    for attempt in 0..AMPLIFIER_POLL_ATTEMPTS_10MIN {
        if attempt > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        if check_cosmos_routed(lcd, gateway, crate::types::HubChain::NAME, message_id)
            .await
            .unwrap_or(false)
        {
            spinner.finish_and_clear();
            ui::success("destination cosm gateway has the message");
            return Ok(());
        }
        spinner.set_message(format!(
            "Waiting for routing (attempt {}/120)...",
            attempt + 1
        ));
    }
    spinner.finish_and_clear();
    Err(eyre::eyre!(
        "destination cosm gateway never received second-leg message"
    ))
}

async fn construct_destination_proof(
    tx: CosmosTxContext<'_>,
    prover: &str,
    message_id: &str,
    step_base: usize,
    step_total: usize,
) -> Result<Vec<u8>> {
    ui::step_header(
        step_base + 3,
        step_total,
        "construct_proof on dest MultisigProver",
    );
    let message = json!({
        "construct_proof": [{
            "source_chain": crate::types::HubChain::NAME,
            "message_id": message_id,
        }]
    });
    let execute = build_execute_msg_any(tx.axelar_address, prover, &message)?;
    let response = sign_and_broadcast_cosmos_tx(
        tx.signing_key,
        tx.axelar_address,
        tx.lcd,
        tx.chain_id,
        tx.fee_denom,
        tx.gas_price,
        vec![execute],
    )
    .await?;
    let session_id = extract_event_attr(&response, "multisig_session_id")?;
    ui::kv("multisig_session_id", &session_id);

    ui::step_header(step_base + 4, step_total, "Wait for proof signing");
    let proof = wait_for_proof(tx.lcd, prover, &session_id).await?;
    ui::success("proof ready");
    let execute_data = proof["status"]["completed"]["execute_data"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no execute_data in proof response"))?;
    Ok(alloy::hex::decode(execute_data)?)
}

struct DestinationExecutionRequest<'a, P> {
    provider: &'a P,
    gateway: Address,
    its_proxy: Address,
    second_leg: &'a SecondLegInfo,
    execute_data: Vec<u8>,
    dest_payload: &'a [u8],
    step_base: usize,
    step_total: usize,
}

async fn approve_and_execute_destination<P: Provider>(
    request: DestinationExecutionRequest<'_, P>,
) -> Result<()> {
    let DestinationExecutionRequest {
        provider,
        gateway,
        its_proxy,
        second_leg,
        execute_data,
        dest_payload,
        step_base,
        step_total,
    } = request;
    ui::step_header(
        step_base + 5,
        step_total,
        "Submit proof to dest EVM gateway",
    );
    let approve = TransactionRequest::default()
        .to(gateway)
        .input(Bytes::from(execute_data).into());
    let pending = provider.send_transaction(approve).await?;
    crate::evm::broadcast_and_log(pending, "evm approve tx").await?;

    let command_id = keccak256(format!("axelar_{}", second_leg.message_id).as_bytes());
    ui::kv("commandId", &format!("{command_id}"));
    let approved = AxelarAmplifierGateway::new(gateway, provider)
        .isContractCallApproved(
            command_id,
            crate::types::HubChain::NAME.to_string(),
            second_leg.source_address.clone(),
            its_proxy,
            keccak256(dest_payload),
        )
        .call()
        .await?;
    ui::kv("isContractCallApproved", &format!("{approved}"));
    if !approved {
        return Err(eyre::eyre!(
            "gateway says message not approved for ITS proxy + hub source — check source_address case / encoding"
        ));
    }

    ui::step_header(step_base + 6, step_total, "Execute on destination ITS");
    let pending = InterchainTokenService::new(its_proxy, provider)
        .execute(
            command_id,
            crate::types::HubChain::NAME.to_string(),
            second_leg.source_address.clone(),
            Bytes::copy_from_slice(dest_payload),
        )
        .send()
        .await?;
    crate::evm::broadcast_and_log(pending, "its execute tx").await?;
    Ok(())
}

/// Drive the second leg actively: wait for hub-routed message → discover its
/// cc_id → wait for the destination cosm gateway to have it → construct_proof
/// on the destination MultisigProver → submit to the EVM gateway →
/// `ITS.execute(...)` on the destination ITS proxy.
pub(super) async fn relay_to_destination<P: Provider>(
    request: DestinationRelayRequest<'_, P>,
) -> Result<()> {
    let DestinationRelayRequest {
        first_leg_message_id,
        src_axelar_id,
        dest_payload,
        dst_its_proxy,
        dst_evm_gateway,
        dst_provider,
        tx,
        dst_cosm_gateway,
        dst_multisig_prover,
        axelarnet_gateway,
        axelar_rpc,
        step_base,
        step_total,
    } = request;
    // Wait until the AxelarnetGateway hub has approved the first-leg message.
    // executable_messages is keyed by the *source* chain of the message.
    ui::step_header(step_base, step_total, "Wait for hub approval");
    wait_for_hub_approval(
        tx.lcd,
        axelarnet_gateway,
        src_axelar_id.as_str(),
        first_leg_message_id,
    )
    .await;

    // Discover the second-leg message_id
    ui::step_header(step_base + 1, step_total, "Discover second-leg cc_id");
    let spinner = ui::wait_spinner("Searching tendermint for hub-execute tx...");
    let second_leg = loop_discover_second_leg(axelar_rpc, first_leg_message_id, &spinner).await?;
    spinner.finish_and_clear();
    ui::kv("second-leg message_id", &second_leg.message_id);
    ui::kv("second-leg source_chain", &second_leg.source_chain);
    ui::kv(
        "second-leg destination_chain",
        &second_leg.destination_chain,
    );
    ui::kv("second-leg source_address", &second_leg.source_address);
    ui::kv(
        "second-leg destination_address",
        &second_leg.destination_address,
    );
    ui::kv("second-leg payload_hash", &second_leg.payload_hash);

    // Sanity-check our reconstruction
    validate_second_leg_payload(dest_payload, &second_leg)?;

    // Wait until the destination cosmos Gateway has the outgoing message
    ui::step_header(
        step_base + 2,
        step_total,
        "Wait for destination cosmos gateway to publish",
    );
    wait_for_destination_route(tx.lcd, dst_cosm_gateway, &second_leg.message_id).await?;

    let execute_data = construct_destination_proof(
        tx,
        dst_multisig_prover,
        &second_leg.message_id,
        step_base,
        step_total,
    )
    .await?;
    approve_and_execute_destination(DestinationExecutionRequest {
        provider: dst_provider,
        gateway: dst_evm_gateway,
        its_proxy: dst_its_proxy,
        second_leg: &second_leg,
        execute_data,
        dest_payload,
        step_base,
        step_total,
    })
    .await?;

    Ok(())
}

/// Poll `discover_second_leg` until it returns Some, with a spinner.
async fn loop_discover_second_leg(
    axelar_rpc: &str,
    first_leg_message_id: &str,
    spinner: &indicatif::ProgressBar,
) -> Result<SecondLegInfo> {
    for i in 0..AMPLIFIER_POLL_ATTEMPTS_5MIN {
        if i > 0 {
            tokio::time::sleep(AMPLIFIER_POLL_INTERVAL).await;
        }
        if let Some(info) = discover_second_leg(axelar_rpc, first_leg_message_id).await? {
            return Ok(info);
        }
        spinner.set_message(format!(
            "Searching for hub-execute tx (attempt {}/60)...",
            i + 1
        ));
    }
    Err(eyre::eyre!(
        "could not discover second-leg cc_id after 5 minutes"
    ))
}
