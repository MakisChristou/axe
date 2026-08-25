//! Sui JSON-RPC client with auto-fallback to public endpoints, plus the
//! response-parsing helpers (`owner_addr_hex`, `object_ref_from_json`,
//! `parse_sui_digest`) used to lift raw JSON into `sui_sdk_types` values.

use base64::Engine;
use eyre::{Result, eyre};
use serde_json::{Value, json};
use sui_sdk_types::{Address as SuiAddress, Digest, ObjectReference};

use super::tx::SubmittedTx;
use crate::retry::{FALLBACK_ATTEMPTS, is_transient_default, retry_async};

/// Public Sui RPCs used as silent fallbacks if the configured endpoint errors.
/// Mainnet and testnet share keys; we pick by URL hint.
const TESTNET_FALLBACKS: &[&str] = &[
    "https://fullnode.testnet.sui.io:443",
    "https://sui-testnet-endpoint.blockvision.org",
    "https://sui-testnet-rpc.publicnode.com",
];
const MAINNET_FALLBACKS: &[&str] = &[
    "https://fullnode.mainnet.sui.io:443",
    // PublicNode retired sui-mainnet-rpc.publicnode.com (permanent 404,
    // observed killing preflight in cron run 32793045257).
    "https://sui-rpc.publicnode.com",
];

#[derive(Debug, thiserror::Error)]
enum SuiRpcError {
    #[error("Sui RPC {endpoint} HTTP {status}: {body}")]
    Http {
        endpoint: String,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("Sui RPC {endpoint} {method}: {payload}")]
    Rpc {
        endpoint: String,
        method: String,
        // Boxed to keep the Err variant small (clippy::result_large_err).
        payload: Box<Value>,
        message: String,
    },
    #[error("Sui RPC {endpoint} non-JSON: {source}")]
    NonJson {
        endpoint: String,
        source: serde_json::Error,
    },
    #[error("Sui RPC {endpoint} body read failed: {source}")]
    Body {
        endpoint: String,
        source: reqwest::Error,
    },
    #[error("Sui RPC {endpoint} request failed: {source}")]
    Request {
        endpoint: String,
        source: reqwest::Error,
    },
    #[error("Sui RPC exhausted all endpoints")]
    Exhausted,
}

impl SuiRpcError {
    /// Whether another pass over the endpoints could plausibly succeed.
    /// Transport, HTTP-status, body-read, and non-JSON failures are endpoint
    /// flakiness; a JSON-RPC error body is only retried when its text matches
    /// the transient signatures (429, timeouts, 5xx) — a deterministic
    /// failure (MoveAbort, object not found, insufficient gas) answers the
    /// same on every pass, so retrying just delays the real error.
    fn is_transient(&self) -> bool {
        match self {
            Self::Rpc { .. } => is_transient_default(self),
            _ => true,
        }
    }

    fn is_query_events_indexer_pending(&self) -> bool {
        matches!(
            self,
            Self::Rpc { message, .. }
                if message.contains("Could not find the referenced transaction events")
                    && message.contains("TransactionDigest")
        )
    }
}

#[derive(Clone)]
pub struct SuiClient {
    primary: String,
    fallbacks: Vec<String>,
    http: reqwest::Client,
}

impl SuiClient {
    pub fn new(rpc_url: &str) -> Self {
        let primary = rpc_url.trim_end_matches('/').to_string();
        let fallbacks = if primary.contains("mainnet") {
            MAINNET_FALLBACKS
                .iter()
                .filter(|u| **u != primary)
                .map(|s| (*s).to_string())
                .collect()
        } else {
            TESTNET_FALLBACKS
                .iter()
                .filter(|u| **u != primary)
                .map(|s| (*s).to_string())
                .collect()
        };
        Self {
            primary,
            fallbacks,
            http: crate::http::client().clone(),
        }
    }

