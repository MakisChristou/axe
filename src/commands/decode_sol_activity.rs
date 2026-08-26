use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use eyre::Result;
use owo_colors::OwoColorize;
use serde::Serialize;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::UiTransactionEncoding;
use tokio::task::spawn_blocking;

use super::decode_sol_tx;
use crate::cli::SolProgram;
use crate::config_source;
use crate::types::Network;

// ---------------------------------------------------------------------------
// Config discovery
// ---------------------------------------------------------------------------

struct DiscoveredProgram {
    network: String,
    chain_name: String,
    rpc_url: String,
    label: String,
    address: String,
}

async fn discover_programs(
    configs: &[(Network, PathBuf)],
    program_filter: Option<SolProgram>,
) -> Vec<DiscoveredProgram> {
    let program_type_filter = program_filter.map(|p| match p {
        SolProgram::Gateway => "gateway",
        SolProgram::Its => "its",
        SolProgram::GasService => "gas-service",
        SolProgram::Memo => "memo",
    });

    let mut programs = Vec::new();

    for (network, config_path) in configs {
        let Ok(config_content) = tokio::fs::read_to_string(config_path).await else {
            continue;
        };
        let config: serde_json::Value = match serde_json::from_str(&config_content) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let Some(chains) = config.get("chains").and_then(|v| v.as_object()) else {
            continue;
        };

        // Find SVM chains and their RPC
        for (chain_name, chain_config) in chains {
            let chain_type = chain_config
                .get("chainType")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if chain_type != "svm" {
                continue;
            }

            let rpc_url = match chain_config.get("rpc").and_then(|v| v.as_str()) {
                Some(r) => r.to_string(),
                None => continue,
            };

            let Some(contracts) = chain_config.get("contracts").and_then(|v| v.as_object()) else {
                continue;
            };

            // Map contract names to our program types
            let contract_map = [
                ("AxelarGateway", "gateway", "Gateway"),
                ("AxelarGasService", "gas-service", "GasService"),
                ("AxelarMemo", "memo", "Memo"),
                ("InterchainTokenService", "its", "ITS"),
            ];

            for (contract_name, prog_type, label) in &contract_map {
                if let Some(filter) = program_type_filter
                    && *prog_type != filter
                {
                    continue;
                }

                let address = contracts
                    .get(*contract_name)
                    .and_then(|v| v.get("address").or(Some(v)))
                    .and_then(|v| v.as_str());

                if let Some(addr) = address {
                    programs.push(DiscoveredProgram {
                        network: network.to_string(),
                        chain_name: chain_name.clone(),
                        rpc_url: rpc_url.clone(),
                        label: label.to_string(),
                        address: addr.to_string(),
                    });
                }
            }
        }
    }

    programs
}

// ---------------------------------------------------------------------------
// JSON output struct
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct ActivityEntry {
    network: String,
    chain: String,
    program: String,
    program_address: String,
    signature: String,
    slot: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<i64>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    events: Vec<String>,
}

fn print_program_header(entry: &DiscoveredProgram) {
    println!(
        "\n{}",
        format!(
            "━━ {} ({}/{}) {} ━━",
            entry.label, entry.network, entry.chain_name, entry.address
        )
        .bold()
    );
}

fn print_activity_line(
    signature: &str,
    block_time: Option<i64>,
    succeeded: bool,
    instruction: Option<&str>,
    events: &[String],
) {
    let time = block_time
        .map(format_timestamp)
        .unwrap_or_else(|| "?".to_string());
    let status = if succeeded {
        format!("{}", "OK".green())
    } else {
        format!("{}", "FAIL".red())
    };
    let signature = if signature.len() > 20 {
        format!("{}...", &signature[..20])
    } else {
        signature.to_string()
    };
    let events = if events.is_empty() {
        String::new()
    } else {
        format!(" → {}", events.join(", ").dimmed())
    };
    println!(
        "  {} [{}] {:<45} {}{}",
        time.dimmed(),
        status,
        instruction.unwrap_or("?").bold(),
        signature.dimmed(),
        events,
    );
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Find the programs to scan, shared by both front ends.
async fn programs_to_scan(
    program_filter: Option<SolProgram>,
    network: Option<Network>,
) -> Result<Vec<DiscoveredProgram>> {
    let networks: Vec<Network> = match network {
        Some(n) => vec![n],
        None => Network::ALL.to_vec(),
    };
    let mut configs: Vec<(Network, PathBuf)> = Vec::new();
    for net in networks {
        if let Ok(source) = config_source::resolve(net, None).await {
            configs.push((net, source.into_path()));
        }
    }
    if configs.is_empty() {
        return Err(eyre::eyre!(
            "no chains config found for any network. Set CHAINS_CONFIG or connect to the network."
        ));
    }

    let programs = discover_programs(&configs, program_filter).await;

    if programs.is_empty() {
        return Err(eyre::eyre!("no Solana programs found in chains config(s)"));
    }

    Ok(programs)
}

/// Scan recent program activity without printing anything.
///
/// The scan prints its human output as it walks each program, so quiet mode
/// is what makes the same code usable by a caller that wants the data.
pub(crate) async fn resolve(
    program_filter: Option<SolProgram>,
    network: Option<Network>,
    limit: usize,
) -> Result<Vec<ActivityEntry>> {
    let programs = programs_to_scan(program_filter, network).await?;
    let known = decode_sol_tx::known_programs();
    Ok(spawn_blocking(move || scan_program_activity(&programs, &known, limit, true)).await?)
}

pub async fn run(
    program_filter: Option<SolProgram>,
    network: Option<Network>,
    limit: usize,
    json_mode: bool,
) -> Result<()> {
    let programs = programs_to_scan(program_filter, network).await?;
    let known = decode_sol_tx::known_programs();
    let all_entries =
        spawn_blocking(move || scan_program_activity(&programs, &known, limit, json_mode)).await?;

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&all_entries)?);
    }

    Ok(())
}

