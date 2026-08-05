//! Legacy Axelar EVM gateway verification.
//!
//! Consensus chains (e.g. Avalanche) are secured by the EVM-verifier + nexus
//! flow, not Amplifier, so there is no `VotingVerifier`/`MultisigProver` to
//! poll. The destination message status lives entirely on the legacy
//! `AxelarGateway`:
//!
//! - approval emits `ContractCallApproved(commandId, …, payloadHash, sourceTxHash, …)`
//! - execution emits `ContractCallExecuted(commandId)` and flips
//!   `isCommandExecuted(commandId)` to true.
//!
//! We derive the message status directly from the chain: locate the emitted
//! `ContractCallApproved` (matched by the indexed `payloadHash` plus the exact
//! `sourceTxHash`) and take the on-chain `commandId` from it — guaranteed
//! correct, and confirmed to match the canonical GMP-API value.

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use eyre::{Result, eyre};

use crate::evm::ContractCallApproved;

/// Max blocks per `eth_getLogs` window. Public RPCs cap the range (polygon Amoy
/// rejects > ~128 blocks); we scan newest→oldest in windows of this size so a
/// fresh approval is found in the first request and we never submit a range an
/// RPC will reject. Kept well under common caps.
const LOG_WINDOW_BLOCKS: u64 = 100;

/// Extract the source transaction hash from an EVM source message id
/// (`{0x-tx-hash}-{event_index}`). This is the `message_id` stored on a
/// `PendingTx` for an EVM source (`SourceChainType::Evm`); the `-{index}`
/// suffix is the source log index, which the approval event we match against
/// carries separately, so only the hash is needed here.
pub(super) fn source_tx_hash_from_message_id(message_id: &str) -> Result<FixedBytes<32>> {
    let (hash_str, _idx) = message_id
        .rsplit_once('-')
        .ok_or_else(|| eyre!("malformed EVM message id (no '-{{index}}' suffix): {message_id}"))?;
    let hash_hex = hash_str.strip_prefix("0x").unwrap_or(hash_str);
    let bytes =
        hex::decode(hash_hex).map_err(|e| eyre!("bad tx hash in message id {message_id}: {e}"))?;
    if bytes.len() != 32 {
        return Err(eyre!(
            "tx hash in message id {message_id} is {} bytes, expected 32",
            bytes.len()
        ));
    }
    Ok(FixedBytes::<32>::from_slice(&bytes))
}

/// Scan the destination legacy gateway for the `ContractCallApproved` matching
/// this message and return the on-chain `commandId`. Filters by the indexed
/// `payloadHash`, then pins the match with `contractAddress` + the exact
/// `sourceTxHash` (the source-chain tx that emitted the original
/// `callContract`). Used for GMP, where the dest message's source is the EVM
/// source chain. Returns `None` until the approval is observed.
pub(super) async fn find_contract_call_approved<P: Provider>(
    provider: &P,
    gateway: Address,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
    source_tx_hash: FixedBytes<32>,
    from_block: u64,
) -> Result<Option<[u8; 32]>> {
    find_approval(
        provider,
        gateway,
        contract_addr,
        payload_hash,
        Some(source_tx_hash),
        from_block,
    )
    .await
}

/// Like [`find_contract_call_approved`] but matches on `payloadHash` +
/// `contractAddress` only. Used for the ITS second leg (hub→dest), whose
/// `sourceTxHash` is an Axelar tx — not an EVM hash we can reconstruct. The
/// second-leg `payloadHash` is unique per transfer, so it pins the match.
pub(super) async fn find_contract_call_approved_by_payload<P: Provider>(
    provider: &P,
    gateway: Address,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
    from_block: u64,
) -> Result<Option<[u8; 32]>> {
    find_approval(
        provider,
        gateway,
        contract_addr,
        payload_hash,
        None,
        from_block,
    )
    .await
}

/// Shared `ContractCallApproved` scan: filter by indexed `payloadHash`, match
/// `contractAddress`, and (when given) the exact `sourceTxHash`. Returns the
/// emitted `commandId` of the first match.
async fn find_approval<P: Provider>(
    provider: &P,
    gateway: Address,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
    source_tx_hash: Option<FixedBytes<32>>,
    from_block: u64,
) -> Result<Option<[u8; 32]>> {
    let latest = provider.get_block_number().await?;
    let mut to = latest;
    while to >= from_block {
        let from = to.saturating_sub(LOG_WINDOW_BLOCKS - 1).max(from_block);
        let filter = Filter::new()
            .address(gateway)
            .event_signature(ContractCallApproved::SIGNATURE_HASH)
            .topic3(payload_hash)
            .from_block(from)
            .to_block(to);
        let logs = provider.get_logs(&filter).await?;
        for log in logs {
            let Ok(decoded) = ContractCallApproved::decode_log(&log.inner) else {
                continue;
            };
            if decoded.data.contractAddress == contract_addr
                && source_tx_hash.is_none_or(|h| decoded.data.sourceTxHash == h)
            {
                return Ok(Some(decoded.data.commandId.into()));
            }
        }
        if from == from_block {
            break;
        }
        to = from - 1;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_tx_hash_from_message_id_strips_index_suffix() {
        let hash = source_tx_hash_from_message_id(
            "0x3e49bc399a15eb36a555cf207980155753b7154471918a87f8bdad01a4df01ad-3",
        )
        .unwrap();
        assert_eq!(
            hex::encode(hash),
            "3e49bc399a15eb36a555cf207980155753b7154471918a87f8bdad01a4df01ad"
        );
    }

    #[test]
    fn source_tx_hash_from_message_id_rejects_malformed() {
        assert!(source_tx_hash_from_message_id("no-suffix-but-bad-hash").is_err());
        assert!(source_tx_hash_from_message_id("0xdeadbeef").is_err()); // no '-index'
    }
}
