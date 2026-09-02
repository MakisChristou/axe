use comfy_table::{Attribute, Cell, Color};
use owo_colors::OwoColorize;

use super::types::{InventoryChain, InventoryReport, InventoryToken};
use crate::commands::intents::presentation::{
    asset_table, format_token_amount, format_usd, format_usd_price,
};
use crate::commands::intents::types::is_native_token;
use crate::ui;

pub fn render(report: &InventoryReport, low_inventory_threshold_usd: f64) {
    ui::section("solver inventory");
    ui::kv("network", report.network.as_str());
    ui::address("solver", &report.solver_address);
    ui::kv("known value", &format_usd(report.known_value_usd));
    ui::kv(
        "coverage",
        &format!(
            "{} of {} assets valued · {} balances read",
            report.valued_assets, report.total_assets, report.readable_assets
        ),
    );
    ui::kv("prices", report.price_source);
    let low_inventory = report
        .chains
        .iter()
        .flat_map(|chain| &chain.tokens)
        .filter(|token| is_low_inventory(token, low_inventory_threshold_usd))
        .count();
    if low_inventory > 0 {
        println!(
            "  {}",
            format!(
                "LOW INVENTORY: {low_inventory} asset balance{} below {}",
                if low_inventory == 1 { " is" } else { "s are" },
                format_usd(low_inventory_threshold_usd)
            )
            .red()
            .bold()
        );
    }

    for chain in &report.chains {
        render_chain(chain, low_inventory_threshold_usd);
    }
}

fn render_chain(chain: &InventoryChain, low_inventory_threshold_usd: f64) {
    println!();
    let rpc = if chain.rpc_available {
        "RPC ready"
    } else {
        "RPC unavailable"
    };
    println!(
        "  {}  ·  {}  ·  {}  ·  {}",
        chain.chain_label,
        chain.chain_id,
        chain.chain_type.to_ascii_uppercase(),
        rpc
    );
    if chain.tokens.is_empty() {
        ui::info("No assets advertised.");
        return;
    }

    let mut table = asset_table(&["Asset", "Kind", "Balance", "USD price", "USD value"]);
    for token in &chain.tokens {
        table.add_row(render_token(token, low_inventory_threshold_usd));
    }
    println!("{table}");
    println!(
        "    Known chain value: {}",
        format_usd(chain.known_value_usd)
    );
}

fn render_token(token: &InventoryToken, low_inventory_threshold_usd: f64) -> Vec<Cell> {
    let kind = if is_native_token(&token.address) {
        "native"
    } else {
        "token"
    };
    let balance = token
        .balance
        .as_deref()
        .map(format_token_amount)
        .unwrap_or_else(|| "unavailable".to_owned());
    let price = token
        .price_usd
        .map(format_usd_price)
        .unwrap_or_else(|| "unpriced".to_owned());
    let value = token
        .value_usd
        .map(format_usd)
        .unwrap_or_else(|| "—".to_owned());
    let mut cells = vec![
        Cell::new(&token.symbol).fg(Color::Cyan),
        Cell::new(kind),
        Cell::new(balance),
        Cell::new(price),
        Cell::new(value),
    ];
    if is_low_inventory(token, low_inventory_threshold_usd) {
        cells[0] = cells[0]
            .clone()
            .fg(Color::Red)
            .add_attribute(Attribute::Bold);
        cells[4] = cells[4]
            .clone()
            .fg(Color::Red)
            .add_attribute(Attribute::Bold);
    }
    cells
}

fn is_low_inventory(token: &InventoryToken, threshold_usd: f64) -> bool {
    token.value_usd.is_some_and(|value| value < threshold_usd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(value_usd: Option<f64>) -> InventoryToken {
        InventoryToken {
            address: "0x0000000000000000000000000000000000000000".to_owned(),
            symbol: "TEST".to_owned(),
            decimals: 18,
            balance: Some("1".to_owned()),
            price_usd: value_usd,
            value_usd,
        }
    }

    #[test]
    fn low_inventory_requires_a_known_value_below_fifty_dollars() {
        assert!(is_low_inventory(&token(Some(49.99)), 50.0));
        assert!(!is_low_inventory(&token(Some(50.0)), 50.0));
        assert!(!is_low_inventory(&token(None), 50.0));
    }
}