fn scan_program_activity(
    programs: &[DiscoveredProgram],
    known: &HashMap<Pubkey, &'static str>,
    limit: usize,
    json_mode: bool,
) -> Vec<ActivityEntry> {
    let mut all_entries = Vec::new();

    for entry in programs {
        let Ok(pubkey) = Pubkey::from_str(&entry.address) else {
            continue;
        };

        let rpc = crate::solana::rpc_client(&entry.rpc_url);

        let Ok(sigs) = rpc.get_signatures_for_address_with_config(
            &pubkey,
            solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                limit: Some(limit),
                ..Default::default()
            },
        ) else {
            continue;
        };

        if sigs.is_empty() {
            continue;
        }

        if !json_mode {
            print_program_header(entry);
        }

        for sig_info in &sigs {
            let status = if sig_info.err.is_some() {
                "Failed"
            } else {
                "Success"
            };

            let sig = &sig_info.signature;
            let slot = sig_info.slot;
            let block_time = sig_info.block_time;

            let (ix_name, args_json, events) = fetch_and_decode(&rpc, sig, known);

            if !json_mode {
                print_activity_line(
                    sig,
                    block_time,
                    sig_info.err.is_none(),
                    ix_name.as_deref(),
                    &events,
                );
            }

            all_entries.push(ActivityEntry {
                network: entry.network.clone(),
                chain: entry.chain_name.clone(),
                program: entry.label.clone(),
                program_address: entry.address.clone(),
                signature: sig.clone(),
                slot,
                timestamp: block_time,
                status: status.to_string(),
                instruction: ix_name,
                args: args_json,
                events,
            });
        }
    }

    all_entries
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fetch_and_decode(
    rpc: &RpcClient,
    sig_str: &str,
    known: &HashMap<Pubkey, &'static str>,
) -> (Option<String>, Option<serde_json::Value>, Vec<String>) {
    let Ok(sig) = Signature::from_str(sig_str) else {
        return (None, None, vec![]);
    };

    let Ok(tx) = rpc.get_transaction_with_config(
        &sig,
        RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Json),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        },
    ) else {
        return (None, None, vec![]);
    };

    // Extract account keys
    let account_keys: Vec<Pubkey> = match &tx.transaction.transaction {
        solana_transaction_status::EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            solana_transaction_status::UiMessage::Raw(raw) => raw
                .account_keys
                .iter()
                .filter_map(|k| Pubkey::from_str(k).ok())
                .collect(),
            _ => vec![],
        },
        _ => vec![],
    };

    let mut all_keys = account_keys;
    if let Some(meta) = &tx.transaction.meta
        && let solana_transaction_status::option_serializer::OptionSerializer::Some(loaded) =
            &meta.loaded_addresses
    {
        for k in &loaded.writable {
            if let Ok(pk) = Pubkey::from_str(k) {
                all_keys.push(pk);
            }
        }
        for k in &loaded.readonly {
            if let Ok(pk) = Pubkey::from_str(k) {
                all_keys.push(pk);
            }
        }
    }

    // Find the primary Axelar instruction (skip ComputeBudget)
    let mut ix_name: Option<String> = None;
    let mut args_json: Option<serde_json::Value> = None;

    if let solana_transaction_status::EncodedTransaction::Json(ui_tx) = &tx.transaction.transaction
        && let solana_transaction_status::UiMessage::Raw(raw) = &ui_tx.message
    {
        for ix in &raw.instructions {
            let program_idx = ix.program_id_index as usize;
            let program_id = all_keys.get(program_idx);
            let program_label = program_id.and_then(|pk| known.get(pk)).copied();

            if program_label == Some("ComputeBudget") {
                continue;
            }

            if program_label.is_some() {
                let data_bytes = bs58::decode(&ix.data).into_vec().unwrap_or_default();
                if let Some(name) = decode_sol_tx::instruction_name(&data_bytes) {
                    args_json = Some(decode_sol_tx::decode_instruction_args_json(
                        name,
                        &data_bytes,
                    ));
                    ix_name = Some(name.to_string());
                    break;
                }
            }
        }
    }

    // Extract events from inner instructions
    let mut events = Vec::new();
    if let Some(meta) = &tx.transaction.meta
        && let solana_transaction_status::option_serializer::OptionSerializer::Some(inner_ixs) =
            &meta.inner_instructions
    {
        for group in inner_ixs {
            for ix in &group.instructions {
                if let solana_transaction_status::UiInstruction::Compiled(ci) = ix {
                    let data_bytes = bs58::decode(&ci.data).into_vec().unwrap_or_default();
                    if data_bytes.len() >= 16
                        && data_bytes[..8] == *decode_sol_tx::EVENT_IX_TAG_LE
                        && let Some(name) = decode_sol_tx::event_name(&data_bytes[8..16])
                    {
                        events.push(name.to_string());
                    }
                }
            }
        }
    }

    (ix_name, args_json, events)
}

fn format_timestamp(ts: i64) -> String {
    let secs = ts;
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    let mut y = 1970i64;
    let mut remaining_days = days_since_epoch;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let month_days = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0usize;
    for (i, &d) in month_days.iter().enumerate() {
        if remaining_days < d {
            m = i;
            break;
        }
        remaining_days -= d;
    }

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m + 1,
        remaining_days + 1,
        hours,
        minutes
    )
}
