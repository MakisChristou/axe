//! Solana gateway interactions: the source-side `call_contract` send path,
//! the destination-side manual approval flow (init session → verify
//! signatures → approve → execute), and the parsing of `CallContractEvent`
//! emitted by the Anchor `emit_cpi!` macro.

use std::time::Instant;

use anchor_lang::{Discriminator, InstructionData};
use eyre::Result;
use solana_axelar_std::execute_data::{ExecuteData, MerklizedPayload};
use solana_axelar_std::{MerklizedMessage, PayloadType, SigningVerifierSetInfo};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use solana_transaction_status::{
    UiInnerInstructions, UiInstruction, option_serializer::OptionSerializer,
};

use super::rpc::{
    fetch_confirmed_tx, fetch_tx_details, is_transient_solana, retry_blocking, rpc_client,
};
use crate::commands::load_test::metrics::{TxMetrics, TxOutcome};
use crate::types::Network;
use crate::ui;

/// Lamports attached as `PayGas` for a sol→`destination_chain` GMP call.
///
/// Testnet observation (Axelarscan): sol→sol consistently spends ~0.0002 SOL
/// on relayer bookkeeping, so 0.0005 SOL is ~2.5× headroom. Other destinations
/// keep the historical 0.01 SOL until we confirm a smaller amount is safe on
/// mainnet, where destination-execution cost is converted via price oracles
/// and can push the required SOL payment higher than testnet shows.
pub fn pay_gas_lamports(destination_chain: &str) -> u64 {
    // Match every env's Solana axelar id: "solana" (mainnet/testnet),
    // "solana-stagenet-3" (stagenet), "solana-18" (devnet).
    if destination_chain.starts_with("solana") {
        500_000
    } else {
        10_000_000
    }
}

/// Send a call_contract instruction to the Solana Axelar Gateway.
/// Returns the transaction signature and per-tx metrics.
pub fn send_call_contract(
    rpc_url: &str,
    keypair: &dyn Signer,
    network: Network,
    destination_chain: &str,
    destination_address: &str,
    payload: &[u8],
) -> Result<(String, TxMetrics)> {
    let submit_start = Instant::now();
    let rpc_client = rpc_client(rpc_url);
    let gateway_id = network.solana_gateway_id();

    let gateway_config_pda = Pubkey::find_program_address(
        &[solana_axelar_gateway::GatewayConfig::SEED_PREFIX],
        &gateway_id,
    )
    .0;
    let (event_authority_pda, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &gateway_id);

    let ix_data = solana_axelar_gateway::instruction::CallContract {
        destination_chain: destination_chain.to_string(),
        destination_contract_address: destination_address.to_string(),
        payload: payload.to_vec(),
        signing_pda_bump: 0,
    }
    .data();

    let fee_payer = keypair.pubkey();
    let accounts = vec![
        AccountMeta::new(fee_payer, true),
        AccountMeta::new(fee_payer, true),
        AccountMeta::new_readonly(gateway_config_pda, false),
        AccountMeta::new_readonly(event_authority_pda, false),
        AccountMeta::new_readonly(gateway_id, false),
    ];

    let call_contract_ix = Instruction {
        program_id: gateway_id,
        accounts,
        data: ix_data,
    };

    // Pay gas on non-devnet environments so the relayer picks up the message.
    let instructions = match network {
        Network::DevnetAmplifier => vec![call_contract_ix],
        _ => {
            let pay_gas_ix = build_pay_gas_instruction(
                network,
                fee_payer,
                destination_chain,
                destination_address,
                payload,
            );
            vec![pay_gas_ix, call_contract_ix]
        }
    };

    let blockhash = retry_blocking(
        "solana get_latest_blockhash",
        |_| true,
        || rpc_client.get_latest_blockhash(),
    )?;
    let message = Message::new_with_blockhash(&instructions, Some(&fee_payer), &blockhash);
    let mut transaction = Transaction::new_unsigned(message);
    transaction.sign(&[keypair], blockhash);

    let submit_time_ms = submit_start.elapsed().as_millis() as u64;

    // Same signed tx on every attempt — dedup by signature makes this safe.
    let signature = retry_blocking("solana submit call_contract", is_transient_solana, || {
        rpc_client.send_and_confirm_transaction(&transaction)
    })?;

    let confirm_time_ms = submit_start.elapsed().as_millis() as u64;
    let latency_ms = confirm_time_ms.saturating_sub(submit_time_ms);

    let (compute_units, slot) = fetch_tx_details(&rpc_client, &signature).unwrap_or((None, None));

    let metrics = TxMetrics {
        signature: signature.to_string(),
        submit_time_ms,
        confirm_time_ms: Some(confirm_time_ms),
        latency_ms: Some(latency_ms),
        compute_units,
        slot,
        outcome: TxOutcome::Succeeded,
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        payload: Vec::new(),
        send_instant: None,
        amplifier_timing: None,
    };

    Ok((signature.to_string(), metrics))
}

