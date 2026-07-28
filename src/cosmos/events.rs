//! Event-derived queries. These functions parse Tendermint event attributes
//! out of cosmos tx responses (or follow them up with another query) to
//! discover side effects of the amplifier pipeline: proposal IDs, second-leg
//! ITS message metadata, and routing/approval state.

use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{AxeError, AxeResult};

use super::rpc::{CosmwasmQueryPending, lcd_cosmwasm_smart_query_typed, rpc_tx_search_event};

/// Outer envelope for an LCD `tx_response` payload (the body returned by
/// `/cosmos/tx/v1beta1/txs/{hash}`). Only the events sub-list is needed by
/// `extract_proposal_id`.
#[derive(Deserialize)]
struct TxResponseEnvelope {
    tx_response: TxResponseBody,
}

#[derive(Deserialize)]
struct TxResponseBody {
    events: Vec<TmEvent>,
}

/// A single Tendermint event with a string `type` and a list of
/// `attributes`. The attribute keys are domain-specific (e.g. `proposal_id`,
/// `message_id`, `payload_hash`); callers iterate `attributes` and match by
/// key, so we keep both `key` and `value` as `Option<String>` rather than
/// modelling each variant. Both `event_type` and `attributes` default to
/// permissive values (`""` and `[]`) so events that legacy `Value` lookups
/// silently ignored still deserialise successfully.
#[derive(Deserialize)]
struct TmEvent {
    #[serde(default, rename = "type")]
    event_type: String,
    #[serde(default)]
    attributes: Vec<TmAttribute>,
}

#[derive(Deserialize)]
struct TmAttribute {
    key: Option<String>,
    value: Option<String>,
}

/// Tendermint RPC `tx_search` response envelope:
/// `{ "result": { "txs": [{ "tx_result": { "events": [...] } }] } }`.
/// Used by `discover_second_leg`.
#[derive(Deserialize)]
struct TxSearchEnvelope {
    result: Option<TxSearchResult>,
}

#[derive(Deserialize)]
struct TxSearchResult {
    #[serde(default)]
    txs: Vec<TxSearchTx>,
}

#[derive(Deserialize)]
struct TxSearchTx {
    tx_result: Option<TxResult>,
}

#[derive(Deserialize)]
struct TxResult {
    #[serde(default)]
    events: Vec<TmEvent>,
}

/// Extract proposal_id from tx response events
pub fn extract_proposal_id(tx_resp: &Value) -> Result<u64> {
    let envelope: TxResponseEnvelope =
        serde_json::from_value(tx_resp.clone()).with_context(|| "no events in tx response")?;
    for event in &envelope.tx_response.events {
        if event.event_type == "submit_proposal" || event.event_type == "proposal_submitted" {
            for attr in &event.attributes {
                if attr.key.as_deref() == Some("proposal_id") {
                    let val = attr.value.as_deref().unwrap_or("0");
                    return Ok(val.parse()?);
                }
            }
        }
    }
    Err(eyre::eyre!("proposal_id not found in tx events"))
}

/// Second-leg message info extracted from a hub execution tx that consumed
/// the first-leg message. Set by parsing the `wasm-routing` event.
#[derive(Debug)]
pub struct SecondLegInfo {
    pub message_id: String,
    pub source_chain: String,
    pub destination_chain: String,
    pub payload_hash: String,
    pub source_address: String,
    pub destination_address: String,
}

/// Discover the second-leg message_id by searching the AxelarnetGateway tx
/// that executed the first-leg message, then extracting `wasm-routing` event
/// attributes. Returns `Ok(None)` if the hub hasn't executed yet.
pub async fn discover_second_leg(
    rpc: &str,
    first_leg_message_id: &str,
) -> Result<Option<SecondLegInfo>> {
    let resp = rpc_tx_search_event(
        rpc,
        "wasm-message_executed.message_id",
        first_leg_message_id,
    )
    .await?;

    parse_second_leg_from_tx_search(resp)
}

fn parse_second_leg_from_tx_search(resp: Value) -> Result<Option<SecondLegInfo>> {
    parse_second_leg_from_tx_search_typed(resp).map_err(eyre::Report::new)
}

fn parse_second_leg_from_tx_search_typed(resp: Value) -> AxeResult<Option<SecondLegInfo>> {
    let envelope: TxSearchEnvelope =
        serde_json::from_value(resp).map_err(|err| AxeError::InvalidTxSearch {
            reason: err.to_string(),
        })?;
    let txs = envelope.result.ok_or(AxeError::TxSearchMissingResult)?.txs;

    if txs.is_empty() {
        return Ok(None);
    }

    let events = match txs.into_iter().next().and_then(|t| t.tx_result) {
        Some(r) => r.events,
        None => return Err(AxeError::TxSearchMissingTxResult),
    };

    for event in &events {
        if event.event_type != "wasm-routing" {
            continue;
        }

        let attrs = &event.attributes;

        let get_attr = |key: &str| -> Option<String> {
            attrs.iter().find_map(|a| {
                if a.key.as_deref()? == key {
                    a.value.clone()
                } else {
                    None
                }
            })
        };

        let require_attr = |key: &str| -> AxeResult<String> {
            get_attr(key).ok_or_else(|| AxeError::MissingWasmRoutingAttribute {
                field: key.to_string(),
            })
        };

        return Ok(Some(SecondLegInfo {
            message_id: require_attr("message_id")?,
            source_chain: require_attr("source_chain")?,
            destination_chain: require_attr("destination_chain")?,
            payload_hash: require_attr("payload_hash")?,
            source_address: require_attr("source_address")?,
            destination_address: require_attr("destination_address")?,
        }));
    }

    Ok(None)
}

