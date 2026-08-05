use std::path::Path;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use eyre::Result;

use crate::ui;

/// Minimum balance to proceed (0.001 ETH = 10^15 wei)
const MIN_BALANCE: U256 = U256::from_limbs([1_000_000_000_000_000, 0, 0, 0]);

/// Check that all EVM wallets have sufficient native balance for gas.
/// Prints each address and its balance, then errors if any are below the minimum.
pub async fn check_evm_balances(
    rpc_url: &str,
    wallets: &[(&str, Address)],
    token_symbol: &str,
) -> Result<()> {
    if wallets.is_empty() {
        return Ok(());
    }

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let mut underfunded: Vec<(&str, Address, f64)> = Vec::new();

    for &(label, addr) in wallets {
        let balance = provider.get_balance(addr).await?;
        let display = wei_to_display(balance);

        if balance < MIN_BALANCE {
            ui::warn(&format!(
                "{label}: {addr} — {display:.4} {token_symbol} (LOW)"
            ));
            underfunded.push((label, addr, display));
        } else {
            ui::kv(label, &format!("{addr} — {display:.4} {token_symbol}"));
        }
    }

    if !underfunded.is_empty() {
        ui::error("the following addresses need funding before continuing:");
        for (label, addr, bal) in &underfunded {
            ui::error(&format!(
                "  {label}: {addr} has {bal:.4} {token_symbol} (min 0.001)"
            ));
        }
        return Err(eyre::eyre!(
            "{} address(es) underfunded — fund them and retry",
            underfunded.len()
        ));
    }

    Ok(())
}

/// Verify the deployer's native balance on an EVM chain. Looks up the chain's
/// `tokenSymbol` from the target config so balances render in the right unit;
/// errors if the symbol is missing rather than silently defaulting (a stray
/// "ETH" on a non-ETH chain or HBAR mid-flow used to be confusing).
pub async fn check_deployer_balance(
    rpc_url: &str,
    deployer_address: Address,
    target_json: &Path,
    axelar_id: &str,
) -> Result<()> {
    let cfg = crate::config::ChainsConfig::load(target_json).await?;
    let token_symbol = cfg
        .chains
        .get(axelar_id)
        .and_then(|c| c.token_symbol.as_deref())
        .ok_or_else(|| eyre::eyre!("no tokenSymbol for chain '{axelar_id}' in target json"))?;
    check_evm_balances(rpc_url, &[("deployer", deployer_address)], token_symbol).await
}

fn wei_to_display(wei: U256) -> f64 {
    let divisor = U256::from(1_000_000_000_000_000_000u64); // 10^18
    let whole = wei / divisor;
    let remainder = wei % divisor;

    // Convert whole part + fractional part to f64
    let whole_f64: f64 = whole.to::<u64>() as f64;
    let remainder_f64: f64 = remainder.to::<u64>() as f64 / 1e18;
    whole_f64 + remainder_f64
}