/// Build the gas-service `PayGas` instruction prepended to `call_contract`
/// on networks whose relayer requires gas payment (everything but
/// devnet-amplifier).
fn build_pay_gas_instruction(
    network: Network,
    fee_payer: Pubkey,
    destination_chain: &str,
    destination_address: &str,
    payload: &[u8],
) -> Instruction {
    let gas_service_id = network.solana_gas_service_id();
    let payload_hash: [u8; 32] = solana_sdk::keccak::hash(payload).to_bytes();
    let treasury_pda = Pubkey::find_program_address(
        &[solana_axelar_gas_service::Treasury::SEED_PREFIX],
        &gas_service_id,
    )
    .0;
    let gas_event_authority =
        Pubkey::find_program_address(&[b"__event_authority"], &gas_service_id).0;

    let pay_gas_data = solana_axelar_gas_service::instruction::PayGas {
        destination_chain: destination_chain.to_string(),
        destination_address: destination_address.to_string(),
        payload_hash,
        amount: pay_gas_lamports(destination_chain),
        refund_address: fee_payer,
    }
    .data();

    Instruction {
        program_id: gas_service_id,
        accounts: vec![
            AccountMeta::new(fee_payer, true),
            AccountMeta::new(treasury_pda, false),
            AccountMeta::new_readonly(
                Pubkey::from_str_const("11111111111111111111111111111111"),
                false,
            ),
            AccountMeta::new_readonly(gas_event_authority, false),
            AccountMeta::new_readonly(gas_service_id, false),
        ],
        data: pay_gas_data,
    }
}

/// 1-based instruction index of `call_contract` in the transaction.
/// When a `pay_gas` instruction is prepended (non-devnet), call_contract is at index 2.
pub fn solana_call_contract_index(network: Network) -> u8 {
    match network {
        Network::DevnetAmplifier => 1,
        _ => 2,
    }
}

/// Decoded Solana gateway `CallContractEvent` extracted from a confirmed tx.
/// `sender` is base58 (the caller program ID for ITS-routed messages).
pub struct CallContractEventInfo {
    pub sender: String,
    pub destination_chain: String,
    pub destination_address: String,
    pub payload_hash: [u8; 32],
    pub payload: Vec<u8>,
}

