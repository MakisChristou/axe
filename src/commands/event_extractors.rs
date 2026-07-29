//! EVM transaction-receipt event extractors shared across the test and
//! load-test commands. Pure log-parsing — no I/O, no UI.

use alloy::primitives::{Address, FixedBytes, U256, keccak256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::{SolEvent, SolValue};
use eyre::Result;

use crate::evm::{ContractCall, InterchainTokenDeployed};

/// Generate a random 32-byte salt using the wall-clock nanos. Used by the ITS
/// tests so each run gets a fresh tokenId without collisions.
pub fn generate_salt() -> FixedBytes<32> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let encoded = ("its-test", U256::from(nanos)).abi_encode_params();
    keccak256(&encoded)
}

/// Extract tokenId and token address from the InterchainTokenDeployed event
/// in a receipt's logs. Reads topics/data directly to avoid ABI-decode issues
/// with indexed field differences across ITS versions.
pub fn extract_token_deployed_event(
    receipt: &TransactionReceipt,
) -> Result<(FixedBytes<32>, Address)> {
    for log in receipt.inner.logs() {
        if log.topics().first() == Some(&InterchainTokenDeployed::SIGNATURE_HASH) {
            // tokenId is always topics[1] (first indexed param)
            let token_id = *log
                .topics()
                .get(1)
                .ok_or_else(|| eyre::eyre!("InterchainTokenDeployed missing tokenId topic"))?;

            // tokenAddress is the first ABI-encoded field in data (bytes 12..32)
            let data = log.data().data.as_ref();
            if data.len() < 32 {
                return Err(eyre::eyre!(
                    "InterchainTokenDeployed has truncated data: {} bytes, expected ≥ 32 \
                     (token address occupies bytes 12..32)",
                    data.len()
                ));
            }
            return Ok((token_id, Address::from_slice(&data[12..32])));
        }
    }
    Err(eyre::eyre!(
        "InterchainTokenDeployed event not found in receipt logs"
    ))
}

/// Extract ContractCall event data from a transaction receipt.
/// Returns (event_index, payload, payload_hash, destination_chain, destination_address).
pub fn extract_contract_call_event(
    receipt: &TransactionReceipt,
) -> Result<(usize, Vec<u8>, FixedBytes<32>, String, String)> {
    for (i, log) in receipt.inner.logs().iter().enumerate() {
        if log.topics().first() == Some(&ContractCall::SIGNATURE_HASH) {
            let decoded = ContractCall::decode_log(&log.inner)
                .map_err(|e| eyre::eyre!("failed to decode ContractCall event: {e}"))?;
            return Ok((
                i,
                decoded.data.payload.to_vec(),
                decoded.topics().2, // payloadHash is the 3rd topic
                decoded.data.destinationChain,
                decoded.data.destinationContractAddress,
            ));
        }
    }
    Err(eyre::eyre!("ContractCall event not found in receipt logs"))
}
