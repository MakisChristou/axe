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
use std::sync::Arc;
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

/// Derive N keypairs from a BIP39 mnemonic using ed25519 SLIP-0010 derivation.
pub fn derive_keypairs_from_mnemonic(
    mnemonic: &str,
    count: usize,
) -> Result<Vec<Arc<dyn Signer + Send + Sync>>> {
    use solana_sdk::signature::keypair_from_seed;

    let seed = bip39::Mnemonic::parse(mnemonic)
        .map_err(|e| eyre::eyre!("invalid mnemonic: {e}"))?
        .to_seed("");

    let mut keypairs: Vec<Arc<dyn Signer + Send + Sync>> = Vec::with_capacity(count);

    for i in 0..count {
        let path = format!("m/44'/501'/{i}'");
        let derived = derive_key_from_seed(&seed, &path)?;
        let kp = keypair_from_seed(&derived[..32])
            .map_err(|e| eyre::eyre!("failed to create keypair: {e}"))?;
        keypairs.push(Arc::new(kp));
    }

    Ok(keypairs)
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

    let instruction = Instruction {
        program_id: solana_axelar_gateway::id(),
        accounts,
        data: ix_data,
    };

    let blockhash = rpc_client.get_latest_blockhash()?;
    let message = Message::new_with_blockhash(&[instruction], Some(&fee_payer), &blockhash);
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

/// SLIP-0010 ed25519 key derivation from seed.
#[allow(clippy::missing_asserts_for_indexing)]
fn derive_key_from_seed(seed: &[u8], path: &str) -> Result<[u8; 64]> {
    use hmac::{Hmac, Mac};
    use sha2::Sha512;

    let mut hmac =
        Hmac::<Sha512>::new_from_slice(b"ed25519 seed").map_err(|e| eyre::eyre!("{e}"))?;
    hmac.update(seed);
    let result = hmac.finalize();
    let bytes = result.into_bytes();

    let mut key = [0u8; 64];
    key[..32].copy_from_slice(&bytes[..32]);
    key[32..].copy_from_slice(&bytes[32..64]);

    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 && *part == "m" {
            continue;
        }

        let hardened = part.ends_with('\'');
        let index_str = part.trim_end_matches('\'');
        let index: u32 = index_str
            .parse()
            .map_err(|_| eyre::eyre!("invalid derivation path index: {part}"))?;

        #[allow(clippy::arithmetic_side_effects)]
        let child_index = if hardened {
            0x8000_0000 | index
        } else {
            index
        };

        let mut data = Vec::with_capacity(37);
        data.push(0);
        data.extend_from_slice(&key[..32]);
        data.extend_from_slice(&child_index.to_be_bytes());

        let mut hmac =
            Hmac::<Sha512>::new_from_slice(&key[32..64]).map_err(|e| eyre::eyre!("{e}"))?;
        hmac.update(&data);
        let result = hmac.finalize();
        let bytes = result.into_bytes();

        key[..32].copy_from_slice(&bytes[..32]);
        key[32..].copy_from_slice(&bytes[32..64]);
    }

    Ok(key)
}
