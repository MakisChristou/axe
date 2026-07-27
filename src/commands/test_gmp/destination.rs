use alloy::{
    primitives::{Address, B256, Bytes, keccak256},
    providers::Provider,
    rpc::types::TransactionRequest,
};
use eyre::Result;
use solana_axelar_std::{CrossChainId, Message};

use crate::evm::{AxelarAmplifierGateway, SenderReceiver};
use crate::solana::{
    approve_messages_on_gateway, decode_execute_data, execute_on_memo, load_keypair,
};
use crate::ui;

/// Submit the Amplifier-built `execute_data` to the EVM gateway, confirm the
/// message is approved, then call `execute` on the SenderReceiver and read
/// back the stored message. `source_address` is the original sender's
/// address (a SenderReceiver address for the EVM-loopback flow, a Solana
/// keypair pubkey for sol→evm) — the EVM gateway validates the approval
/// against this value, so it must match what was emitted at the source.
pub struct EvmExecutionRequest<'a, P> {
    pub provider: &'a P,
    pub gateway: Address,
    pub sender_receiver: Address,
    pub source_chain: &'a str,
    pub source_address: &'a str,
    pub message_id: &'a str,
    pub execute_data_hex: &'a str,
    pub payload_bytes: &'a [u8],
    pub payload_hash: B256,
    pub step_idx_approve: usize,
    pub step_idx_execute: usize,
    pub total_steps: usize,
}

pub async fn approve_and_execute_evm<P: Provider>(
    request: EvmExecutionRequest<'_, P>,
) -> Result<()> {
    let EvmExecutionRequest {
        provider,
        gateway,
        sender_receiver,
        source_chain,
        source_address,
        message_id,
        execute_data_hex,
        payload_bytes,
        payload_hash,
        step_idx_approve,
        step_idx_execute,
        total_steps,
    } = request;
    ui::step_header(step_idx_approve, total_steps, "Submit proof to EVM gateway");
    let execute_data = alloy::hex::decode(execute_data_hex)?;

    let approve_tx = TransactionRequest::default()
        .to(gateway)
        .input(Bytes::from(execute_data).into());
    let pending_approve = provider.send_transaction(approve_tx).await?;
    let _approve_receipt = crate::evm::broadcast_and_log(pending_approve, "tx").await?;

    // Use the modern Amplifier `isMessageApproved` view — keyed on
    // (sourceChain, messageId, sourceAddress, contractAddress, payloadHash)
    // — instead of the legacy `isContractCallApproved` which needs a
    // commandId we'd have to derive from a non-trivial formula that
    // varies across gateway versions. This is what `load-test` checks too.
    let gw_contract = AxelarAmplifierGateway::new(gateway, provider);
    let approved = gw_contract
        .isMessageApproved(
            source_chain.to_string(),
            message_id.to_string(),
            source_address.to_string(),
            sender_receiver,
            payload_hash,
        )
        .call()
        .await?;
    ui::kv("isMessageApproved", &format!("{approved}"));

    if !approved {
        return Err(eyre::eyre!("message not approved on EVM gateway"));
    }

    // SenderReceiver.execute(commandId, ...) needs the gateway's internal
    // approval key. The Amplifier EVM gateway uses
    // `keccak256(string.concat(sourceChain, "_", messageId))` — underscore
    // separator, *not* the dash that the Solana
    // `solana_axelar_std::Message::command_id()` uses. The Cosmos hub
    // emits both forms, with the EVM-flavoured underscore form serving as
    // the on-chain approval key here.
    ui::step_header(step_idx_execute, total_steps, "Execute on SenderReceiver");
    let command_id_input = {
        let mut buf = Vec::with_capacity(source_chain.len() + 1 + message_id.len());
        buf.extend_from_slice(source_chain.as_bytes());
        buf.push(b'_');
        buf.extend_from_slice(message_id.as_bytes());
        buf
    };
    let command_id = keccak256(&command_id_input);
    ui::kv("commandId", &format!("{command_id}"));

    let sr_contract = SenderReceiver::new(sender_receiver, provider);
    let exec_call = sr_contract.execute(
        command_id,
        source_chain.to_string(),
        source_address.to_string(),
        Bytes::from(payload_bytes.to_vec()),
    );
    match exec_call.send().await {
        Ok(pending_exec) => match crate::evm::broadcast_and_log(pending_exec, "tx").await {
            Ok(_) => match sr_contract.message().call().await {
                Ok(stored_message) => {
                    ui::kv("stored message", &format!("\"{stored_message}\""));
                }
                Err(e) => ui::warn(&format!("could not read stored message: {e}")),
            },
            Err(e) => ui::warn(&format!(
                "execute() reverted (likely commandId-derivation mismatch on this gateway version): {e}",
            )),
        },
        Err(e) => ui::warn(&format!("execute() submission failed: {e}")),
    }

    Ok(())
}

/// Submit the Amplifier-built `execute_data` to the Solana gateway, then
/// call the destination program (memo) with the decoded GMP message and
/// raw payload. Wraps Steps 7-8 of an SVM-destination flow.
#[derive(Clone, Copy)]
pub struct SvmExecutionRequest<'a> {
    pub dst_rpc: &'a str,
    pub network: crate::types::Network,
    pub source_chain: &'a str,
    pub destination_chain: &'a str,
    pub source_address: &'a str,
    pub destination_address: &'a str,
    pub message_id: &'a str,
    pub payload_bytes: &'a [u8],
    pub payload_hash: B256,
    pub execute_data_hex: &'a str,
    pub step_idx_approve: usize,
    pub step_idx_execute: usize,
    pub total_steps: usize,
}

pub fn approve_and_execute_svm(request: SvmExecutionRequest<'_>) -> Result<()> {
    let SvmExecutionRequest {
        dst_rpc,
        network,
        source_chain,
        destination_chain,
        source_address,
        destination_address,
        message_id,
        payload_bytes,
        payload_hash,
        execute_data_hex,
        step_idx_approve,
        step_idx_execute,
        total_steps,
    } = request;
    ui::step_header(step_idx_approve, total_steps, "Approve on Solana gateway");
    let keypair = load_keypair(None)?;
    let execute_data = decode_execute_data(execute_data_hex)?;
    approve_messages_on_gateway(dst_rpc, &keypair, network, &execute_data)?;

    ui::step_header(step_idx_execute, total_steps, "Execute on destination");
    let gmp_message = Message {
        cc_id: CrossChainId {
            chain: source_chain.to_string(),
            id: message_id.to_string(),
        },
        source_address: source_address.to_string(),
        destination_chain: destination_chain.to_string(),
        destination_address: destination_address.to_string(),
        payload_hash: payload_hash.0,
    };

    let memo_sig = execute_on_memo(dst_rpc, &keypair, network, gmp_message, payload_bytes)?;
    ui::tx_hash("execute", &memo_sig.to_string());

    Ok(())
}
