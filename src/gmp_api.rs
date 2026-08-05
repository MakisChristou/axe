//! Thin reqwest client over the Axelarscan GMP API (`/gmp/searchGMP`).
//!
//! Shared across commands: the express-reimbursement monitor (`test_express`)
//! and the load-test verifier's final executed-state recheck (`load_test`).
//! Queries used:
//! - list recent express transfers (optionally filtered by destination chain),
//! - fetch a single message by source tx hash,
//! - fetch a single message by its canonical `message_id`.

mod types;

pub use types::{AmountCheck, ExpressRecord, Phase1, Phase2};

use eyre::{Context, Result};
use serde::Deserialize;
use serde_json::json;

/// GMP API base URL for the given network. Testnet/stagenet/devnet share the
/// testnet Axelarscan deployment; mainnet has its own.
pub fn base_url(network: crate::types::Network) -> &'static str {
    match network {
        crate::types::Network::Mainnet => "https://api.axelarscan.io",
        _ => "https://testnet.api.axelarscan.io",
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<ExpressRecord>,
}

async fn post_search(base: &str, body: serde_json::Value) -> Result<Vec<ExpressRecord>> {
    let url = format!("{base}/gmp/searchGMP");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?
        .error_for_status()
        .with_context(|| format!("GMP API returned an error status for {url}"))?;
    let parsed: SearchResponse = resp
        .json()
        .await
        .with_context(|| format!("decoding GMP API response from {url}"))?;
    Ok(parsed.data)
}

/// List the most recent express transfers, newest first. Only records that
/// actually carry `express_executed` are returned by this sort. Optionally
/// narrowed to a single destination chain.
pub async fn search_recent_express(
    base: &str,
    dest_chain: Option<&str>,
    size: usize,
) -> Result<Vec<ExpressRecord>> {
    let mut body = json!({
        "size": size,
        "sort": [{ "express_executed.created_at.ms": "desc" }],
    });
    if let Some(chain) = dest_chain {
        body["destinationChain"] = json!(chain);
    }
    post_search(base, body).await
}

/// Fetch a single message by its source transaction hash, if indexed.
pub async fn search_by_tx(base: &str, tx: &str) -> Result<Option<ExpressRecord>> {
    let records = post_search(base, json!({ "txHash": tx })).await?;
    Ok(records.into_iter().next())
}

/// Fetch a single message by its canonical `message_id` (source tx id plus
/// the event-index suffix, e.g. `0x…-20`), if indexed. This is the exact key
/// Axelarscan stores per GMP message, so it is the preferred lookup.
pub async fn search_by_message_id(base: &str, message_id: &str) -> Result<Option<ExpressRecord>> {
    let records = post_search(base, json!({ "messageId": message_id })).await?;
    Ok(records.into_iter().next())
}
