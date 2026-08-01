//! Phase B helpers — preflight checks for the destination ITS (does it trust
//! the source chain? what bech32 does it expect for the hub address?) and a
//! receiver-balance polling loop that confirms a transfer landed without
//! ever erroring (a stuck relay isn't a fatal test failure).
//!
//! The actual Phase B body — the Solana `InterchainTransfer` + relay legs —
//! lives inline in [`run_config`](super::run_config) for now. Only its
//! supporting helpers were extracted here.

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
};
use eyre::Result;

use super::{DEST_CHAIN, TOTAL_STEPS};
use crate::evm::{ERC20, InterchainTokenService, Ownable};
use crate::timing::{DEST_CHAIN_POLL_ATTEMPTS, DEST_CHAIN_POLL_INTERVAL};
use crate::ui;

/// Probe whether the destination ITS trusts the source chain. Two ITS API
/// generations are deployed in the wild — the older one returns a string from
/// `trustedAddress(chain)`, the newer exposes `isTrustedChain(chain)`. If
/// neither says trusted, print remediation and return `Ok(false)`.
pub(super) async fn check_destination_trusts_source<P: Provider>(
    its: &InterchainTokenService::InterchainTokenServiceInstance<&P>,
    src_axelar_id: &crate::types::ChainAxelarId,
    dst_its_proxy: Address,
    dst: &str,
    dst_rpc: &str,
) -> Result<bool> {
    let legacy_trust = its
        .trustedAddress(src_axelar_id.clone().into())
        .call()
        .await
        .ok();
    let new_trust = its
        .isTrustedChain(src_axelar_id.clone().into())
        .call()
        .await
        .ok();
    let trusted = match (legacy_trust.as_deref(), new_trust) {
        (Some(s), _) if !s.is_empty() => true,
        (_, Some(b)) => b,
        _ => false,
    };
    if trusted {
        return Ok(true);
    }

    ui::error(&format!(
        "destination ITS at {dst_its_proxy} on {dst} does not trust source chain '{src_axelar_id}'"
    ));
    let owner = Ownable::new(dst_its_proxy, its.provider())
        .owner()
        .call()
        .await
        .ok();
    let mut lines: Vec<String> = vec![format!(
        "Set '{src_axelar_id}' as trusted on the destination ITS:"
    )];
    if let Some(owner) = owner {
        lines.push(format!("  owner: {owner}"));
    }
    lines.push(format!(
        "  cast send {dst_its_proxy} 'setTrustedChain(string)' '{src_axelar_id}' \\"
    ));
    lines.push(format!(
        "    --rpc-url {dst_rpc} --private-key $PRIVATE_KEY"
    ));
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    ui::action_required(&line_refs);
    Ok(false)
}

/// Resolve the hub bech32 the destination ITS expects in `execute`'s
/// `sourceAddress`. Probes legacy `trustedAddress("axelar")` first, then new
/// `itsHubAddress()`, falling back to the cosm config.
pub(super) async fn resolve_hub_address_evm_view<P: Provider>(
    its: &InterchainTokenService::InterchainTokenServiceInstance<&P>,
    its_hub_address: &str,
) -> String {
    match its
        .trustedAddress(crate::types::HubChain::NAME.to_string())
        .call()
        .await
    {
        Ok(s) if !s.is_empty() => s,
        _ => match its.itsHubAddress().call().await {
            Ok(s) if !s.is_empty() => s,
            _ => its_hub_address.to_string(),
        },
    }
}

/// Poll the destination-chain ERC20 until the receiver's balance is non-zero
/// (i.e. the relayer has executed the transfer). Logs success/timeout but
/// never errors — a stuck relay isn't a fatal test failure.
pub(super) async fn poll_for_balance_on_destination<P: Provider>(
    dest_provider: &P,
    predicted_addr: Address,
    receiver: Address,
) {
    ui::step_header(10, TOTAL_STEPS, &format!("Verify transfer on {DEST_CHAIN}"));
    ui::address("token", &format!("{predicted_addr}"));
    ui::address("receiver", &format!("{receiver}"));

    let dest_token = ERC20::new(predicted_addr, dest_provider);
    let spinner = ui::wait_spinner(&format!("Waiting for balance to appear on {DEST_CHAIN}..."));

    let mut final_balance = U256::ZERO;
    for i in 0..DEST_CHAIN_POLL_ATTEMPTS {
        if i > 0 {
            tokio::time::sleep(DEST_CHAIN_POLL_INTERVAL).await;
        }
        match dest_token.balanceOf(receiver).call().await {
            Ok(bal) => {
                if bal > U256::ZERO {
                    final_balance = bal;
                    break;
                }
                spinner.set_message(format!("Balance still 0 (attempt {}/30)...", i + 1));
            }
            Err(_) => {
                spinner.set_message(format!("Query failed (attempt {}/30)...", i + 1));
            }
        }
    }
    spinner.finish_and_clear();

    if final_balance > U256::ZERO {
        ui::success(&format!(
            "Receiver {receiver} has balance {final_balance} on {DEST_CHAIN}"
        ));
    } else {
        ui::warn(&format!("Balance still 0 on {DEST_CHAIN} after 5 minutes"));
        ui::info("The relayer may still be processing. Check axelarscan for status.");
    }
}
