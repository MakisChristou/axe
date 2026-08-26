use alloy::dyn_abi::DynSolValue;
use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use eyre::Result;
use owo_colors::OwoColorize;
use serde::Serialize;
use serde_json::json;
use std::path::Path;
use std::str::FromStr;

use super::decode;
use crate::cli::EvmContract;
use crate::config_source;
use crate::types::Network;

// ---------------------------------------------------------------------------
// Contract discovery from config files
// ---------------------------------------------------------------------------

struct EvmContractEntry {
    network: String,
    chain_name: String,
    rpc_url: String,
    label: String,
    address: Address,
}

async fn discover_contracts(
    network: Network,
    config_path: &Path,
    chain: &str,
    contract_filter: Option<EvmContract>,
) -> Vec<EvmContractEntry> {
    let type_filter = contract_filter.map(|c| match c {
        EvmContract::Gateway => "gateway",
        EvmContract::Its => "its",
        EvmContract::GasService => "gas-service",
    });

    let mut entries = Vec::new();

    let Ok(config_content) = tokio::fs::read_to_string(config_path).await else {
        return entries;
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&config_content) else {
        return entries;
    };
    let Some(chains) = config.get("chains").and_then(|v| v.as_object()) else {
        return entries;
    };

    for (chain_name, chain_config) in chains {
        let chain_type = chain_config
            .get("chainType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if chain_type != "evm" {
            continue;
        }

        if chain_name != chain {
            continue;
        }

        let rpc_url = match chain_config.get("rpc").and_then(|v| v.as_str()) {
            Some(r) => r.to_string(),
            None => continue,
        };

        let Some(contracts) = chain_config.get("contracts").and_then(|v| v.as_object()) else {
            continue;
        };

        let contract_map = [
            ("AxelarGateway", "gateway", "Gateway"),
            ("InterchainTokenService", "its", "ITS"),
            ("AxelarGasService", "gas-service", "GasService"),
        ];

        for (contract_name, prog_type, label) in &contract_map {
            if let Some(filter) = type_filter
                && *prog_type != filter
            {
                continue;
            }

            let addr_str = contracts
                .get(*contract_name)
                .and_then(|v| v.get("address").or(Some(v)))
                .and_then(|v| v.as_str());

            if let Some(addr_str) = addr_str
                && let Ok(address) = Address::from_str(addr_str)
            {
                entries.push(EvmContractEntry {
                    network: network.to_string(),
                    chain_name: chain_name.clone(),
                    rpc_url: rpc_url.clone(),
                    label: label.to_string(),
                    address,
                });
            }
        }
    }

    entries
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct EvmActivityEntry {
    network: String,
    chain: String,
    contract: String,
    contract_address: String,
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<String>,
    block: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_index: Option<u64>,
    params: serde_json::Value,
}

fn print_contract_header(entry: &EvmContractEntry) {
    let address = format!("{:x}", entry.address);
    let short = format!("0x{}...{}", &address[..4], &address[36..]);
    println!(
        "\n{}",
        format!(
            "━━ {} ({}/{}) {} ━━",
            entry.label, entry.network, entry.chain_name, short
        )
        .bold()
    );
}

fn print_event_line(block: u64, event_name: &str, params: &str, transaction_hash: Option<&str>) {
    let transaction = transaction_hash
        .map(|hash| {
            if hash.len() > 14 {
                format!("{}...", &hash[..14])
            } else {
                hash.to_string()
            }
        })
        .unwrap_or_default();
    println!(
        "  {} {:<42} {}  {}",
        format!("blk {block}").dimmed(),
        event_name.bold(),
        params.dimmed(),
        transaction.dimmed(),
    );
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

async fn collect(
    contract_filter: Option<EvmContract>,
    network: Network,
    chain: String,
    limit: usize,
    json_mode: bool,
) -> Result<Vec<EvmActivityEntry>> {
    let config_path = config_source::resolve(network, None).await?.into_path();
    let contracts = discover_contracts(network, &config_path, &chain, contract_filter).await;

    if contracts.is_empty() {
        return Err(eyre::eyre!(
            "no EVM contracts found for chain '{chain}' in the {network} chains config"
        ));
    }

    let mut all_entries: Vec<EvmActivityEntry> = Vec::new();

    for entry in &contracts {
        let provider = ProviderBuilder::new().connect_http(entry.rpc_url.parse()?);

        let Ok(latest_block) = provider.get_block_number().await else {
            if !json_mode {
                eprintln!(
                    "  {} could not reach {} ({})",
                    "!".yellow(),
                    entry.chain_name,
                    entry.rpc_url
                );
            }
            continue;
        };

        let from_block = latest_block.saturating_sub(10000);

        let filter = Filter::new()
            .address(entry.address)
            .from_block(from_block)
            .to_block(latest_block);

        let Ok(logs) = provider.get_logs(&filter).await else {
            continue;
        };

        if logs.is_empty() {
            continue;
        }

        // Take the last N logs (most recent)
        let recent_logs: Vec<_> = if logs.len() > limit {
            logs[logs.len() - limit..].to_vec()
        } else {
            logs
        };

        if !json_mode {
            print_contract_header(entry);
        }

        for log in &recent_logs {
            let topics: Vec<B256> = log.topics().to_vec();
            let data = log.data().data.as_ref();

            let (event_name, params) = match decode::decode_log(&topics, data) {
                Some((name, params)) => (name, params),
                None => {
                    let topic0 = topics
                        .first()
                        .map(|t| format!("0x{t:.8}..."))
                        .unwrap_or_default();
                    (format!("Unknown({topic0})"), vec![])
                }
            };

            // Extract just the event name (before the parentheses)
            let short_name = event_name.split('(').next().unwrap_or(&event_name);

            let tx_hash = log.transaction_hash.map(|h| format!("0x{h:x}"));
            let block_num = log.block_number.unwrap_or(0);
            let log_index = log.log_index;

            // Build params summary for human mode
            let params_summary = build_params_summary(short_name, &params);

            // Build JSON params
            let params_json = params_to_json(&params);

            if !json_mode {
                print_event_line(block_num, short_name, &params_summary, tx_hash.as_deref());
            }

            all_entries.push(EvmActivityEntry {
                network: entry.network.clone(),
                chain: entry.chain_name.clone(),
                contract: entry.label.clone(),
                contract_address: format!("0x{:x}", entry.address),
                event: short_name.to_string(),
                tx_hash,
                block: block_num,
                log_index,
                params: params_json,
            });
        }
    }

    Ok(all_entries)
}

/// Scan recent contract events without printing anything.
///
/// The scan prints each event as it walks the logs, so quiet mode is what
/// makes the same code usable by a caller that wants the data.
pub(crate) async fn resolve(
    contract_filter: Option<EvmContract>,
    network: Network,
    chain: String,
    limit: usize,
) -> Result<Vec<EvmActivityEntry>> {
    collect(contract_filter, network, chain, limit, true).await
}

pub async fn run(
    contract_filter: Option<EvmContract>,
    network: Network,
    chain: String,
    limit: usize,
    json_mode: bool,
) -> Result<()> {
    let all_entries = collect(contract_filter, network, chain, limit, json_mode).await?;

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&all_entries)?);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_params_summary(
    event_name: &str,
    params: &[(String, alloy::dyn_abi::DynSolValue)],
) -> String {
    let get = |name: &str| -> Option<String> {
        params.iter().find(|(n, _)| n == name).map(|(_, v)| {
            let s = format_sol_value(v);
            if s.len() > 20 {
                format!("{}...", &s[..20])
            } else {
                s
            }
        })
    };

    match event_name {
        "ContractCall" => {
            let dest = get("destinationChain").unwrap_or_default();
            let sender = get("sender").unwrap_or_default();
            format!("dest={dest} sender={sender}")
        }
        "InterchainTransfer" => {
            let dest = get("destinationChain").unwrap_or_default();
            let amount = get("amount").unwrap_or_default();
            format!("dest={dest} amount={amount}")
        }
        "InterchainTokenDeploymentStarted" | "InterchainTokenDeployed" => {
            let name = get("tokenName").or_else(|| get("name")).unwrap_or_default();
            let symbol = get("tokenSymbol")
                .or_else(|| get("symbol"))
                .unwrap_or_default();
            format!("{name} ({symbol})")
        }
        "NativeGasPaidForContractCall" | "GasPaidForContractCall" => {
            let dest = get("destinationChain").unwrap_or_default();
            let amount = get("gasFeeAmount").unwrap_or_default();
            format!("dest={dest} fee={amount}")
        }
        "ContractCallApproved" | "MessageApproved" => {
            let src = get("sourceChain").unwrap_or_default();
            format!("source={src}")
        }
        _ => {
            // Show first 2 params
            params
                .iter()
                .take(2)
                .map(|(n, v)| {
                    let s = format_sol_value(v);
                    let short = if s.len() > 16 {
                        format!("{}...", &s[..16])
                    } else {
                        s
                    };
                    format!("{n}={short}")
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

fn format_sol_value(value: &DynSolValue) -> String {
    match value {
        DynSolValue::String(s) => s.clone(),
        DynSolValue::Address(a) => format!("0x{a:x}"),
        DynSolValue::Uint(u, _) => format!("{u}"),
        DynSolValue::Int(i, _) => format!("{i}"),
        DynSolValue::Bool(b) => format!("{b}"),
        DynSolValue::FixedBytes(b, _) => format!("0x{}", hex::encode(b)),
        DynSolValue::Bytes(b) => {
            if b.len() <= 32 {
                format!("0x{}", hex::encode(b))
            } else {
                format!("0x{}...", hex::encode(&b[..16]))
            }
        }
        _ => format!("{value:?}"),
    }
}

fn params_to_json(params: &[(String, DynSolValue)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, value) in params {
        map.insert(name.clone(), sol_value_to_json(value));
    }
    serde_json::Value::Object(map)
}

/// Convert a decoded ABI value to JSON.
///
/// Shared with the calldata decoder: this is the leaf case of a decoded
/// argument tree, and having one converter keeps the two front ends agreeing
/// on how a uint or a byte string is represented.
pub(crate) fn sol_value_to_json(value: &DynSolValue) -> serde_json::Value {
    match value {
        DynSolValue::Bool(b) => json!(b),
        DynSolValue::Uint(u, _) => json!(format!("{u}")),
        DynSolValue::Int(i, _) => json!(format!("{i}")),
        DynSolValue::Address(a) => json!(format!("0x{a:x}")),
        DynSolValue::FixedBytes(b, _) => json!(format!("0x{}", hex::encode(b))),
        DynSolValue::Bytes(b) => {
            if b.len() <= 64 {
                json!(format!("0x{}", hex::encode(b)))
            } else {
                json!(format!(
                    "0x{}... ({} bytes)",
                    hex::encode(&b[..32]),
                    b.len()
                ))
            }
        }
        DynSolValue::String(s) => json!(s),
        DynSolValue::Array(arr) => {
            json!(arr.iter().map(sol_value_to_json).collect::<Vec<_>>())
        }
        DynSolValue::Tuple(arr) => {
            json!(arr.iter().map(sol_value_to_json).collect::<Vec<_>>())
        }
        _ => json!(format!("{value:?}")),
    }
}
