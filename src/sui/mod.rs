//! Sui (Move) primitives for axe load-testing. Parses `suiprivkey1...` bech32
//! secrets into ed25519/secp256k1 keypairs, derives the canonical Sui address,
//! wraps a JSON-RPC client with auto-fallback to public endpoints, and builds,
//! signs, and submits Programmable Transaction Blocks (PTBs) for the GMP
//! `send_call` flow used by the load test.
//!
//! Submodules:
//! - [`wallet`]: `SuiKeypair`, `SuiWallet`, address derivation, intent
//!   signature serialization.
//! - [`rpc`]: `SuiClient` (JSON-RPC + fallback) and the parsing helpers that
//!   lift Sui-RPC responses into `sui_sdk_types` values.
//! - [`tx`]: `SubmittedTx`, `PtbBuilder`, BCS encoding, intent framing, and
//!   `sign_and_submit`.
//! - [`config`]: `SuiContractsConfig` and the chains-config readers
//!   (`read_sui_chain_config`, `read_sui_gateway_pkg`, `parse_sui_addr`).
//! - [`gmp`]: `SuiGmpCall`, `GmpSendResult`, and the high-level
//!   `send_gmp_call` PTB.
//!
//! All public surfaces are re-exported from this `mod.rs` so existing
//! `crate::sui::*` imports keep compiling unchanged.

mod config;
mod gmp;
mod its;
mod rpc;
mod tx;
mod wallet;

pub use config::{read_sui_chain_config, read_sui_gateway_pkg};
pub use gmp::{SuiGmpCall, send_gmp_call};
pub use its::{InterchainTransferRequest, read_sui_its_config, send_its_interchain_transfer};
pub use rpc::SuiClient;
pub use wallet::SuiWallet;