/// Find the gateway-emitted CallContractEvent in a Solana tx and return its
/// fields. Anchor `emit_cpi!` writes events to the program's event-authority
/// via a CPI whose instruction `data` is
/// `[8-byte EVENT_IX_TAG_LE || 8-byte event-disc || borsh(event)]`.
pub fn extract_gateway_call_contract_payload(
    rpc_url: &str,
    signature_str: &str,
) -> Result<CallContractEventInfo> {
    let rpc_client = rpc_client(rpc_url);
    let sig: Signature = signature_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid signature: {e}"))?;
    let tx = fetch_confirmed_tx(&rpc_client, &sig)?
        .ok_or_else(|| eyre::eyre!("could not fetch transaction {signature_str}"))?;
    let meta = tx
        .transaction
        .meta
        .ok_or_else(|| eyre::eyre!("transaction has no metadata"))?;

    let inner_lists: Vec<UiInnerInstructions> = match meta.inner_instructions {
        OptionSerializer::Some(v) => v,
        _ => return Err(eyre::eyre!("transaction has no inner_instructions")),
    };

    let want_disc = solana_axelar_gateway::events::CallContractEvent::DISCRIMINATOR;

    for ii in &inner_lists {
        for (inner_pos, inst) in ii.instructions.iter().enumerate() {
            let UiInstruction::Compiled(ix) = inst else {
                continue;
            };
            let data = bs58::decode(&ix.data)
                .into_vec()
                .map_err(|e| eyre::eyre!("inner instruction data not valid base58: {e}"))?;
            if data.len() < 16
                || data[..8] != *anchor_lang::event::EVENT_IX_TAG_LE
                || &data[8..16] != want_disc
            {
                continue;
            }
            let event: solana_axelar_gateway::events::CallContractEvent =
                borsh::BorshDeserialize::try_from_slice(&data[16..])
                    .map_err(|e| eyre::eyre!("decode CallContractEvent failed: {e}"))?;
            let _ = inner_pos;
            return Ok(CallContractEventInfo {
                sender: event.sender.to_string(),
                destination_chain: event.destination_chain,
                destination_address: event.destination_contract_address,
                payload_hash: event.payload_hash,
                payload: event.payload,
            });
        }
    }

    Err(eyre::eyre!(
        "no gateway CallContractEvent found in inner instructions"
    ))
}

/// Extract the ITS gateway message ID from a confirmed transaction.
///
/// ITS instructions invoke the gateway's `call_contract` via CPI. The Solana
/// message ID format is `{signature}-{top_ix}.{inner_ix}` where `inner_ix` is
/// the index of the gateway CPI in the flattened inner-instruction list.
///
/// The inner index varies depending on whether the ITS program calls `pay_gas`
/// before `call_contract` (gas_value > 0 adds extra CPIs), and on the
/// program version / instruction layout.
///
/// We parse the transaction logs and count every CPI invocation (depth >= 2)
/// sequentially. Each such invocation corresponds to one entry in the
/// inner_instructions array. We find the last gateway invoke, which is the
/// `call_contract` that emits the `CallContractEvent`.
pub fn extract_its_message_id(
    rpc_url: &str,
    network: Network,
    signature_str: &str,
) -> Result<String> {
    let rpc_client = rpc_client(rpc_url);
    let sig: Signature = signature_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid signature: {e}"))?;

    let tx = fetch_confirmed_tx(&rpc_client, &sig)?
        .ok_or_else(|| eyre::eyre!("could not fetch transaction {signature_str}"))?;

    let meta = tx
        .transaction
        .meta
        .ok_or_else(|| eyre::eyre!("transaction has no metadata"))?;

    let logs: Vec<String> = match meta.log_messages {
        solana_transaction_status::option_serializer::OptionSerializer::Some(logs) => logs,
        _ => return Err(eyre::eyre!("transaction has no log messages")),
    };

    let gateway_id = network.solana_gateway_id().to_string();

    // Parse logs to find which top-level instruction invoked the gateway and
    // which inner CPI within that instruction is the event emit.
    //
    // Log pattern:
    //   "Program X invoke [1]"  → top-level instruction start (depth 1)
    //   "Program X invoke [N]"  → CPI at depth N (inner instruction)
    //   "Program X success"     → instruction end
    //
    // We track which top-level instruction we're in (1-based group index)
    // and count inner CPIs within each group separately.
    let mut top_level_index: u32 = 0; // 1-based
    let mut inner_ix_counter: u32 = 0; // 0-based within current group
    let mut found_group: Option<u32> = None;
    let mut found_inner_idx: Option<u32> = None;

    for log in &logs {
        if let Some(rest) = log.strip_prefix("Program ") {
            if rest.ends_with(" invoke [1]") {
                // New top-level instruction
                top_level_index += 1;
                inner_ix_counter = 0;
            } else if rest.contains(" invoke [") {
                // Inner CPI
                if rest.split(" invoke [").next() == Some(gateway_id.as_str()) {
                    found_group = Some(top_level_index);
                    // The emit_cpi! after this invoke adds one more inner instruction
                    // that doesn't produce an "invoke" log. The message ID references
                    // that emit instruction, so we add 1.
                    found_inner_idx = Some(inner_ix_counter + 1);
                }
                inner_ix_counter += 1;
            }
        }
    }

    match (found_group, found_inner_idx) {
        (Some(group), Some(idx)) => {
            // Convert to 1-based indexing for the message ID format
            Ok(format!("{signature_str}-{group}.{idx}"))
        }
        _ => Err(eyre::eyre!(
            "could not find gateway call_contract CPI in transaction logs"
        )),
    }
}

