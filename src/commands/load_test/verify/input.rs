//! Typed inputs shared by batch and streaming verification entry points.
//!
//! Every bundle here is lifetime-free. Route modules build one, hand it to a
//! verification entry point, and — for streaming routes — move it into a
//! spawned task, without threading a `'a` through their own signatures. The
//! one thing that cannot be owned, the `&mut [TxMetrics]` a batch run writes
//! its verdicts into, travels as a separate argument.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use alloy::primitives::Address;
use tokio::sync::mpsc;

use super::state::PendingTx;
use crate::commands::load_test::LoadTestArgs;
use crate::commands::load_test::chain_names::AxelarChainId;
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
#[derive(Clone)]
pub struct VerificationRoute {
    pub config: PathBuf,
    pub source_chain: AxelarChainId,
    pub destination_chain: AxelarChainId,
    pub network: Network,
}

impl VerificationRoute {
    /// Verification always runs against the Axelar-side chain ids, not the
    /// display names, so this is the only correct way to build a route.
    pub fn from_args(args: &LoadTestArgs) -> Self {
        Self {
            config: args.config.clone(),
            source_chain: args.source_axelar_id.clone().into(),
            destination_chain: args.destination_axelar_id.clone().into(),
            network: args.network,
        }
    }
}

/// Batch verification input for a single-leg GMP route.
pub struct GmpBatchVerification<D> {
    pub route: VerificationRoute,
    pub destination: D,
    pub source_type: SourceChainType,
}

/// Batch verification input for an ITS route through the Axelar hub.
pub struct ItsBatchVerification<D> {
    pub route: VerificationRoute,
    pub destination: D,
}

/// Streaming verification input shared by sustained GMP and ITS routes.
pub struct StreamingVerification<D> {
    pub route: VerificationRoute,
    pub destination: D,
    pub rx: mpsc::UnboundedReceiver<PendingTx>,
    pub send_done: Arc<AtomicBool>,
    pub spinner: indicatif::ProgressBar,
}

/// Existing EVM client and gateway used by burst GMP verification.
///
/// `P` is instantiated with `&SomeProvider` at the call site: alloy implements
/// `Provider` for references, so the borrow lives in the type parameter and
/// the struct itself stays lifetime-free.
pub struct EvmGmpDestination<P> {
    pub address: String,
    pub gateway_addr: Address,
    pub provider: P,
}

/// EVM RPC endpoint and gateway used by streaming GMP verification.
pub struct EvmGmpStreamingDestination {
    pub address: String,
    pub gateway_addr: Address,
    pub rpc_url: String,
}

/// Stellar contracts and client settings used by GMP verification.
pub struct StellarGmpDestination {
    pub contract: String,
    pub rpc_url: String,
    pub network_type: String,
    pub gateway_contract: String,
    pub signer_pk: [u8; 32],
}

/// Sui package lookup inputs used by GMP verification.
pub struct SuiGmpDestination {
    pub address: String,
    pub rpc_url: String,
}

/// Sui RPC endpoint used by ITS verification.
pub struct SuiItsDestination {
    pub rpc_url: String,
}

/// Solana inputs used by GMP verification.
pub struct SolanaGmpDestination {
    pub address: String,
    pub rpc_url: String,
}

/// Solana RPC endpoint used by ITS verification.
pub struct SolanaItsDestination {
    pub rpc_url: String,
}

/// Stellar contracts and client settings used by ITS verification.
pub struct StellarItsDestination {
    pub rpc_url: String,
    pub network_type: String,
    pub gateway_contract: String,
    pub signer_pk: [u8; 32],
}

/// XRPL endpoint and recipient used by ITS verification.
pub struct XrplItsDestination {
    pub rpc_url: String,
    pub recipient: String,
}

/// EVM endpoint and gateway used by ITS verification.
pub struct EvmItsDestination {
    pub gateway_addr: Address,
    pub rpc_url: String,
}
