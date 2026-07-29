use std::path::Path;

use comfy_table::{Cell, ContentArrangement, Table};
use eyre::Result;
use serde_json::{Value, json};

use crate::commands::verifiers::lookup_name;
use crate::config_source;
use crate::cosmos::{read_axelar_contract_field, rpc_block_time, rpc_tx_search};
use crate::ui;

const SUPPORTED_NETWORKS: &[crate::types::Network] = &[
    crate::types::Network::Testnet,
    crate::types::Network::Mainnet,
];

fn resolve_chain_axelar_id(config_path: &Path, chain_input: &str) -> Result<String> {
    let content = std::fs::read_to_string(config_path)?;
    let root: Value = serde_json::from_str(&content)?;
    let chains = root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' in config"))?;

    if let Some(chain_config) = chains.get(chain_input) {
        return Ok(chain_config
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(chain_input)
            .to_string());
    }
    for (key, chain_config) in chains {
        let axelar_id = chain_config
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(key);
        if axelar_id.eq_ignore_ascii_case(chain_input) {
            return Ok(axelar_id.to_string());
        }
    }
    let mut available: Vec<&str> = chains.keys().map(|k| k.as_str()).collect();
    available.sort();
    Err(eyre::eyre!(
        "chain '{}' not found in config. Available: {}",
        chain_input,
        available.join(", ")
    ))
}

fn read_axelar_rpc_from(config_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(config_path)?;
    let root: Value = serde_json::from_str(&content)?;
    root.pointer("/axelar/rpc")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| eyre::eyre!("no axelar.rpc in config"))
}

#[derive(Debug)]
struct VoteRow {
    height: u64,
    tx_hash: String,
    poll_id: String,
    votes: Vec<String>, // raw vote strings: "succeeded_on_chain", "failed_on_chain", "not_found"
}

/// Translate a single vote string to a short label.
fn vote_label(v: &str) -> &'static str {
    match v {
        "succeeded_on_chain" => "Y",
        "failed_on_chain" => "F",
        "not_found" => "?",
        _ => "X",
    }
}

/// Format an ISO-8601 timestamp as a compact "X ago" string relative to now.
/// Returns "-" if the timestamp can't be parsed.
fn relative_time(iso: &str) -> String {
    use chrono::{DateTime, Utc};
    let parsed: Option<DateTime<Utc>> = iso.parse::<DateTime<Utc>>().ok().or_else(|| {
        DateTime::parse_from_rfc3339(iso)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    });
    let Some(t) = parsed else {
        return "-".to_string();
    };
    let now = Utc::now();
    let secs = (now - t).num_seconds();
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        let m = mins % 60;
        return if m == 0 {
            format!("{hours}h ago")
        } else {
            format!("{hours}h {m}m ago")
        };
    }
    let days = hours / 24;
    if days < 30 {
        let h = hours % 24;
        return if h == 0 {
            format!("{days}d ago")
        } else {
            format!("{days}d {h}h ago")
        };
    }
    let months = days / 30;
    let d = days % 30;
    if d == 0 {
        format!("{months}mo ago")
    } else {
        format!("{months}mo {d}d ago")
    }
}

/// Summarise a votes vector into a compact label (`Y`, `F`, `?`, or e.g. `Y,Y,F`).
fn vote_summary(votes: &[String]) -> String {
    if votes.is_empty() {
        return "-".to_string();
    }
    if votes.iter().all(|v| v == &votes[0]) {
        return vote_label(&votes[0]).to_string();
    }
    votes
        .iter()
        .map(|v| vote_label(v))
        .collect::<Vec<_>>()
        .join(",")
}

fn vote_row_from_event(
    transaction: &Value,
    event: &Value,
    verifier: &str,
    voting_verifier: &str,
) -> Option<VoteRow> {
    if event.get("type").and_then(Value::as_str) != Some("wasm-voted") {
        return None;
    }
    let mut poll_id = None;
    let mut votes = None;
    let mut voter = None;
    let mut contract = None;
    for attribute in event
        .get("attributes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let key = attribute.get("key").and_then(Value::as_str).unwrap_or("");
        let value = attribute.get("value").and_then(Value::as_str).unwrap_or("");
        match key {
            "poll_id" => poll_id = Some(value.trim_matches('"').to_string()),
            "votes" => votes = serde_json::from_str::<Vec<String>>(value).ok(),
            "voter" => voter = Some(value),
            "_contract_address" => contract = Some(value),
            _ => {}
        }
    }
    if voter != Some(verifier) || contract != Some(voting_verifier) {
        return None;
    }
    Some(VoteRow {
        height: transaction
            .get("height")
            .and_then(Value::as_str)
            .and_then(|height| height.parse().ok())
            .unwrap_or(0),
        tx_hash: transaction
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        poll_id: poll_id?,
        votes: votes.unwrap_or_default(),
    })
}