    /// JSON-RPC call with silent fallback on transient failures.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_typed(method, params)
            .await
            .map_err(eyre::Report::new)
    }

    /// One pass over `[primary, ...fallbacks]`, retried up to
    /// [`FALLBACK_ATTEMPTS`] passes with geometric backoff while the last
    /// error of a pass looks transient ([`SuiRpcError::is_transient`]).
    /// Every pass reuses the same serialized params, so re-POSTing a signed
    /// transaction is safe — Sui dedups by digest.
    async fn call_typed(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, SuiRpcError> {
        let mut endpoints = vec![self.primary.clone()];
        endpoints.extend(self.fallbacks.iter().cloned());
        retry_async(
            &format!("Sui RPC {method}"),
            FALLBACK_ATTEMPTS,
            SuiRpcError::is_transient,
            || self.call_each_endpoint(method, &params, &endpoints),
        )
        .await
    }

    /// Single pass: try each endpoint in order, moving on to the next
    /// whenever one fails; returns the last endpoint's error if all do.
    async fn call_each_endpoint(
        &self,
        method: &str,
        params: &Value,
        endpoints: &[String],
    ) -> std::result::Result<Value, SuiRpcError> {
        let mut last_err = None;
        for endpoint in endpoints {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            });
            match self.http.post(endpoint).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Ok(text) => {
                            if !status.is_success() {
                                last_err = Some(SuiRpcError::Http {
                                    endpoint: endpoint.clone(),
                                    status,
                                    body: text.chars().take(300).collect(),
                                });
                                continue;
                            }
                            match serde_json::from_str::<Value>(&text) {
                                Ok(v) => {
                                    if let Some(err) = v.get("error") {
                                        last_err = Some(SuiRpcError::Rpc {
                                            endpoint: endpoint.clone(),
                                            method: method.to_string(),
                                            payload: Box::new(err.clone()),
                                            message: err
                                                .get("message")
                                                .and_then(Value::as_str)
                                                .unwrap_or_default()
                                                .to_string(),
                                        });
                                        // RPC-level error usually applies on every endpoint.
                                        // But still try fallbacks for transient issues.
                                        continue;
                                    }
                                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                                }
                                Err(e) => {
                                    last_err = Some(SuiRpcError::NonJson {
                                        endpoint: endpoint.clone(),
                                        source: e,
                                    });
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            last_err = Some(SuiRpcError::Body {
                                endpoint: endpoint.clone(),
                                source: e,
                            });
                            continue;
                        }
                    }
                }
                Err(e) => {
                    last_err = Some(SuiRpcError::Request {
                        endpoint: endpoint.clone(),
                        source: e,
                    });
                    continue;
                }
            }
        }
        Err(last_err.unwrap_or(SuiRpcError::Exhausted))
    }

    pub async fn get_chain_identifier(&self) -> Result<String> {
        let r = self.call("sui_getChainIdentifier", json!([])).await?;
        Ok(r.as_str().unwrap_or_default().to_string())
    }

    pub async fn get_reference_gas_price(&self) -> Result<u64> {
        let r = self.call("suix_getReferenceGasPrice", json!([])).await?;
        let s = r.as_str().ok_or_else(|| eyre!("rgp response not string"))?;
        s.parse::<u64>().map_err(|e| eyre!("rgp parse: {e}"))
    }

    pub async fn get_balance(&self, owner: &SuiAddress) -> Result<u64> {
        let r = self
            .call(
                "suix_getBalance",
                json!([owner_addr_hex(owner), "0x2::sui::SUI"]),
            )
            .await?;
        let s = r["totalBalance"]
            .as_str()
            .ok_or_else(|| eyre!("totalBalance missing in {r}"))?;
        s.parse::<u64>().map_err(|e| eyre!("balance parse: {e}"))
    }

    /// Get the largest SUI coin object owned by `owner`. Returns its ObjectReference
    /// (id, version, digest) suitable for `GasPayment::objects`.
    /// Fetch ALL owned `Coin<T>` objects for `owner`, every page, sorted
    /// largest-first. A single `suix_getCoins` page (50 items) is NOT the
    /// whole wallet: cron refunds fragment it into hundreds of small coins,
    /// and picking only from page one made the largest real coin invisible.
    pub async fn all_coins_of_type(
        &self,
        owner: &SuiAddress,
        coin_type: &str,
    ) -> Result<Vec<(ObjectReference, u128)>> {
        // 20 pages x 100 = 2000 objects — far beyond any sane wallet.
        const MAX_PAGES: usize = 20;
        let mut coins: Vec<(ObjectReference, u128)> = Vec::new();
        let mut cursor = Value::Null;
        for _ in 0..MAX_PAGES {
            let r = self
                .call(
                    "suix_getCoins",
                    json!([owner_addr_hex(owner), coin_type, cursor, 100]),
                )
                .await?;
            let arr = r["data"]
                .as_array()
                .ok_or_else(|| eyre!("getCoins(coin_type='{coin_type}') missing data: {r}"))?;
            for c in arr {
                let bal: u128 = c["balance"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                coins.push((object_ref_from_json(c)?, bal));
            }
            if !r["hasNextPage"].as_bool().unwrap_or(false) {
                break;
            }
            cursor = r["nextCursor"].clone();
        }
        coins.sort_by_key(|(_, bal)| std::cmp::Reverse(*bal));
        Ok(coins)
    }

    /// Collect SUI gas coins totalling at least `min_total_mist`, largest
    /// first (capped at 200 — Sui accepts up to 256 gas-payment objects).
    /// Every listed object is "smashed" into one by the runtime before
    /// execution, so this both survives a fragmented wallet (observed live:
    /// 150+ sub-0.2-SUI objects while no single object covered the
    /// cross-chain gas split) and permanently defragments it a little on
    /// every run.
    pub async fn pick_gas_coins(
        &self,
        owner: &SuiAddress,
        min_total_mist: u128,
    ) -> Result<Vec<ObjectReference>> {
        const MAX_GAS_OBJECTS: usize = 200;
        let coins = self.all_coins_of_type(owner, "0x2::sui::SUI").await?;
        if coins.is_empty() {
            return Err(eyre!(
                "wallet {} has no SUI coins — fund it first",
                owner_addr_hex(owner)
            ));
        }
        let total: u128 = coins.iter().map(|(_, bal)| bal).sum();
        let mut picked = Vec::new();
        let mut sum: u128 = 0;
        for (obj, bal) in coins {
            if (sum >= min_total_mist && !picked.is_empty()) || picked.len() >= MAX_GAS_OBJECTS {
                break;
            }
            sum += bal;
            picked.push(obj);
        }
        if sum < min_total_mist {
            return Err(eyre!(
                "wallet {} holds {total} mist of SUI in total — less than the {min_total_mist} \
                 mist needed for gas budget + cross-chain gas",
                owner_addr_hex(owner)
            ));
        }
        Ok(picked)
    }

    /// Fetch `(initial_shared_version, latest_version, digest)` for a shared
    /// object. The first is what PTB inputs need; the latter two are useful
    /// for owned-object inputs.
    pub async fn get_shared_object_initial_version(&self, object_id: &SuiAddress) -> Result<u64> {
        let r = self
            .call(
                "sui_getObject",
                json!([
                    owner_addr_hex(object_id),
                    {"showOwner": true, "showPreviousTransaction": false}
                ]),
            )
            .await?;
        let owner = r
            .pointer("/data/owner")
            .ok_or_else(|| eyre!("getObject missing owner: {r}"))?;
        if let Some(shared) = owner.get("Shared") {
            let v: u64 = shared
                .get("initial_shared_version")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| eyre!("Shared.initial_shared_version missing: {shared}"))?;
            return Ok(v);
        }
        Err(eyre!(
            "object {} is not Shared; owner = {owner}",
            owner_addr_hex(object_id)
        ))
    }

    /// Submit a fully-signed transaction. Returns the tx digest.
    pub async fn execute_transaction(
        &self,
        tx_bcs: &[u8],
        sigs: &[Vec<u8>],
    ) -> Result<SubmittedTx> {
        let tx_b64 = base64::engine::general_purpose::STANDARD.encode(tx_bcs);
        let sigs_b64: Vec<String> = sigs
            .iter()
            .map(|s| base64::engine::general_purpose::STANDARD.encode(s))
            .collect();
        let r = self
            .call(
                "sui_executeTransactionBlock",
                json!([
                    tx_b64,
                    sigs_b64,
                    {"showEffects": true, "showEvents": true},
                    "WaitForLocalExecution"
                ]),
            )
            .await?;
        let digest = r["digest"]
            .as_str()
            .ok_or_else(|| eyre!("execute response missing digest: {r}"))?
            .to_string();
        let status = r
            .pointer("/effects/status/status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let success = status == "success";
        let error = if success {
            None
        } else {
            r.pointer("/effects/status/error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let events = r
            .get("events")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(SubmittedTx {
            digest,
            success,
            error,
            events,
        })
    }

    /// Query recent `MessageApproved` events from a Move events module
    /// (typically the AxelarGateway events module) and return whether one
    /// matching `(source_chain, message_id)` exists.
    pub async fn has_message_approved(
        &self,
        gateway_events_type: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<bool> {
        self.has_matching_event(gateway_events_type, source_chain, message_id)
            .await
    }

    /// Query for `MessageExecuted` event matching `(source_chain, message_id)`.
    pub async fn has_message_executed(
        &self,
        gateway_events_type: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<bool> {
        self.has_matching_event(gateway_events_type, source_chain, message_id)
            .await
    }

    async fn has_matching_event(
        &self,
        move_event_type: &str,
        source_chain: &str,
        message_id: &str,
    ) -> Result<bool> {
        // Walk pages of `suix_queryEvents` (newest-first) until we find a
        // matching `(source_chain, message_id)` or hit `MAX_PAGES`. The
        // gateway can emit hundreds of MessageApproved events per minute on
        // testnet, so the 50-newest snapshot from a single call isn't
        // enough — our event ages out before we poll.
        const PAGE_SIZE: usize = 100;
        const MAX_PAGES: usize = 20; // 20 × 100 = 2000 newest events scanned
        let mut cursor: Value = Value::Null;
        for _ in 0..MAX_PAGES {
            let r = match self
                .call_typed(
                    "suix_queryEvents",
                    json!([
                        {"MoveEventType": move_event_type},
                        cursor,
                        PAGE_SIZE,
                        true,
                    ]),
                )
                .await
            {
                Ok(result) => result,
                Err(err) if err.is_query_events_indexer_pending() => return Ok(false),
                Err(err) => return Err(eyre::Report::new(err)),
            };
            let arr = r
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for ev in &arr {
                let Some(pj) = ev.get("parsedJson") else {
                    continue;
                };
                // Sui's `MessageApproved` events nest `{source_chain,
                // message_id, ...}` under a `message` key. `MessageExecuted`
                // emits the fields at the top level. Try both shapes.
                let inner = pj.get("message").unwrap_or(pj);
                let src = inner
                    .get("source_chain")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let mid = inner
                    .get("message_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if src == source_chain && mid == message_id {
                    return Ok(true);
                }
            }
            let has_next = r
                .get("hasNextPage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
            match r.get("nextCursor") {
                Some(c) if !c.is_null() => cursor = c.clone(),
                _ => break,
            }
        }
        Ok(false)
    }
}

pub(super) fn owner_addr_hex(a: &SuiAddress) -> String {
    format!("0x{}", hex::encode(a.as_bytes()))
}

pub(super) fn object_ref_from_json(v: &Value) -> Result<ObjectReference> {
    let id = v["coinObjectId"]
        .as_str()
        .or_else(|| v["objectId"].as_str())
        .or_else(|| v.pointer("/data/objectId").and_then(|x| x.as_str()))
        .ok_or_else(|| eyre!("objectId missing in {v}"))?;
    let version: u64 = v["version"]
        .as_str()
        .or_else(|| v.pointer("/data/version").and_then(|x| x.as_str()))
        .ok_or_else(|| eyre!("version missing in {v}"))?
        .parse()
        .map_err(|e| eyre!("version parse: {e}"))?;
    let digest_str = v["digest"]
        .as_str()
        .or_else(|| v.pointer("/data/digest").and_then(|x| x.as_str()))
        .ok_or_else(|| eyre!("digest missing in {v}"))?;
    let id_addr = SuiAddress::from_hex(id).map_err(|e| eyre!("objectId hex: {e:?}"))?;
    let digest = parse_sui_digest(digest_str)?;
    Ok(ObjectReference::new(id_addr, version, digest))
}

/// Parse Sui's base58-encoded digest into the canonical 32-byte `Digest`.
fn parse_sui_digest(s: &str) -> Result<Digest> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| eyre!("digest base58 decode: {e}"))?;
    if bytes.len() != 32 {
        return Err(eyre!(
            "digest decoded to {} bytes, expected 32",
            bytes.len()
        ));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&bytes);
    Digest::from_bytes(a).map_err(|e| eyre!("digest parse: {e:?}"))
}