// ---------------------------------------------------------------------------
// Manual destination-side approval flow
// ---------------------------------------------------------------------------

/// Deserialize the execute_data hex string from a Cosmos proof response
/// into the Solana `ExecuteData` struct.
pub fn decode_execute_data(execute_data_hex: &str) -> Result<ExecuteData> {
    let bytes = hex::decode(execute_data_hex)?;
    let execute_data: ExecuteData = borsh::BorshDeserialize::try_from_slice(&bytes)?;
    Ok(execute_data)
}

/// Step 7a: Initialize a payload verification session on the Solana gateway.
/// This creates the session PDA that tracks signature verification progress.
fn initialize_verification_session(
    rpc_url: &str,
    payer: &Keypair,
    network: Network,
    payload_merkle_root: [u8; 32],
    signing_verifier_set_merkle_root: [u8; 32],
) -> Result<Signature> {
    let rpc = rpc_client(rpc_url);
    let gateway_id = network.solana_gateway_id();

    let gateway_config_pda = Pubkey::find_program_address(
        &[solana_axelar_gateway::GatewayConfig::SEED_PREFIX],
        &gateway_id,
    )
    .0;

    // Derive VerifierSetTracker PDA
    let (verifier_set_tracker_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::VerifierSetTracker::SEED_PREFIX,
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    // Derive verification session PDA
    let payload_type_byte: u8 = PayloadType::ApproveMessages.into();
    let (verification_session_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::SignatureVerificationSessionData::SEED_PREFIX,
            &payload_merkle_root,
            &[payload_type_byte],
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let ix_data = solana_axelar_gateway::instruction::InitializePayloadVerificationSession {
        merkle_root: payload_merkle_root,
        payload_type: PayloadType::ApproveMessages,
    };

    let ix = Instruction {
        program_id: gateway_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new(verification_session_pda, false),
            AccountMeta::new_readonly(verifier_set_tracker_pda, false),
            AccountMeta::new_readonly(anchor_lang::prelude::system_program::ID, false),
        ],
        data: ix_data.data(),
    };

    let recent_blockhash = retry_blocking(
        "solana get_latest_blockhash",
        |_| true,
        || rpc.get_latest_blockhash(),
    )?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    // Same signed tx on every attempt — dedup by signature makes this safe.
    let sig = retry_blocking("solana submit init_session", is_transient_solana, || {
        rpc.send_and_confirm_transaction_with_spinner(&tx)
    })?;
    Ok(sig)
}

/// Step 7b: Verify a single signature against the verification session.
/// Called once per signer. Can be called in parallel.
fn verify_signature(
    rpc_url: &str,
    payer: &Keypair,
    network: Network,
    payload_merkle_root: [u8; 32],
    signing_verifier_set_merkle_root: [u8; 32],
    verifier_info: SigningVerifierSetInfo,
) -> Result<Signature> {
    let rpc = rpc_client(rpc_url);
    let gateway_id = network.solana_gateway_id();

    let gateway_config_pda = Pubkey::find_program_address(
        &[solana_axelar_gateway::GatewayConfig::SEED_PREFIX],
        &gateway_id,
    )
    .0;

    let payload_type_byte: u8 = verifier_info.payload_type.into();
    let (verification_session_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::SignatureVerificationSessionData::SEED_PREFIX,
            &payload_merkle_root,
            &[payload_type_byte],
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let (verifier_set_tracker_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::VerifierSetTracker::SEED_PREFIX,
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let ix_data = solana_axelar_gateway::instruction::VerifySignature {
        payload_merkle_root,
        verifier_info,
    };

    let ix = Instruction {
        program_id: gateway_id,
        accounts: vec![
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new(verification_session_pda, false),
            AccountMeta::new_readonly(verifier_set_tracker_pda, false),
        ],
        data: ix_data.data(),
    };

    // SetComputeUnitLimit: program_id = ComputeBudget111..., data = [0x02, limit_u32_le]
    let cu_limit: u32 = 400_000;
    let cu_ix = Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: [&[0x02], cu_limit.to_le_bytes().as_slice()].concat(),
    };

    let recent_blockhash = retry_blocking(
        "solana get_latest_blockhash",
        |_| true,
        || rpc.get_latest_blockhash(),
    )?;
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix, ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    // Same signed tx on every attempt — dedup by signature makes this safe.
    let sig = retry_blocking(
        "solana submit verify_signature",
        is_transient_solana,
        || rpc.send_and_confirm_transaction_with_spinner(&tx),
    )?;
    Ok(sig)
}

/// Step 7c: Approve a message on the Solana gateway after all signatures
/// have been verified. Creates the IncomingMessage PDA.
fn approve_message(
    rpc_url: &str,
    payer: &Keypair,
    network: Network,
    merklized_message: MerklizedMessage,
    payload_merkle_root: [u8; 32],
    signing_verifier_set_merkle_root: [u8; 32],
) -> Result<Signature> {
    let rpc = rpc_client(rpc_url);
    let gateway_id = network.solana_gateway_id();

    let gateway_config_pda = Pubkey::find_program_address(
        &[solana_axelar_gateway::GatewayConfig::SEED_PREFIX],
        &gateway_id,
    )
    .0;

    let command_id = merklized_message.leaf.message.command_id();
    let (incoming_message_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::IncomingMessage::SEED_PREFIX,
            command_id.as_ref(),
        ],
        &gateway_id,
    );

    let payload_type_byte: u8 = PayloadType::ApproveMessages.into();
    let (verification_session_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::SignatureVerificationSessionData::SEED_PREFIX,
            &payload_merkle_root,
            &[payload_type_byte],
            &signing_verifier_set_merkle_root,
        ],
        &gateway_id,
    );

    let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &gateway_id);

    let ix_data = solana_axelar_gateway::instruction::ApproveMessage {
        merklized_message,
        payload_merkle_root,
    };

    let ix = Instruction {
        program_id: gateway_id,
        accounts: vec![
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(verification_session_pda, false),
            AccountMeta::new(incoming_message_pda, false),
            AccountMeta::new_readonly(anchor_lang::prelude::system_program::ID, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(gateway_id, false),
        ],
        data: ix_data.data(),
    };

    let recent_blockhash = retry_blocking(
        "solana get_latest_blockhash",
        |_| true,
        || rpc.get_latest_blockhash(),
    )?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    // Same signed tx on every attempt — dedup by signature makes this safe.
    let sig = retry_blocking("solana submit approve_message", is_transient_solana, || {
        rpc.send_and_confirm_transaction_with_spinner(&tx)
    })?;
    Ok(sig)
}

