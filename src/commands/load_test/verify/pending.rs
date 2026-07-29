use std::time::Instant;

use alloy::primitives::{Address, keccak256};
use eyre::{Result, WrapErr};

use super::input::SourceChainType;
use super::pipeline::parse_payload_hash;
use super::state::{PendingTx, PendingTxInput, Phase};
use crate::commands::load_test::identifiers::{MessageId, PayloadHash};
use crate::commands::load_test::metrics::TxMetrics;
use crate::solana::solana_call_contract_index;
use crate::types::Network;

fn parse_first_leg_payload_hash(tx: &TxMetrics, required: bool) -> Result<Option<PayloadHash>> {
    if tx.payload_hash.is_empty() {
        if required {
            return Err(eyre::eyre!(
                "missing payload_hash for confirmed tx {}",
                tx.signature
            ));
        }
        return Ok(None);
    }
    parse_payload_hash(&tx.payload_hash)
        .map(Some)
        .wrap_err_with(|| format!("invalid payload_hash for confirmed tx {}", tx.signature))
}

/// Build a `PendingTx` for an ITS-via-hub batch entry. The four ITS batch
/// orchestrators share the same initialization; only the starting phase
/// differs.
pub(super) fn pending_tx_for_its_batch(
    tx: &TxMetrics,
    idx: usize,
    initial_phase: Phase,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, initial_phase == Phase::Voted)?;
    Ok(PendingTx::new(PendingTxInput {
        idx,
        message_id: tx.signature.clone().into(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        initial_phase,
    }))
}

/// Compute the source-side message ID for a confirmed transaction.
///
/// EVM, Stellar, and Sui senders pre-format the ID. Solana raw GMP paths need
/// the static call-contract index, while ITS paths already carry their
/// dynamically observed inner-instruction index.
pub(super) fn message_id_for_source(
    tx: &TxMetrics,
    source_type: SourceChainType,
    network: Network,
) -> MessageId {
    let message_id = match source_type {
        SourceChainType::Evm | SourceChainType::Stellar | SourceChainType::Sui => {
            tx.signature.clone()
        }
        SourceChainType::Svm => {
            if tx.signature.contains('-') {
                tx.signature.clone()
            } else {
                format!("{}-{}.1", tx.signature, solana_call_contract_index(network))
            }
        }
    };
    message_id.into()
}

/// Destination-specific initialization data for a GMP batch entry.
pub(super) struct PendingGmpBatchArgs {
    pub(super) idx: usize,
    pub(super) message_id: MessageId,
    pub(super) contract_addr: Address,
    pub(super) command_id: Option<[u8; 32]>,
    pub(super) gmp_destination_chain: String,
    pub(super) gmp_destination_address: String,
    pub(super) initial_phase: Phase,
}

/// Build a `PendingTx` for a GMP batch entry.
pub(super) fn pending_tx_for_gmp_batch(
    tx: &TxMetrics,
    args: PendingGmpBatchArgs,
) -> Result<PendingTx> {
    let PendingGmpBatchArgs {
        idx,
        message_id,
        contract_addr,
        command_id,
        gmp_destination_chain,
        gmp_destination_address,
        initial_phase,
    } = args;
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    Ok(PendingTx::new(PendingTxInput {
        idx,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id,
        gmp_destination_chain,
        gmp_destination_address,
        initial_phase,
    }))
}

/// Convert confirmed metrics into a Solana verification entry.
pub(in crate::commands::load_test) fn tx_to_pending_solana(
    tx: &TxMetrics,
    idx: usize,
    source_chain: &str,
    has_voting_verifier: bool,
    source_type: SourceChainType,
    network: Network,
    legacy_route: bool,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    let message_id = message_id_for_source(tx, source_type, network);
    let cmd_input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
    Ok(PendingTx::new(PendingTxInput {
        idx,
        message_id,
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: Some(keccak256(&cmd_input).into()),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        initial_phase: match (has_voting_verifier, legacy_route) {
            (true, _) => Phase::Voted,
            (false, true) => Phase::Approved,
            (false, false) => Phase::Routed,
        },
    }))
}

/// Convert confirmed Stellar metrics into a GMP verification entry.
pub(in crate::commands::load_test) fn tx_to_pending_stellar(
    tx: &TxMetrics,
    has_voting_verifier: bool,
    contract_addr: Address,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, true)?;
    Ok(PendingTx::new(PendingTxInput {
        idx: 0,
        message_id: tx.signature.clone().into(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        initial_phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::Routed
        },
    }))
}

/// Convert confirmed XRPL metrics into an ITS verification entry.
pub(in crate::commands::load_test) fn tx_to_pending_xrpl(
    tx: &TxMetrics,
    has_voting_verifier: bool,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, has_voting_verifier)?;
    Ok(PendingTx::new(PendingTxInput {
        idx: 0,
        message_id: tx.signature.clone().into(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        initial_phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::HubApproved
        },
    }))
}

/// Convert confirmed metrics into an ITS hub verification entry.
pub(in crate::commands::load_test) fn tx_to_pending_its(
    tx: &TxMetrics,
    has_voting_verifier: bool,
) -> Result<PendingTx> {
    let payload_hash = parse_first_leg_payload_hash(tx, has_voting_verifier)?;
    Ok(PendingTx::new(PendingTxInput {
        idx: 0,
        message_id: tx.signature.clone().into(),
        send_instant: tx.send_instant.unwrap_or_else(Instant::now),
        source_address: tx.source_address.clone(),
        contract_addr: Address::ZERO,
        payload_hash,
        payload_hash_hex: tx.payload_hash.clone(),
        command_id: None,
        gmp_destination_chain: tx.gmp_destination_chain.clone(),
        gmp_destination_address: tx.gmp_destination_address.clone(),
        initial_phase: if has_voting_verifier {
            Phase::Voted
        } else {
            Phase::HubApproved
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::{SourceChainType, message_id_for_source};
    use crate::commands::load_test::metrics::TxMetrics;
    use crate::types::Network;

    #[test]
    fn message_id_normalization_matches_each_source_family_contract() {
        let network = Network::DevnetAmplifier;
        let evm = TxMetrics::succeeded("0xabc-4", 0);
        assert_eq!(
            message_id_for_source(&evm, SourceChainType::Evm, network).as_ref(),
            "0xabc-4"
        );

        let preformatted_solana = TxMetrics::succeeded("signature-1.7", 0);
        assert_eq!(
            message_id_for_source(&preformatted_solana, SourceChainType::Svm, network).as_ref(),
            "signature-1.7"
        );

        let raw_solana = TxMetrics::succeeded("signature", 0);
        assert_eq!(
            message_id_for_source(&raw_solana, SourceChainType::Svm, network).as_ref(),
            format!(
                "signature-{}.1",
                crate::solana::solana_call_contract_index(network)
            )
        );

        let stellar = TxMetrics::succeeded("0xstellar-2", 0);
        assert_eq!(
            message_id_for_source(&stellar, SourceChainType::Stellar, network).as_ref(),
            "0xstellar-2"
        );
    }
}
