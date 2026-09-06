mod pricing;
mod render;
mod types;

use std::path::PathBuf;

use eyre::Result;

use super::presentation::IntentActivity;
use super::read::{ApiArgs, api_client, filter_tokens, merge_catalog};
use super::route::{DiscoveryFeedback, read_wallet_assets, resolve_evm_chains};
use super::types::AssetType;
use crate::config::ChainsConfig;
use crate::ui;

pub struct InventoryArgs {
    pub api: ApiArgs,
    pub config: PathBuf,
    pub json: bool,
    pub asset_type: Option<AssetType>,
}

pub async fn inventory(args: InventoryArgs) -> Result<()> {
    let activity = IntentActivity::new("Loading solver inventory…", !args.json);
    let deployment = types::SolverDeployment::try_from(args.api.network)?;
    let solver = deployment.address();
    let client = api_client(&args.api)?;
    let config = ChainsConfig::load(&args.config).await?;
    let (catalog_chains, catalog_tokens) = tokio::try_join!(client.chains(), client.tokens())?;
    let tokens = filter_tokens(catalog_tokens.tokens, args.asset_type);
    let catalog = merge_catalog(catalog_chains.chains.clone(), tokens.clone(), None)?;
    let symbols = tokens.iter().map(|token| token.symbol.as_str());
    let feedback = if args.json {
        DiscoveryFeedback::Quiet
    } else {
        DiscoveryFeedback::Detailed
    };
    let (chains, prices) = tokio::join!(
        resolve_evm_chains(&config, catalog_chains.chains, feedback),
        pricing::fetch_usd_prices(symbols),
    );
    let chains = chains?;
    let prices = prices.unwrap_or_else(|error| {
        if !args.json {
            ui::warn(&format!(
                "USD prices are unavailable: {}",
                ui::scrub_urls(&error.to_string())
            ));
        }
        pricing::UsdPrices::unavailable()
    });
    let assets = read_wallet_assets(&chains, tokens, solver, feedback).await;
    let report = types::InventoryReport::build(types::InventoryInputs {
        network: args.api.network,
        solver_address: solver,
        catalog,
        available_chain_ids: chains.keys().cloned().collect(),
        assets,
        price_source: prices.source,
        prices: prices.by_symbol,
    });
    drop(activity);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render::render(&report, deployment.low_inventory_threshold_usd());
    }
    Ok(())
}
