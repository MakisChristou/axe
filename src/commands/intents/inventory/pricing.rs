use std::collections::{BTreeMap, HashMap};

use eyre::{Result, WrapErr, bail, eyre};
use reqwest::header::USER_AGENT;
use serde::Deserialize;

const COINGECKO_BASE_URL: &str = "https://api.coingecko.com/api/v3";
const DEFILLAMA_BASE_URL: &str = "https://coins.llama.fi";

pub struct UsdPrices {
    pub source: &'static str,
    pub by_symbol: HashMap<String, f64>,
}

impl UsdPrices {
    pub fn unavailable() -> Self {
        Self {
            source: "unavailable",
            by_symbol: HashMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CoinPrice {
    usd: f64,
}

#[derive(Debug, Deserialize)]
struct DefiLlamaResponse {
    coins: HashMap<String, DefiLlamaPrice>,
}

#[derive(Debug, Deserialize)]
struct DefiLlamaPrice {
    price: f64,
}

pub async fn fetch_usd_prices<'a>(symbols: impl Iterator<Item = &'a str>) -> Result<UsdPrices> {
    let symbols_by_id = symbols
        .filter_map(|symbol| coingecko_id(symbol).map(|id| (id, symbol)))
        .fold(
            BTreeMap::<&str, Vec<&str>>::new(),
            |mut grouped, (id, symbol)| {
                grouped.entry(id).or_default().push(symbol);
                grouped
            },
        );
    if symbols_by_id.is_empty() {
        return Ok(UsdPrices {
            source: "CoinGecko",
            by_symbol: HashMap::new(),
        });
    }

    let ids = symbols_by_id.keys().copied().collect::<Vec<_>>().join(",");
    let (source, by_id) = match fetch_coingecko(&ids).await {
        Ok(prices) => ("CoinGecko", prices),
        Err(coingecko_error) => {
            let prices = fetch_defillama(&ids).await.map_err(|defillama_error| {
                eyre!(
                    "CoinGecko failed ({coingecko_error}); DeFiLlama fallback failed ({defillama_error})"
                )
            })?;
            ("DeFiLlama", prices)
        }
    };

    let mut by_symbol = HashMap::new();
    for (id, symbols) in symbols_by_id {
        let Some(price) = by_id.get(id).copied() else {
            continue;
        };
        for symbol in symbols {
            by_symbol.insert(symbol.to_ascii_uppercase(), price);
        }
    }
    Ok(UsdPrices { source, by_symbol })
}

async fn fetch_coingecko(ids: &str) -> Result<HashMap<String, f64>> {
    let url = format!("{COINGECKO_BASE_URL}/simple/price");
    let response = crate::http::client()
        .get(&url)
        .header(USER_AGENT, "axelar-demo")
        .query(&[
            ("ids", ids),
            ("vs_currencies", "usd"),
            ("precision", "full"),
        ])
        .send()
        .await
        .wrap_err("CoinGecko price request failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("CoinGecko returned HTTP {status}");
    }
    let by_id = response
        .json::<HashMap<String, CoinPrice>>()
        .await
        .wrap_err("CoinGecko price response did not match the expected schema")?;
    Ok(by_id
        .into_iter()
        .map(|(id, price)| (id, price.usd))
        .collect())
}

async fn fetch_defillama(ids: &str) -> Result<HashMap<String, f64>> {
    let llama_ids = ids
        .split(',')
        .map(|id| format!("coingecko:{id}"))
        .collect::<Vec<_>>()
        .join(",");
    let url = format!("{DEFILLAMA_BASE_URL}/prices/current/{llama_ids}");
    let response = crate::http::client()
        .get(&url)
        .send()
        .await
        .wrap_err("DeFiLlama price request failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("DeFiLlama returned HTTP {status}");
    }
    let response = response
        .json::<DefiLlamaResponse>()
        .await
        .wrap_err("DeFiLlama price response did not match the expected schema")?;
    Ok(response
        .coins
        .into_iter()
        .filter_map(|(id, price)| {
            id.strip_prefix("coingecko:")
                .map(|id| (id.to_owned(), price.price))
        })
        .collect())
}

fn coingecko_id(symbol: &str) -> Option<&'static str> {
    match symbol.to_ascii_uppercase().as_str() {
        "ETH" => Some("ethereum"),
        "WETH" => Some("weth"),
        "AVAX" | "WAVAX" => Some("avalanche-2"),
        "USDC" => Some("usd-coin"),
        "EURC" => Some("euro-coin"),
        "BTC" | "CIRBTC" => Some("bitcoin"),
        "WBTC" => Some("wrapped-bitcoin"),
        "SOL" | "WSOL" => Some("solana"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_solver_catalog_symbols_to_coingecko() {
        assert_eq!(coingecko_id("ETH"), Some("ethereum"));
        assert_eq!(coingecko_id("cirBTC"), Some("bitcoin"));
        assert_eq!(coingecko_id("unknown"), None);
    }
}
