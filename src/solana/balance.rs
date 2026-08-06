//! Per-flow lamport thresholds and the preflight wallet-balance check.
//!
//! Lives next to the rest of `crate::solana` rather than in the load-test
//! command module because the same thresholds gate `axe test ...` and the
//! manual relay flows; one source of truth keeps the "fund wallet" message
//! consistent across entry points.

use eyre::Result;
use solana_sdk::pubkey::Pubkey;

use super::rpc::{retry_blocking, rpc_client};
use crate::ui;

/// Default minimum balance for a Solana wallet to send a single GMP-style
/// transaction (call_contract + pay_gas of 0.01 SOL + tx fees + headroom).
pub const MIN_SOL_SEND_LAMPORTS: u64 = 20_000_000; // 0.02 SOL

/// Default minimum balance for a Solana wallet that runs the manual
/// destination-side gateway approval flow (init session, N verify_signature
/// calls, approve_message, execute) where each tx pays fees and some create
/// rent-exempt PDAs.
pub const MIN_SOL_RELAY_LAMPORTS: u64 = 50_000_000; // 0.05 SOL

/// Minimum balance for the ITS test command.
pub const MIN_SOL_ITS_LAMPORTS: u64 = 100_000_000; // 0.1 SOL

/// Preflight: ensure a Solana wallet on the given RPC has at least
/// `min_lamports`. Errors with a clear "fund this address" message rather
/// than the cryptic "Attempt to debit an account but found no record of a
/// prior credit" RPC error we'd otherwise hit at send-time.
pub fn check_solana_balance(
    rpc_url: &str,
    label: &str,
    pubkey: &Pubkey,
    min_lamports: u64,
) -> Result<()> {
    let rpc_client = rpc_client(rpc_url);
    let balance = retry_blocking(
        "solana get_balance",
        |_| true,
        || rpc_client.get_balance(pubkey),
    )
    .map_err(|e| eyre::eyre!("failed to query Solana balance for {pubkey}: {e}"))?;

    let display = balance as f64 / 1_000_000_000.0;
    let min_display = min_lamports as f64 / 1_000_000_000.0;

    if balance < min_lamports {
        ui::error(&format!("{label} Solana wallet underfunded:"));
        ui::error(&format!("  address: {pubkey}"));
        ui::error(&format!(
            "  balance: {display:.6} SOL (need >= {min_display:.6})"
        ));
        if balance == 0 {
            ui::error("  account has zero SOL — fund or airdrop before retrying");
            ui::error(&format!("    solana airdrop 2 {pubkey}"));
        }
        return Err(eyre::eyre!(
            "fund {pubkey} with at least {min_display:.6} SOL and retry"
        ));
    }

    ui::kv(
        &format!("{label} balance"),
        &format!("{display:.6} SOL (>= {min_display:.6})"),
    );
    Ok(())
}