/// Run the full Solana gateway approval flow:
/// 1. Initialize verification session
/// 2. Verify all signatures
/// 3. Approve the message
pub fn approve_messages_on_gateway(
    rpc_url: &str,
    payer: &Keypair,
    network: Network,
    execute_data: &ExecuteData,
) -> Result<()> {
    // Step 7a: Initialize verification session
    ui::info("initializing payload verification session...");
    match initialize_verification_session(
        rpc_url,
        payer,
        network,
        execute_data.payload_merkle_root,
        execute_data.signing_verifier_set_merkle_root,
    ) {
        Ok(sig) => ui::tx_hash("init session", &sig.to_string()),
        Err(e) => {
            if classify_idempotent_gateway_error(GatewayOperation::InitializeSession, &e)
                == GatewayErrorDisposition::AlreadyApplied
            {
                ui::info("verification session already initialized");
            } else {
                return Err(e);
            }
        }
    }

    // Step 7b: Verify signatures (one per signer)
    let num_signers = execute_data.signing_verifier_set_leaves.len();
    ui::info(&format!("verifying {num_signers} signatures..."));

    for (i, verifier_info) in execute_data.signing_verifier_set_leaves.iter().enumerate() {
        // Normalize recovery ID: Cosmos signers produce 27/28 (Ethereum-style),
        // but Solana's secp256k1_recover expects 0/1.
        let mut info = verifier_info.clone();
        if info.signature.0[64] >= 27 {
            info.signature.0[64] -= 27;
        }
        match verify_signature(
            rpc_url,
            payer,
            network,
            execute_data.payload_merkle_root,
            execute_data.signing_verifier_set_merkle_root,
            info,
        ) {
            Ok(_) => {
                ui::info(&format!("  signature {}/{num_signers} verified", i + 1));
            }
            Err(e) => {
                if classify_idempotent_gateway_error(GatewayOperation::VerifySignature, &e)
                    == GatewayErrorDisposition::AlreadyApplied
                {
                    ui::info(&format!(
                        "  signature {}/{num_signers} already verified",
                        i + 1
                    ));
                } else {
                    return Err(eyre::eyre!("failed to verify signature {}: {e}", i + 1));
                }
            }
        }
    }

    // Step 7c: Approve messages
    let MerklizedPayload::NewMessages { messages } = &execute_data.payload_items else {
        return Err(eyre::eyre!(
            "expected NewMessages payload, got VerifierSetRotation"
        ));
    };

    ui::info(&format!("approving {} message(s)...", messages.len()));
    for (i, merklized_message) in messages.iter().enumerate() {
        match approve_message(
            rpc_url,
            payer,
            network,
            merklized_message.clone(),
            execute_data.payload_merkle_root,
            execute_data.signing_verifier_set_merkle_root,
        ) {
            Ok(sig) => {
                ui::tx_hash(&format!("approve msg {}", i + 1), &sig.to_string());
            }
            Err(e) => {
                if classify_idempotent_gateway_error(GatewayOperation::ApproveMessage, &e)
                    == GatewayErrorDisposition::AlreadyApplied
                {
                    ui::info(&format!("  message {} already approved", i + 1));
                } else {
                    return Err(eyre::eyre!("failed to approve message {}: {e}", i + 1));
                }
            }
        }
    }

    ui::success("all messages approved on Solana gateway");
    Ok(())
}

