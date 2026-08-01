//! XRPL client primitives used by the load-test tool.
//!
//! Thin wrapper over `xrpl_http_client::Client` + `xrpl_binary_codec` for
//! building, signing, submitting and polling XRPL `Payment` transactions that
//! carry Axelar ITS memos.
//!
//! Submodules:
//! - [`wallet`]: `XrplWallet`, secp256k1 family-seed derivation, and the
//!   `account_id_from_public_key` helper.
//! - [`its`]: `build_its_transfer_memos` (and the small `memo` builder).
//! - [`rpc`]: `XrplClient`, `AccountInfo`, `ValidatedTx`, and the
//!   `LAST_LEDGER_SEQUENCE_BUMP` tx-submission tuning constant.
//! - [`helpers`]: small free-standing helpers (`parse_address`,
//!   `account_id_to_hex`, `faucet_url_for_network`, `signed_tx_hash_hex`).

// XRPL route support is split between the current load-test sender/verifier
// code and reusable client primitives here. The unused items are the generic
// client helpers for submitting/polling XRPL transactions and destination
// verification plumbing; remove this once all XRPL routes use those helpers or
// the duplicate route-local code is deleted.
#![allow(dead_code)]

mod helpers;
mod its;
mod rpc;
mod wallet;

pub use helpers::{account_id_to_hex, faucet_url_for_network, parse_address};
pub use its::build_its_transfer_memos;
pub use rpc::{LAST_LEDGER_SEQUENCE_BUMP, XrplClient};
pub use wallet::XrplWallet;
