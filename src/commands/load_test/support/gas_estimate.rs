//! Gas estimates for cross-chain GMP payments.
//!
//! The relayer rejects executes whose paid gas-budget doesn't cover the
//! destination-chain execute cost (`availableGasBalance.amount must be
//! positive: …`). Hardcoded source-native defaults (0.02 ETH-equivalent)
//! were tuned for ETH-priced chains and silently underpay routes where
//! source-native is cheap (XRP) or destination-native
//! is volatile (Hyperliquid, where gas-price has been observed to swing
//! ~3.5× intraday).
//!
//! This module wraps the canonical relayer-aware quote at
//! `…api.axelarscan.io/gmp/estimateGasFee`, returning a 1.5×-padded value
//! in source-native smallest-unit (wei / lamports / stroops / mist / etc.).
//! Callers fall back to their existing constants when the API can't be
//! reached or returns 0 (testnet/stagenet do this for unsupported routes).

use crate::http;
use crate::retry::{FALLBACK_ATTEMPTS, retry_async};
use crate::types::Network;
use reqwest::StatusCode;

/// Multiplier applied to the relayer's quote: returned = raw × 3/2.
/// Covers intraday destination-gas-price swings between estimate-at-startup
/// and the relayer's actual execute call.
const SAFETY_NUM: u128 = 3;
const SAFETY_DEN: u128 = 2;

/// Destination-side gas-limit hint passed to the API. The realized
/// `gasUsed` cluster for plain ContractCall executes is ~107k; 400k covers
/// heavier (ITS, multi-account) executes with margin and matches the
/// relayer's own `gasMultiplier=auto` calibration band.
pub(super) const DEFAULT_DEST_GAS_LIMIT: u64 = 400_000;

/// Query Axelarscan for the relayer's gas quote on this route and apply
/// a 1.5× safety margin.
///
/// Returns `None` when the lookup yields no usable number — either the
/// target network has no Axelarscan endpoint (devnet-amplifier), the
/// HTTP request failed, or the API returned 0 (common on testnet/stagenet
/// for routes that aren't fully wired through their indexer).
pub(super) async fn estimate_route_gas(
    network: Network,
    source_axelar_id: &str,
    destination_axelar_id: &str,
    source_token_symbol: &str,
    gas_limit: u64,
) -> Option<u128> {
    let base = api_base_url(network)?;
    let url = format!(
        "{base}/gmp/estimateGasFee?sourceChain={source_axelar_id}\
         &destinationChain={destination_axelar_id}\
         &gasLimit={gas_limit}\
         &gasMultiplier=auto\
         &sourceTokenSymbol={source_token_symbol}"
    );
    let client = http::client();
    // Retry transient failures before giving up; once the retry budget is
    // exhausted (or on a permanent 4xx) still return `None` so callers keep
    // their hardcoded fallback.
    let body = retry_async(
        "gmp-api estimateGasFee",
        FALLBACK_ATTEMPTS,
        is_transient_http,
        || async {
            let resp = client.get(&url).send().await?.error_for_status()?;
            resp.text().await
        },
    )
    .await
    .ok()?;
    let raw: u128 = body.trim().parse().ok()?;
    if raw == 0 {
        return None;
    }
    Some(raw.saturating_mul(SAFETY_NUM) / SAFETY_DEN)
}

/// Retry classification: transport errors (connect / timeout / body read)
/// and HTTP 429/5xx are transient; any other 4xx is a permanent request
/// error and surfaces immediately.
fn is_transient_http(err: &reqwest::Error) -> bool {
    match err.status() {
        Some(status) => status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
        None => true, // transport-level: no HTTP status attached
    }
}

fn api_base_url(network: Network) -> Option<&'static str> {
    match network {
        Network::Mainnet => Some("https://api.axelarscan.io"),
        Network::Testnet => Some("https://testnet.api.axelarscan.io"),
        Network::Stagenet => Some("https://stagenet.api.axelarscan.io"),
        Network::DevnetAmplifier => None,
    }
}