#[derive(Clone, Copy)]
enum GatewayOperation {
    InitializeSession,
    VerifySignature,
    ApproveMessage,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GatewayErrorDisposition {
    AlreadyApplied,
    Fatal,
}

fn classify_idempotent_gateway_error(
    operation: GatewayOperation,
    error: &eyre::Report,
) -> GatewayErrorDisposition {
    let message = error.to_string();
    let already_applied = match operation {
        GatewayOperation::InitializeSession | GatewayOperation::ApproveMessage => {
            message.contains("already in use")
        }
        // Scoped to the "this slot is already verified" phrasings. A bare
        // "already" would also swallow unrelated runtime failures - Solana
        // reports account contention as "already borrowed" - and treating one
        // of those as success would carry on with a signature that never
        // verified.
        GatewayOperation::VerifySignature => {
            message.contains("SlotAlreadyVerified") || message.contains("already verified")
        }
    };
    if already_applied {
        GatewayErrorDisposition::AlreadyApplied
    } else {
        GatewayErrorDisposition::Fatal
    }
}

/// Execute an approved GMP message on the Memo program.
/// Calls the Memo program's `execute` instruction which validates the message
/// against the gateway and logs the memo payload.
pub fn execute_on_memo(
    rpc_url: &str,
    payer: &Keypair,
    network: Network,
    message: solana_axelar_std::Message,
    payload: &[u8],
) -> Result<Signature> {
    let rpc = rpc_client(rpc_url);
    let gateway_id = network.solana_gateway_id();
    let memo_id = network.solana_memo_id();

    let command_id = message.command_id();

    // AxelarExecuteAccounts
    let (incoming_message_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::IncomingMessage::SEED_PREFIX,
            command_id.as_ref(),
        ],
        &gateway_id,
    );
    let (signing_pda, _) = Pubkey::find_program_address(
        &[
            solana_axelar_gateway::ValidateMessageSigner::SEED_PREFIX,
            command_id.as_ref(),
        ],
        &memo_id,
    );
    let gateway_config_pda = Pubkey::find_program_address(
        &[solana_axelar_gateway::GatewayConfig::SEED_PREFIX],
        &gateway_id,
    )
    .0;
    let (gateway_event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &gateway_id);

