//! `RpcClient` constructor + retry helpers for fetching confirmed/finalized
//! transactions.
//!
//! All write paths in `axe` use `CommitmentConfig::finalized()`; the
//! constructor keeps that invariant in one place so callers don't sprinkle
//! `RpcClient::new_with_commitment(_, finalized())` literals across the
//! codebase. `fetch_confirmed_tx` lives here because it owns the retry
//! schedule that's tuned to the public devnet RPC's eventual-consistency
//! window between `confirmed` and `getTransaction` indexing.

use eyre::{Result, eyre};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Signature, Signer};
use solana_sdk::transaction::Transaction;
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

/// Whether a submit failure is a blockhash-expiry report. Expiry is the
/// node's proof the transaction did NOT land and never can - so rebuilding
/// on a fresh blockhash cannot double-send, while re-sending the expired
/// bytes can never succeed.
pub(crate) fn is_blockhash_expired<E: std::fmt::Display>(err: &E) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("blockhash not found") || msg.contains("block height exceeded")
}

/// Rebuild-on-expiry rounds in [`sign_send_confirm`]. Each round waits out
/// send_and_confirm's own blockhash-validity window (~2 min), so a few
/// rounds already spans several minutes of congestion.
const MAX_BLOCKHASH_REBUILDS: u32 = 3;

/// Sign `instructions` on a fresh blockhash and submit via
/// `send_and_confirm_transaction`, with the two retry axes that are each
/// safe on Solana:
///
/// - transient transport errors re-send the SAME signed bytes (dedup by
///   signature - at most one copy lands), via [`retry_blocking`]
/// - a blockhash-expiry failure re-signs on a fresh blockhash and resends
///   (the expired tx provably never landed), up to
///   [`MAX_BLOCKHASH_REBUILDS`] rounds
///
/// Terminal failures append `simulate_transaction` diagnostics (program
/// logs) for the last attempted transaction.
pub(crate) fn sign_send_confirm(
    rpc_client: &RpcClient,
    label: &str,
    instructions: &[Instruction],
    payer: &Pubkey,
    signer: &dyn Signer,
) -> Result<Signature> {
    let mut rebuild: u32 = 0;
    loop {
        let blockhash = retry_blocking(
            "solana get_latest_blockhash",
            |_| true,
            || rpc_client.get_latest_blockhash(),
        )?;
        let message = Message::new_with_blockhash(instructions, Some(payer), &blockhash);
        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[signer], blockhash);

        // Same signed tx on every attempt - dedup by signature makes this
        // safe. Blockhash errors are excluded from same-bytes retries by
        // `is_transient_solana` and handled by the rebuild arm below.
        let sent = retry_blocking(label, is_transient_solana, || {
            rpc_client.send_and_confirm_transaction(&transaction)
        });
        match sent {
            Ok(signature) => return Ok(signature),
            Err(e) if is_blockhash_expired(&e) && rebuild + 1 < MAX_BLOCKHASH_REBUILDS => {
                rebuild += 1;
                ui::warn(&format!(
                    "{label}: blockhash expired without the tx landing - re-signing on a \
                     fresh blockhash (rebuild {rebuild}/{MAX_BLOCKHASH_REBUILDS})"
                ));
            }
            Err(e) => {
                let diagnostics = simulation_diagnostics(rpc_client, &transaction);
                return Err(eyre!("{label}: {e}\n  -> {diagnostics}"));
            }
        }
    }
}

/// Simulate the failed transaction and render its program logs, so a
/// terminal submit failure carries the on-chain reason.
fn simulation_diagnostics(rpc_client: &RpcClient, transaction: &Transaction) -> String {
    match rpc_client.simulate_transaction(transaction) {
        Ok(simulation) => {
            let logs = simulation.value.logs.unwrap_or_default();
            let header = match simulation.value.err {
                Some(error) => format!("simulation error: {error:?}"),
                None => "simulation succeeded but submit failed".to_string(),
            };
            if logs.is_empty() {
                header
            } else {
                let body = logs
                    .iter()
                    .map(|line| format!("    {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{header}\n  program logs:\n{body}")
            }
        }
        Err(error) => format!("simulate_transaction follow-up failed: {error}"),
    }
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
