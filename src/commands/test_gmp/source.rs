use alloy::{
    primitives::{Address, B256, keccak256},
    providers::Provider,
    sol_types::{SolEvent, SolValue},
};
use eyre::Result;
use solana_sdk::{pubkey::Pubkey, signer::Signer};
use tokio::task::spawn_blocking;

use crate::commands::load_test::{make_executable_payload, memo_program_id};
use crate::evm::{ContractCall, SenderReceiver};
use crate::solana::{extract_its_message_id, load_keypair, send_call_contract};
use crate::ui;

/// The bits of a freshly sent GMP `callContract` that downstream Amplifier
/// steps need: the routing fields (incl. who sent it), the message id, and
/// both the raw payload (for destination `execute`) and its hash (for the
/// `isContractCallApproved` check).
pub struct SentGmp {
    pub destination_chain: String,
    pub destination_address: String,
    pub source_address: String,
    pub message_id: String,
    pub payload_bytes: Vec<u8>,
    pub payload_hash: B256,
}

/// Step 1 of the GMP smoke test: call `sendMessage` on the SenderReceiver,
/// then locate the ContractCall log in the receipt to derive the message id
/// (`<tx_hash>-<log_index>`). The flow loops back to the same chain, so the
/// destination is `(source_chain, sender_receiver)`.
pub async fn send_evm_call_contract<P: Provider>(
    provider: &P,
    sender_receiver: Address,
    source_chain: &str,
    step_idx: usize,
    total_steps: usize,
) -> Result<SentGmp> {
    let destination_chain = source_chain.to_string();
    let destination_address = format!("{sender_receiver}");
    let message = "hello from axelar evm deployer".to_string();

    ui::step_header(step_idx, total_steps, "Send GMP callContract");
    ui::kv("destination chain", &destination_chain);
    ui::kv("destination address", &destination_address);
    ui::kv("message", &format!("\"{message}\""));

    let contract = SenderReceiver::new(sender_receiver, provider);
    let call = contract
        .sendMessage(
            destination_chain.clone(),
            destination_address.clone(),
            message.clone(),
        )
        .value(crate::types::eth_milli(1)); // 0.001 ETH cross-chain gas budget

    let pending = call.send().await?;
    let tx_hash = *pending.tx_hash();
    let receipt = crate::evm::broadcast_and_log(pending, "tx").await?;

    let event_index = receipt
        .inner
        .logs()
        .iter()
        .enumerate()
        .find_map(|(i, log)| {
            if log.topics().first() == Some(&ContractCall::SIGNATURE_HASH) {
                Some(i)
            } else {
                None
            }
        })
        .ok_or_else(|| eyre::eyre!("ContractCall event not found in receipt logs"))?;

    let payload_bytes = (message,).abi_encode_params();
    let payload_hash = keccak256(&payload_bytes);
    let message_id = format!("{tx_hash:#x}-{event_index}");

    ui::kv("message_id", &message_id);
    ui::kv("payload_hash", &format!("{payload_hash}"));

    Ok(SentGmp {
        source_address: format!("{sender_receiver}"),
        destination_chain,
        destination_address,
        message_id,
        payload_bytes,
        payload_hash,
    })
}

/// Step 1 for an SVM source: call the gateway's `call_contract` from the
/// loaded keypair. With `destination_address = None` the call targets the
/// SVM memo program with an `make_executable_payload` (sol→sol loopback).
/// With `destination_address = Some(addr)` the call targets that address
/// (typically a `SenderReceiver` deployed on an EVM destination) with an
/// ABI-encoded `(string,)` payload the EVM contract can decode.
///
/// The message id comes back from the gateway log via
/// `extract_its_message_id`, falling back to `<sig>-1.1` if the log isn't
/// indexable yet.
pub async fn send_svm_call_contract(
    src_rpc: &str,
    network: crate::types::Network,
    destination_chain: &str,
    destination_address: Option<&str>,
    step_idx: usize,
    total_steps: usize,
) -> Result<SentGmp> {
    let keypair = load_keypair(None).await?;

    let (destination_address, payload_bytes) = match destination_address {
        None => {
            let memo_program = memo_program_id(network);
            let counter_pda = Pubkey::find_program_address(&[b"counter"], &memo_program).0;
            let payload = make_executable_payload(&None, &counter_pda);
            (memo_program.to_string(), payload)
        }
        Some(addr) => {
            // ABI-encoded `(string,)` so an EVM SenderReceiver can decode it
            // back via `abi.decode(payload, (string))` in `_execute(...)`.
            let message = "hello from solana mainnet manual gmp test".to_string();
            let payload = (message,).abi_encode_params();
            (addr.to_string(), payload)
        }
    };
    let payload_hash = keccak256(&payload_bytes);

    ui::step_header(step_idx, total_steps, "Send callContract");
    ui::kv("destination address", &destination_address);

    let source_address = keypair.pubkey().to_string();
    let source_rpc = src_rpc.to_string();
    let destination_chain = destination_chain.to_string();
    let send_destination_chain = destination_chain.clone();
    let send_destination = destination_address.clone();
    let send_payload = payload_bytes.clone();
    let (metrics, message_id) = spawn_blocking(move || {
        let (_, metrics) = send_call_contract(
            &source_rpc,
            &keypair,
            network,
            &send_destination_chain,
            &send_destination,
            &send_payload,
        )?;
        let raw_sig = metrics.signature.clone();
        let message_id = extract_its_message_id(&source_rpc, network, &raw_sig)
            .unwrap_or_else(|_| format!("{raw_sig}-1.1"));

        Ok::<_, eyre::Report>((metrics, message_id))
    })
    .await??;
    let raw_sig = metrics.signature.clone();

    ui::tx_hash("tx", &raw_sig);
    ui::kv("message_id", &message_id);
    ui::kv("payload_hash", &alloy::hex::encode(payload_hash));
    ui::success(&format!(
        "finalized ({}ms)",
        metrics.latency_ms.unwrap_or(0)
    ));

    // `send_call_contract` already waits for finalized commitment (see
    // `crate::solana::send_call_contract`'s `RpcClient::new_with_commitment(_,
    // CommitmentConfig::finalized())`), so by the time we reach this point
    // the source tx is rollback-proof and verifiers reading at finalized
    // see consistent state. No separate finality barrier needed.

    Ok(SentGmp {
        destination_chain,
        destination_address,
        source_address,
        message_id,
        payload_bytes,
        payload_hash,
    })
}