    // Counter PDA for the memo program
    let (counter_pda, _) = Pubkey::find_program_address(&[b"counter"], &memo_id);

    // The payload passed to call_contract was AxelarMessagePayload-encoded (ABI scheme).
    // The execute instruction receives the inner payload (memo bytes) and the encoding scheme.
    // We need to decode the AxelarMessagePayload to get the inner payload.
    let decoded = solana_axelar_gateway::payload::AxelarMessagePayload::decode(payload)
        .map_err(|e| eyre::eyre!("failed to decode payload: {e:?}"))?;
    let inner_payload = decoded.payload_without_accounts().to_vec();
    let encoding_scheme = decoded.encoding_scheme();

    let ix_data = solana_axelar_memo::instruction::Execute {
        message,
        payload: inner_payload,
        encoding_scheme,
    }
    .data();

    let ix = Instruction {
        program_id: memo_id,
        accounts: vec![
            // AxelarExecuteAccounts (order from executable_accounts! macro)
            AccountMeta::new(incoming_message_pda, false),
            AccountMeta::new_readonly(signing_pda, false),
            AccountMeta::new_readonly(gateway_config_pda, false),
            AccountMeta::new_readonly(gateway_event_authority, false),
            AccountMeta::new_readonly(gateway_id, false),
            // Memo program accounts
            AccountMeta::new(counter_pda, false),
        ],
        data: ix_data,
    };

    let cu_limit: u32 = 400_000;
    let cu_ix = Instruction {
        program_id: Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
        accounts: vec![],
        data: [&[0x02], cu_limit.to_le_bytes().as_slice()].concat(),
    };

    let recent_blockhash = retry_blocking(
        "solana get_latest_blockhash",
        |_| true,
        || rpc.get_latest_blockhash(),
    )?;
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix, ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    // Same signed tx on every attempt — dedup by signature makes this safe.
    let sig = retry_blocking("solana submit memo_execute", is_transient_solana, || {
        rpc.send_and_confirm_transaction_with_spinner(&tx)
    })?;
    Ok(sig)
}

#[cfg(test)]
mod tests {
    use super::{
        GatewayErrorDisposition, GatewayOperation, classify_idempotent_gateway_error as classify,
    };

    fn err(message: &str) -> eyre::Report {
        eyre::eyre!(message.to_string())
    }

    #[test]
    fn already_verified_slot_is_idempotent() {
        for message in ["SlotAlreadyVerified", "signature already verified"] {
            assert_eq!(
                classify(GatewayOperation::VerifySignature, &err(message)),
                GatewayErrorDisposition::AlreadyApplied,
                "{message} should count as already applied"
            );
        }
    }

    #[test]
    fn unrelated_already_wording_is_still_fatal() {
        // Account contention surfaces as "already borrowed". Swallowing it
        // would continue the approval flow with an unverified signature.
        for message in [
            "Account already borrowed",
            "blockhash already processed",
            "custom program error: 0x1",
        ] {
            assert_eq!(
                classify(GatewayOperation::VerifySignature, &err(message)),
                GatewayErrorDisposition::Fatal,
                "{message} should stay fatal"
            );
        }
    }

    #[test]
    fn session_and_approval_use_account_in_use() {
        assert_eq!(
            classify(
                GatewayOperation::InitializeSession,
                &err("Allocate: account Address { .. } already in use")
            ),
            GatewayErrorDisposition::AlreadyApplied
        );
        assert_eq!(
            classify(GatewayOperation::ApproveMessage, &err("already borrowed")),
            GatewayErrorDisposition::Fatal
        );
    }
}
