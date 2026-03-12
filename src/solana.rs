use anchor_lang::InstructionData;
use eyre::Result;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::time::Instant;

use crate::commands::load_test::metrics::TxMetrics;

/// Load a Solana keypair from a file path, or fall back to ~/.config/solana/id.json.
pub fn load_keypair(path: Option<&str>) -> Result<Keypair> {
    let key_path = match path {
        Some(p) => p.to_string(),
        None => {
            let home =
                dirs::home_dir().ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
            home.join(".config/solana/id.json")
                .to_string_lossy()
                .into_owned()
        }
    };
    read_keypair_file(&key_path)
        .map_err(|e| eyre::eyre!("failed to read Solana keypair from {key_path}: {e}"))
}

/// Send a call_contract instruction to the Solana Axelar Gateway.
/// Returns the transaction signature and per-tx metrics.
pub fn send_call_contract(
    rpc_url: &str,
    keypair: &dyn Signer,
    destination_chain: &str,
    destination_address: &str,
    payload: &[u8],
) -> Result<(String, TxMetrics)> {
    let submit_start = Instant::now();
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let gateway_config_pda = solana_axelar_gateway::GatewayConfig::find_pda().0;
    let (event_authority_pda, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &solana_axelar_gateway::id());

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
        AccountMeta::new_readonly(solana_axelar_gateway::id(), false),
    ];

    let call_contract_ix = Instruction {
        program_id: solana_axelar_gateway::id(),
        accounts,
        data: ix_data,
    };

    // Pay gas on non-devnet environments so the relayer picks up the message.
    #[cfg(not(feature = "devnet-amplifier"))]
    let pay_gas_ix = {
        let payload_hash: [u8; 32] = solana_sdk::keccak::hash(payload).to_bytes();
        let treasury_pda = solana_axelar_gas_service::Treasury::find_pda().0;
        let gas_event_authority = Pubkey::find_program_address(
            &[b"__event_authority"],
            &solana_axelar_gas_service::id(),
        )
        .0;

        let pay_gas_data = solana_axelar_gas_service::instruction::PayGas {
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            payload_hash,
            amount: 100_000,
            refund_address: fee_payer,
        }
        .data();

        Instruction {
            program_id: solana_axelar_gas_service::id(),
            accounts: vec![
                AccountMeta::new(fee_payer, true),
                AccountMeta::new(treasury_pda, false),
                AccountMeta::new_readonly(
                    Pubkey::from_str_const("11111111111111111111111111111111"),
                    false,
                ),
                AccountMeta::new_readonly(gas_event_authority, false),
                AccountMeta::new_readonly(solana_axelar_gas_service::id(), false),
            ],
            data: pay_gas_data,
        }
    };

    #[cfg(not(feature = "devnet-amplifier"))]
    let instructions = vec![pay_gas_ix, call_contract_ix];
    #[cfg(feature = "devnet-amplifier")]
    let instructions = vec![call_contract_ix];

    let blockhash = rpc_client.get_latest_blockhash()?;
    let message = Message::new_with_blockhash(&instructions, Some(&fee_payer), &blockhash);
    let mut transaction = Transaction::new_unsigned(message);
    transaction.sign(&[keypair], blockhash);

    #[allow(clippy::cast_possible_truncation)]
    let submit_time_ms = submit_start.elapsed().as_millis() as u64;

    let signature = rpc_client.send_and_confirm_transaction(&transaction)?;

    #[allow(clippy::cast_possible_truncation)]
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
        success: true,
        error: None,
        payload_hash: String::new(),
        source_address: String::new(),
        payload: Vec::new(),
        send_instant: None,
        amplifier_timing: None,
    };

    Ok((signature.to_string(), metrics))
}

/// 1-based instruction index of `call_contract` in the transaction.
/// When a `pay_gas` instruction is prepended (non-devnet), call_contract is at index 2.
pub fn solana_call_contract_index() -> u8 {
    #[cfg(not(feature = "devnet-amplifier"))]
    { 2 }
    #[cfg(feature = "devnet-amplifier")]
    { 1 }
}

fn fetch_tx_details(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<(Option<u64>, Option<u64>)> {
    // Transaction details may not be immediately available after confirmation.
    // Retry a few times with a short delay.
    for _ in 0..3 {
        match rpc_client.get_transaction(signature, UiTransactionEncoding::Json) {
            Ok(tx) => {
                let slot = Some(tx.slot);
                let compute_units = tx
                    .transaction
                    .meta
                    .and_then(|m| Option::from(m.compute_units_consumed));
                return Ok((compute_units, slot));
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    Ok((None, None))
}
