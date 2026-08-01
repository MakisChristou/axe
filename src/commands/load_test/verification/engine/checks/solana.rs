//! Solana destination checks.

use eyre::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use crate::types::Network;

/// Solana's getMultipleAccounts supports up to 100 accounts per call.
const SOLANA_BATCH_SIZE: usize = 100;

/// Incoming message account data offset for the status byte.
/// Layout: 8 (discriminator) + 1 (bump) + 1 (signing_pda_bump) + 3 (pad) = 13
const INCOMING_MESSAGE_STATUS_OFFSET: usize = 13;

/// Batch-check Solana incoming message PDAs via `getMultipleAccounts`.
/// Returns `(tx_index, Option<status_byte>)` for each tx.
pub(in super::super) fn batch_check_solana_incoming_messages(
    rpc_client: &RpcClient,
    network: Network,
    txs: &[(usize, [u8; 32])], // (tx_index, command_id)
) -> Result<Vec<(usize, Option<u8>)>> {
    let gateway_id = network.solana_gateway_id();
    let mut results = Vec::with_capacity(txs.len());
    for chunk in txs.chunks(SOLANA_BATCH_SIZE) {
        let pubkeys: Vec<Pubkey> = chunk
            .iter()
            .map(|(_, cmd_id)| {
                Pubkey::find_program_address(&[b"incoming message", cmd_id], &gateway_id).0
            })
            .collect();
        let accounts = rpc_client.get_multiple_accounts(&pubkeys)?;
        if accounts.len() != chunk.len() {
            return Err(eyre::eyre!(
                "Solana getMultipleAccounts returned {} accounts for {} PDAs",
                accounts.len(),
                chunk.len()
            ));
        }
        for (j, maybe_account) in accounts.iter().enumerate() {
            let status = match maybe_account {
                Some(acc) => {
                    if acc.data.len() <= INCOMING_MESSAGE_STATUS_OFFSET {
                        return Err(eyre::eyre!(
                            "IncomingMessage account too small: {} bytes",
                            acc.data.len()
                        ));
                    }
                    Some(acc.data[INCOMING_MESSAGE_STATUS_OFFSET])
                }
                None => None,
            };
            results.push((chunk[j].0, status));
        }
    }
    Ok(results)
}

/// Check the Solana IncomingMessage PDA for a given command_id.
/// Returns `Some(status_byte)` if the account exists, `None` otherwise.
/// Status: 0 = approved, non-zero = executed.
pub(in super::super) fn check_solana_incoming_message(
    rpc_client: &RpcClient,
    network: Network,
    command_id: &[u8; 32],
) -> Result<Option<u8>> {
    let (pda, _bump) = Pubkey::find_program_address(
        &[b"incoming message", command_id],
        &network.solana_gateway_id(),
    );

    match rpc_client.get_account_data(&pda) {
        Ok(data) => {
            if data.len() <= INCOMING_MESSAGE_STATUS_OFFSET {
                return Err(eyre::eyre!(
                    "IncomingMessage account too small: {} bytes",
                    data.len()
                ));
            }
            Ok(Some(data[INCOMING_MESSAGE_STATUS_OFFSET]))
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("AccountNotFound") || err_str.contains("could not find account") {
                Ok(None)
            } else {
                Err(eyre::eyre!("Solana RPC error: {e}"))
            }
        }
    }
}