async fn query_vote_rows(
    rpc: &str,
    verifier: &str,
    voting_verifier: &str,
    limit: usize,
) -> Result<Vec<VoteRow>> {
    let filter = format!(
        "wasm-voted.voter='{verifier}' AND wasm-voted._contract_address='{voting_verifier}'"
    );
    let spinner = ui::wait_spinner("querying tx_search...");
    let mut rows = Vec::new();
    'pages: for page in 1..=20u32 {
        let result = rpc_tx_search(rpc, &filter, 100, page, true).await?;
        let transactions = result
            .get("txs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if transactions.is_empty() {
            break;
        }
        for transaction in &transactions {
            for event in transaction
                .pointer("/tx_result/events")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(row) =
                    vote_row_from_event(transaction, event, verifier, voting_verifier)
                {
                    rows.push(row);
                    if rows.len() >= limit {
                        break 'pages;
                    }
                }
            }
        }
        if transactions.len() < 100 {
            break;
        }
    }
    spinner.finish_and_clear();
    Ok(rows)
}

async fn fetch_block_timestamps(
    rpc: &str,
    rows: &[VoteRow],
) -> std::collections::HashMap<u64, String> {
    let mut timestamps = std::collections::HashMap::new();
    let mut heights: Vec<u64> = rows.iter().map(|row| row.height).collect();
    heights.sort_unstable();
    heights.dedup();
    let spinner = ui::wait_spinner("fetching block timestamps...");
    for height in heights.into_iter().take(60) {
        if let Ok(timestamp) = rpc_block_time(rpc, height).await {
            timestamps.insert(height, timestamp);
        }
    }
    spinner.finish_and_clear();
    timestamps
}

fn print_json_votes(
    verifier: &str,
    display_name: Option<&str>,
    voting_verifier: &str,
    chain: &str,
    rows: &[VoteRow],
    timestamps: &std::collections::HashMap<u64, String>,
) -> Result<()> {
    let entries = json!({
        "verifier": verifier,
        "name": display_name,
        "voting_verifier": voting_verifier,
        "chain": chain,
        "votes": rows.iter().map(|row| {
            json!({
                "height": row.height,
                "tx_hash": row.tx_hash,
                "time": timestamps.get(&row.height).cloned().unwrap_or_default(),
                "poll_id": row.poll_id,
                "votes": row.votes,
                "summary": vote_summary(&row.votes),
            })
        }).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}

fn print_vote_table(
    verifier_display: &str,
    voting_verifier: &str,
    chain: &str,
    limit: usize,
    rows: &[VoteRow],
    timestamps: &std::collections::HashMap<u64, String>,
) {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["#", "Height", "When", "Poll", "#Msgs", "Vote"]);
    for (index, row) in rows.iter().enumerate() {
        let when = timestamps
            .get(&row.height)
            .map(|timestamp| relative_time(timestamp))
            .unwrap_or_else(|| "-".to_string());
        table.add_row(vec![
            Cell::new(index + 1),
            Cell::new(row.height),
            Cell::new(when),
            Cell::new(&row.poll_id),
            Cell::new(row.votes.len()),
            Cell::new(vote_summary(&row.votes)),
        ]);
    }
    println!("\n{table}\n");
    let all_yes = rows
        .iter()
        .filter(|row| row.votes.iter().all(|vote| vote == "succeeded_on_chain"))
        .count();
    ui::kv("verifier", verifier_display);
    ui::kv("voting-verifier", &format!("{voting_verifier} ({chain})"));
    ui::kv(
        "showing",
        &format!("{} most recent votes (limit={limit})", rows.len()),
    );
    ui::kv(
        "summary",
        &format!(
            "{all_yes} all-yes, {} contained any non-yes",
            rows.len() - all_yes
        ),
    );
}

pub async fn run(
    network: crate::types::Network,
    chain: String,
    verifier: String,
    limit: usize,
    json_mode: bool,
) -> Result<()> {
    if !SUPPORTED_NETWORKS.contains(&network) {
        return Err(eyre::eyre!(
            "verifier-votes only supports: {}",
            SUPPORTED_NETWORKS
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let config_path = config_source::resolve(network, None).await?.into_path();
    let chain_axelar_id = resolve_chain_axelar_id(&config_path, &chain)?;
    let rpc = read_axelar_rpc_from(&config_path)?;

    let vv_pointer = format!("/axelar/contracts/VotingVerifier/{chain_axelar_id}/address");
    let vv_addr = read_axelar_contract_field(&config_path, &vv_pointer).map_err(|_| {
        eyre::eyre!(
            "no VotingVerifier address for chain '{chain_axelar_id}' on {network}. Is it Amplifier?"
        )
    })?;

    let display_name = lookup_name(network, &verifier);
    let verifier_display = match display_name {
        Some(name) => format!("{name} ({verifier})"),
        None => verifier.clone(),
    };

    if !json_mode {
        ui::section(&format!(
            "Verifier votes: {network} / {chain_axelar_id} / {verifier_display}"
        ));
    }

    let rows = query_vote_rows(&rpc, &verifier, &vv_addr, limit).await?;
    if rows.is_empty() {
        return Err(eyre::eyre!(
            "no wasm-voted events found for {} on {} (voting-verifier {}).",
            verifier,
            chain_axelar_id,
            vv_addr
        ));
    }

    let timestamps = fetch_block_timestamps(&rpc, &rows).await;

    if json_mode {
        return print_json_votes(
            &verifier,
            display_name,
            &vv_addr,
            &chain_axelar_id,
            &rows,
            &timestamps,
        );
    }
    print_vote_table(
        &verifier_display,
        &vv_addr,
        &chain_axelar_id,
        limit,
        &rows,
        &timestamps,
    );
    Ok(())
}
