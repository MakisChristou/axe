//! Stellar/Soroban client primitives used by the load-test tool.
//!
//! Wraps `stellar-rpc-client` + manual `stellar-xdr` transaction construction
//! so `axe` can sign and submit Soroban `InvokeHostFunction` operations
//! (mainly `AxelarGateway.call_contract` and ITS methods).
//!
//! Submodules:
//! - [`wallet`]: `network_passphrase_for`, `StellarWallet` (Ed25519 keypair +
//!   G-address derivation).
//! - [`scval`]: `scval_*` builders and extractors that bridge Rust values
//!   and Soroban `ScVal`s, plus `parse_contract_id`.
//! - [`rpc`]: `StellarClient` (JSON-RPC client) and the response-parsing
//!   helpers that walk `GetTransactionResponse.result_meta`.
//! - [`tx`]: `InvokedTx` — the submit + poll result.
//!
//! References:
//! - TypeScript: `axelar-contract-deployments/stellar/gateway.js`, `its.js`
//! - Soroban submission flow: build → `simulate_transaction` → merge footprint
//!   + auth + min_resource_fee → sign → `send_transaction` → poll

mod rpc;
mod scval;
mod tx;
mod wallet;

pub use rpc::{
    DeployInterchainTokenRequest, DeployRemoteInterchainTokenRequest, InterchainTransferRequest,
    MessageApprovalQuery, StellarClient,
};
pub use scval::{
    parse_contract_id, scval_address_account, scval_bytes, scval_string, scval_to_address_string,
    scval_token,
};
pub use wallet::StellarWallet;
