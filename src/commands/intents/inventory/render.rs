use comfy_table::{Cell, Color};

use super::types::{InventoryChain, InventoryReport, InventoryToken};
use crate::commands::intents::presentation::{
    asset_table, format_token_amount, format_usd, format_usd_price,
};
use crate::commands::intents::types::is_native_token;
use crate::ui;

pub fn render(report: &InventoryReport) {
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

    for chain in &report.chains {
        render_chain(chain);
    }
}

fn render_chain(chain: &InventoryChain) {
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
        table.add_row(render_token(token));
    }
    println!("{table}");
    println!(
        "    Known chain value: {}",
        format_usd(chain.known_value_usd)
    );
}

fn render_token(token: &InventoryToken) -> Vec<Cell> {
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
    vec![
        Cell::new(&token.symbol).fg(Color::Cyan),
        Cell::new(kind),
        Cell::new(balance),
        Cell::new(price),
        Cell::new(value),
    ]
}
