//! `RpcClient` constructor + retry helpers for fetching confirmed/finalized
//! transactions.
//!
//! All write paths in `axe` use `CommitmentConfig::finalized()`; the
//! constructor keeps that invariant in one place so callers don't sprinkle
//! `RpcClient::new_with_commitment(_, finalized())` literals across the
//! codebase. `fetch_confirmed_tx` lives here because it owns the retry
//! schedule that's tuned to the public devnet RPC's eventual-consistency
//! window between `confirmed` and `getTransaction` indexing.

use eyre::Result;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::UiTransactionEncoding;

use crate::retry::{FALLBACK_ATTEMPTS, backoff_for_attempt, is_transient_default};
use crate::ui;

/// Blocking counterpart of `crate::retry::retry_async` — the solana_client
/// calls in this module are synchronous, so the async helper can't be used.
/// Same schedule: [`FALLBACK_ATTEMPTS`] attempts with geometric backoff
/// (4s, 8s, 16s, 32s, 64s), ~124s worst-case wall-clock.
///
/// Safe for submits ONLY when `op` re-sends the SAME already-signed
/// transaction: Solana dedups by signature (identical bytes = same tx), so
/// at most one copy lands. The ≈7.5s retry window sits well inside the
/// ~2min blockhash validity, and `send_and_confirm_transaction` already
/// retries internally while the blockhash is valid — this outer loop covers
/// transport-level flakiness (429s, timeouts, dropped connections). Never
/// re-fetch the blockhash or re-sign inside `op`: a fresh signature is a
/// new transaction and a real double-send.
pub(crate) fn retry_blocking<T, E: std::fmt::Display>(
    label: &str,
    is_transient: impl Fn(&E) -> bool,
    mut op: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut attempt: u32 = 0;
    loop {
        match op() {
            Ok(t) => return Ok(t),
            Err(e) if attempt + 1 < FALLBACK_ATTEMPTS && is_transient(&e) => {
                let backoff = backoff_for_attempt(attempt);
                ui::warn(&format!(
                    "{label}: attempt {} failed: {e}; retrying in {}ms",
                    attempt + 1,
                    backoff.as_millis(),
                ));
                std::thread::sleep(backoff);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Transient classifier for Solana submits: `is_transient_default` plus the
/// "rate limit" phrasing some Solana RPCs use, minus anything blockhash-
/// related — re-sending the same signed bytes can't fix an expired or
/// unknown blockhash, so those surface immediately. Simulation failures and
/// instruction errors match no transient signature and bail on attempt 1.
pub(crate) fn is_transient_solana<E: std::fmt::Display>(err: &E) -> bool {
    let msg = err.to_string().to_lowercase();
    if msg.contains("blockhash") {
        return false;
    }
    is_transient_default(err) || msg.contains("rate limit")
}

/// Construct an `RpcClient` with finalized commitment — single helper so
/// callers don't sprinkle `RpcClient::new_with_commitment(_, finalized())`
/// across the codebase.
pub fn rpc_client(rpc_url: &str) -> RpcClient {
    RpcClient::new_with_commitment(rpc_url, CommitmentConfig::finalized())
}

/// Fetch tx slot + compute units consumed for an already-confirmed signature.
pub(super) fn fetch_tx_details(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<(Option<u64>, Option<u64>)> {
    let tx = fetch_confirmed_tx(rpc_client, signature)?;
    match tx {
        Some(tx) => {
            let slot = Some(tx.slot);
            let compute_units = tx
                .transaction
                .meta
                .and_then(|m| Option::from(m.compute_units_consumed));
            Ok((compute_units, slot))
        }
        None => Ok((None, None)),
    }
}

/// Fetch a confirmed transaction with retries.
///
/// Public Solana devnet RPC (api.devnet.solana.com) often takes 30+ seconds
/// to index a freshly-confirmed transaction so it's queryable via
/// getTransaction. Use a generous retry budget (~124s wall-clock: the shared
/// 4..64s schedule) before giving up, since the alternative of guessing the
/// message_id costs the caller a full 5-minute pipeline timeout downstream.
pub(super) fn fetch_confirmed_tx(
    rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<Option<solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta>> {
    // Slight upfront delay — `send_and_confirm_transaction` only guarantees
    // the tx is in `confirmed`, not that it's been backfilled into the
    // history endpoint queried by `getTransaction`.
    std::thread::sleep(std::time::Duration::from_millis(750));
    const ATTEMPTS: u32 = 6;
    for i in 0..ATTEMPTS {
        match rpc_client.get_transaction(signature, UiTransactionEncoding::Json) {
            Ok(tx) => return Ok(Some(tx)),
            Err(_) if i + 1 < ATTEMPTS => {
                std::thread::sleep(backoff_for_attempt(i));
            }
            Err(_) => {}
        }
    }
    Ok(None)
}
