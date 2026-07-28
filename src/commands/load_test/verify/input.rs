//! Typed inputs shared by batch and streaming verification entry points.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use alloy::primitives::Address;
use tokio::sync::mpsc;

use super::state::PendingTx;
use crate::commands::load_test::metrics::TxMetrics;
use crate::types::Network;

/// Source chain type — determines how message IDs are constructed.
#[derive(Clone, Copy)]
pub enum SourceChainType {
    /// Solana source: message ID = `{signature}-{group}.{index}`
    Svm,
    /// EVM source: message ID = `{tx_hash}-{event_index}` (already in tx.signature)
    Evm,
    /// Stellar source: message ID = `0x{lowercase_tx_hash}-{event_index}`
    Stellar,
    /// Sui source: message ID = `{base58_tx_digest}-{event_index}`
    Sui,
}

/// Chain route shared by every verification adapter.
#[derive(Clone, Copy)]
pub struct VerificationRoute<'a> {
    pub config: &'a Path,
    pub source_chain: &'a str,
    pub destination_chain: &'a str,
    pub network: Network,
}

/// Batch verification input for a single-leg GMP route.
pub struct GmpBatchVerification<'a, D> {
    pub route: VerificationRoute<'a>,
    pub destination: D,
    pub metrics: &'a mut [TxMetrics],
    pub source_type: SourceChainType,
}

/// Batch verification input for an ITS route through the Axelar hub.
pub struct ItsBatchVerification<'a, D> {
    pub route: VerificationRoute<'a>,
    pub destination: D,
    pub metrics: &'a mut [TxMetrics],
}

/// Streaming verification input shared by sustained GMP and ITS routes.
pub struct StreamingVerification<'a, D> {
    pub route: VerificationRoute<'a>,
    pub destination: D,
    pub rx: mpsc::UnboundedReceiver<PendingTx>,
    pub send_done: Arc<AtomicBool>,
    pub spinner: indicatif::ProgressBar,
}

/// Existing EVM client and gateway used by burst GMP verification.
pub struct EvmGmpDestination<'a, P> {
    pub address: &'a str,
    pub gateway_addr: Address,
    pub provider: &'a P,
}

/// EVM RPC endpoint and gateway used by streaming GMP verification.
pub struct EvmGmpStreamingDestination<'a> {
    pub address: &'a str,
    pub gateway_addr: Address,
    pub rpc_url: &'a str,
}

/// Stellar contracts and client settings used by GMP verification.
pub struct StellarGmpDestination<'a> {
    pub contract: &'a str,
    pub rpc_url: &'a str,
    pub network_type: &'a str,
    pub gateway_contract: &'a str,
    pub signer_pk: [u8; 32],
}

/// Sui package lookup inputs used by GMP verification.
pub struct SuiGmpDestination<'a> {
    pub address: &'a str,
    pub rpc_url: &'a str,
}

/// Sui RPC endpoint used by ITS verification.
pub struct SuiItsDestination<'a> {
    pub rpc_url: &'a str,
}

/// Solana inputs used by GMP verification.
pub struct SolanaGmpDestination<'a> {
    pub address: &'a str,
    pub rpc_url: &'a str,
}

/// Solana RPC endpoint used by ITS verification.
pub struct SolanaItsDestination<'a> {
    pub rpc_url: &'a str,
}

/// Stellar contracts and client settings used by ITS verification.
pub struct StellarItsDestination<'a> {
    pub rpc_url: &'a str,
    pub network_type: &'a str,
    pub gateway_contract: &'a str,
    pub signer_pk: [u8; 32],
}

/// XRPL endpoint and recipient used by ITS verification.
pub struct XrplItsDestination<'a> {
    pub rpc_url: &'a str,
    pub recipient: &'a str,
}

/// EVM endpoint and gateway used by ITS verification.
pub struct EvmItsDestination<'a> {
    pub gateway_addr: Address,
    pub rpc_url: &'a str,
}