/// Check whether a message has been routed onto the destination Cosmos
/// Gateway's `outgoing_messages` table. True once the AxelarnetGateway has
/// published the second-leg message.
pub async fn check_cosmos_routed(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = serde_json::json!({
        "outgoing_messages": [{
            "source_chain": source_chain,
            "message_id": message_id,
        }]
    });

    let resp = match lcd_cosmwasm_smart_query_typed(lcd, cosm_gateway, &query).await {
        Ok(response) => response,
        Err(error) if error.is_pending(CosmwasmQueryPending::OutgoingMessage) => return Ok(false),
        Err(error) => return Err(error).wrap_err("Cosmos routing query failed"),
    };
    let data = resp.get("data").or_else(|| resp.as_array().map(|_| &resp));
    Ok(match data {
        Some(arr) if arr.is_array() => {
            let items = arr.as_array().unwrap();
            !items.is_empty() && !items.iter().all(|v| v.is_null())
        }
        _ => false,
    })
}

/// Check whether a message is executable on the AxelarnetGateway hub via
/// the `executable_messages` query. True once the hub has been instructed to
/// forward the message (or once it has executed it).
pub async fn check_hub_approved(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = serde_json::json!({
        "executable_messages": {
            "cc_ids": [{
                "source_chain": source_chain,
                "message_id": message_id,
            }]
        }
    });

    let resp = match lcd_cosmwasm_smart_query_typed(lcd, axelarnet_gateway, &query).await {
        Ok(response) => response,
        Err(error) if error.is_pending(CosmwasmQueryPending::ExecutableMessage) => {
            return Ok(false);
        }
        Err(error) => return Err(error).wrap_err("Axelarnet hub approval query failed"),
    };
    let resp_str = serde_json::to_string(&resp)?;
    Ok(!resp_str.contains("null") && resp_str.contains(message_id))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_second_leg_from_tx_search;

    fn tx_search_with_attrs(attrs: &serde_json::Value) -> serde_json::Value {
        json!({
            "result": {
                "txs": [{
                    "tx_result": {
                        "events": [{
                            "type": "wasm-routing",
                            "attributes": attrs
                        }]
                    }
                }]
            }
        })
    }

    #[test]
    fn second_leg_parser_returns_none_when_no_txs() {
        let parsed = parse_second_leg_from_tx_search(json!({
            "result": { "txs": [] }
        }))
        .unwrap();

        assert!(parsed.is_none());
    }

    #[test]
    fn second_leg_parser_requires_routing_addresses() {
        let err = parse_second_leg_from_tx_search(tx_search_with_attrs(&json!([
            { "key": "message_id", "value": "0xabc-1" },
            { "key": "source_chain", "value": "axelar" },
            { "key": "destination_chain", "value": "ethereum" },
            { "key": "payload_hash", "value": "0x1234" }
        ])))
        .unwrap_err();

        assert!(err.to_string().contains("source_address"));
    }

    #[test]
    fn second_leg_parser_rejects_malformed_tx_search() {
        let err = parse_second_leg_from_tx_search(json!({ "error": "bad query" })).unwrap_err();

        assert!(err.to_string().contains("missing result"));
    }

    #[test]
    fn second_leg_parser_extracts_required_fields() {
        let parsed = parse_second_leg_from_tx_search(tx_search_with_attrs(&json!([
            { "key": "message_id", "value": "0xabc-1" },
            { "key": "source_chain", "value": "axelar" },
            { "key": "destination_chain", "value": "ethereum" },
            { "key": "payload_hash", "value": "0x1234" },
            { "key": "source_address", "value": "hub" },
            { "key": "destination_address", "value": "0x0000000000000000000000000000000000000001" }
        ])))
        .unwrap()
        .unwrap();

        assert_eq!(parsed.message_id, "0xabc-1");
        assert_eq!(parsed.source_chain, "axelar");
        assert_eq!(parsed.destination_chain, "ethereum");
        assert_eq!(parsed.payload_hash, "0x1234");
        assert_eq!(parsed.source_address, "hub");
        assert_eq!(
            parsed.destination_address,
            "0x0000000000000000000000000000000000000001"
        );
    }
}
