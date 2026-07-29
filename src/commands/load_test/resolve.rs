//! Config + cache resolution for the load-test command:
//! `LoadTestArgs` → `ResolvedConfig`, JSON cache file IO, the auto-detect
//! heuristics that pick a `(source, destination)` pair when the user only
//! supplies `--config`, and the cargo-feature-driven network sanity check.

use std::collections::HashMap;
use std::path::PathBuf;

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use super::TestType;
use super::chain_names::{AxelarChainId, ConfigChainId, RpcUrl};
use crate::config::{ChainConfig, ChainsConfig};
use crate::ui;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct GmpCache {
    #[serde(
        rename = "senderReceiverAddress",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) sender_receiver_address: Option<String>,
    // Cache files are a forward-compatible external boundary. Preserve fields
    // written by newer AXE versions when updating the values this build knows.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ItsCache {
    #[serde(rename = "tokenId", skip_serializing_if = "Option::is_none")]
    pub(crate) token_id: Option<String>,
    #[serde(rename = "tokenAddress", skip_serializing_if = "Option::is_none")]
    pub(crate) token_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) salt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mint: Option<String>,
    // See `GmpCache::extra`: unknown persisted fields must round-trip rather
    // than disappear when an older binary updates one known field.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

pub(crate) fn cache_path(axelar_id: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("load-test-{axelar_id}.json"))
}

fn read_json_cache<T>(path: &std::path::Path) -> T
where
    T: Default + serde::de::DeserializeOwned,
{
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

pub(crate) fn read_cache(axelar_id: &str) -> GmpCache {
    read_json_cache(&cache_path(axelar_id))
}

pub(crate) fn save_cache(axelar_id: &str, cache: &GmpCache) -> Result<()> {
    let path = cache_path(axelar_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}
fn chain_type(chains: &HashMap<String, ChainConfig>, chain_id: &str) -> Option<String> {
    chains.get(chain_id)?.chain_type.clone()
}

/// Find chains by chainType, optionally skipping core-* prefixed chains.
fn find_chains_by_type(
    chains: &HashMap<String, ChainConfig>,
    chain_type_filter: &str,
    skip_core: bool,
) -> Vec<String> {
    chains
        .iter()
        .filter(|(k, v)| {
            v.chain_type.as_deref() == Some(chain_type_filter)
                && !(skip_core && k.starts_with("core-"))
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Infer test type from source and destination chain types.
pub(crate) fn infer_test_type(source_type: &str, dest_type: &str) -> Result<TestType> {
    match (source_type, dest_type) {
        ("svm", "evm") => Ok(TestType::SolToEvm),
        ("evm", "svm") => Ok(TestType::EvmToSol),
        ("evm", "evm") => Ok(TestType::EvmToEvm),
        ("svm", "svm") => Ok(TestType::SolToSol),
        ("xrpl", "evm") => Ok(TestType::XrplToEvm),
        ("evm", "xrpl") => Ok(TestType::EvmToXrpl),
        ("stellar", "evm") => Ok(TestType::StellarToEvm),
        ("evm", "stellar") => Ok(TestType::EvmToStellar),
        ("stellar", "svm") => Ok(TestType::StellarToSol),
        ("svm", "stellar") => Ok(TestType::SolToStellar),
        ("sui", "evm") => Ok(TestType::SuiToEvm),
        ("evm", "sui") => Ok(TestType::EvmToSui),
        ("sui", "svm") => Ok(TestType::SuiToSol),
        ("svm", "sui") => Ok(TestType::SolToSui),
        ("sui", "stellar") => Ok(TestType::SuiToStellar),
        ("stellar", "sui") => Ok(TestType::StellarToSui),
        ("sui", "xrpl") => Ok(TestType::SuiToXrpl),
        ("xrpl", "sui") => Ok(TestType::XrplToSui),
        _ => Err(eyre::eyre!(
            "unsupported chain type combination: {source_type} -> {dest_type}. \
             Supported: svm -> evm, evm -> svm, evm -> evm, svm -> svm, \
             xrpl -> evm, evm -> xrpl, stellar -> evm, evm -> stellar, \
             stellar -> svm, svm -> stellar, sui <-> {{evm, svm, stellar, xrpl}}"
        )),
    }
}
pub(crate) struct ResolvedConfig {
    pub test_type: TestType,
    pub source_chain: ConfigChainId,
    pub destination_chain: ConfigChainId,
    /// The `axelarId` for the source chain — may differ from the JSON key
    /// (e.g. `"Avalanche"` vs `"avalanche"` for consensus chains).
    pub source_axelar_id: AxelarChainId,
    /// The `axelarId` for the destination chain.
    pub destination_axelar_id: AxelarChainId,
    pub source_rpc: RpcUrl,
    pub destination_rpc: RpcUrl,
    pub private_key: Option<String>,
}

/// Resolve chains, RPCs, and test type from the config JSON.
///
/// Supports three modes:
pub(crate) fn resolve_from_config(
    config: &PathBuf,
    test_type_override: Option<TestType>,
    source_chain_override: Option<String>,
    destination_chain_override: Option<String>,
    private_key_override: Option<String>,
    source_rpc_override: Option<String>,
    destination_rpc_override: Option<String>,
) -> Result<ResolvedConfig> {
    let config_content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let config_root = ChainsConfig::from_json_str(&config_content)
        .with_context(|| format!("no 'chains' object in config {}", config.display()))?;
    let chains = &config_root.chains;

    // --- Resolve test type + chains ---
    let (test_type, source_chain, destination_chain) = match (
        test_type_override,
        source_chain_override,
        destination_chain_override,
    ) {
        // Case 1: Both chains given → infer test type
        (None, Some(src), Some(dst)) => {
            let src_type = chain_type(chains, &src)
                .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?;
            let dst_type = chain_type(chains, &dst)
                .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?;
            let tt = infer_test_type(&src_type, &dst_type)?;
            ui::info(&format!("inferred test type: {tt}"));
            (tt, src, dst)
        }
        // Case 2: Test type + optional overrides → auto-detect missing chains
        (Some(tt), src_opt, dst_opt) => {
            let (src, dst) = auto_detect_chains(chains, tt, src_opt, dst_opt)?;
            (tt, src, dst)
        }
        // Case 3: Nothing or partial → try to auto-detect everything
        (None, src_opt, dst_opt) => {
            // Try to find a valid combination from config
            let (tt, src, dst) = auto_detect_all(chains, src_opt.as_ref(), dst_opt)?;
            (tt, src, dst)
        }
    };

    // Resolve axelarId — consensus chains use a capitalised name on the Cosmos
    // side (e.g. "Avalanche" vs the JSON key "avalanche"). We keep the JSON
    // key for contract lookups but store the axelarId for verification queries.
    let source_axelar_id = chains
        .get(&source_chain)
        .and_then(|c| c.axelar_id.clone())
        .unwrap_or_else(|| source_chain.clone());
    let destination_axelar_id = chains
        .get(&destination_chain)
        .and_then(|c| c.axelar_id.clone())
        .unwrap_or_else(|| destination_chain.clone());

    // --- Read RPCs ---
    let source_rpc = chains
        .get(&source_chain)
        .and_then(|c| c.rpc.clone())
        .ok_or_else(|| eyre::eyre!("no RPC URL for source chain '{source_chain}' in config"))?;
    let destination_rpc = chains
        .get(&destination_chain)
        .and_then(|c| c.rpc.clone())
        .ok_or_else(|| {
            eyre::eyre!("no RPC URL for destination chain '{destination_chain}' in config")
        })?;

    // Treat an empty override the same as no override (callers passing
    // `--source-rpc ""` or an unset env var that became Some("") should fall
    // through to the config default, not bail with "invalid RPC URL ''").
    let resolved_source_rpc = source_rpc_override
        .filter(|s| !s.is_empty())
        .unwrap_or(source_rpc);
    let resolved_destination_rpc = destination_rpc_override
        .filter(|s| !s.is_empty())
        .unwrap_or(destination_rpc);
    // RPC URLs are intentionally not printed — keeps secret/private endpoints
    // out of run logs. GH Actions also auto-masks values that came from
    // secrets, but we'd rather not surface RPCs at all.

    Ok(ResolvedConfig {
        test_type,
        source_chain: ConfigChainId::new(source_chain)?,
        destination_chain: ConfigChainId::new(destination_chain)?,
        source_axelar_id: AxelarChainId::new(source_axelar_id)?,
        destination_axelar_id: AxelarChainId::new(destination_axelar_id)?,
        source_rpc: RpcUrl::new(resolved_source_rpc)?,
        destination_rpc: RpcUrl::new(resolved_destination_rpc)?,
        private_key: private_key_override,
    })
}

/// Auto-detect source/destination chains for a known test type.
fn detect_unique_source(
    chains: &HashMap<String, ChainConfig>,
    override_: Option<String>,
    chain_type: &str,
    skip_core: bool,
    missing_label: &str,
    multiple_label: &str,
) -> Result<String> {
    if let Some(source) = override_ {
        return Ok(source);
    }
    let found = find_chains_by_type(chains, chain_type, skip_core);
    match found.as_slice() {
        [] => Err(eyre::eyre!("no {missing_label} chain found in config")),
        [source] => {
            ui::info(&format!("auto-detected source: {source}"));
            Ok(source.clone())
        }
        _ => Err(eyre::eyre!(
            "multiple {multiple_label} chains found: {}. Use --source-chain to pick one.",
            found.join(", ")
        )),
    }
}

fn detect_first_destination(
    chains: &HashMap<String, ChainConfig>,
    override_: Option<String>,
    chain_type: &str,
    skip_core: bool,
    missing_label: &str,
) -> Result<String> {
    if let Some(destination) = override_ {
        return Ok(destination);
    }
    let found = find_chains_by_type(chains, chain_type, skip_core);
    let destination = found
        .first()
        .ok_or_else(|| eyre::eyre!("no {missing_label} chain found in config"))?;
    ui::info(&format!(
        "auto-detected destination: {destination} (use --destination-chain to override)"
    ));
    Ok(destination.clone())
}

fn auto_detect_chains(
    chains: &HashMap<String, ChainConfig>,
    test_type: TestType,
    source_override: Option<String>,
    dest_override: Option<String>,
) -> Result<(String, String)> {
    match test_type {
        TestType::SolToEvm => {
            let source =
                detect_unique_source(chains, source_override, "svm", false, "SVM (Solana)", "SVM")?;
            let dest = detect_first_destination(chains, dest_override, "evm", true, "EVM")?;
            Ok((source, dest))
        }
        TestType::EvmToSol => {
            let source = detect_unique_source(chains, source_override, "evm", true, "EVM", "EVM")?;
            let dest =
                detect_first_destination(chains, dest_override, "svm", false, "SVM (Solana)")?;
            Ok((source, dest))
        }
        TestType::EvmToEvm => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    if evm.len() < 2 {
                        return Err(eyre::eyre!(
                            "need at least 2 EVM chains in config for evm-to-evm"
                        ));
                    }
                    ui::info(&format!("auto-detected source: {}", evm[0]));
                    evm[0].clone()
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    let picked = evm
                        .iter()
                        .find(|c| **c != source)
                        .ok_or_else(|| eyre::eyre!("need at least 2 EVM chains for evm-to-evm"))?;
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        picked
                    ));
                    picked.clone()
                }
            };
            Ok((source, dest))
        }
        TestType::SolToSol => {
            let source = source_override.map_or_else(
                || {
                    let svm = find_chains_by_type(chains, "svm", false);
                    let source = svm
                        .first()
                        .ok_or_else(|| eyre::eyre!("no SVM (Solana) chain found in config"))?;
                    ui::info(&format!("auto-detected source: {source}"));
                    Ok::<_, eyre::Report>(source.clone())
                },
                Ok,
            )?;
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    // For sol-to-sol, default to the same chain (loopback)
                    ui::info(&format!(
                        "auto-detected destination: {} (same as source for sol-to-sol)",
                        source
                    ));
                    source.clone()
                }
            };
            Ok((source, dest))
        }
        TestType::XrplToEvm => {
            let source =
                detect_unique_source(chains, source_override, "xrpl", false, "XRPL", "XRPL")?;
            let dest = detect_first_destination(chains, dest_override, "evm", true, "EVM")?;
            Ok((source, dest))
        }
        TestType::EvmToXrpl => {
            let source = detect_unique_source(chains, source_override, "evm", true, "EVM", "EVM")?;
            let dest = detect_first_destination(chains, dest_override, "xrpl", false, "XRPL")?;
            Ok((source, dest))
        }
        TestType::StellarToEvm
        | TestType::EvmToStellar
        | TestType::StellarToSol
        | TestType::SolToStellar => {
            auto_detect_stellar_pair(chains, test_type, source_override, dest_override)
        }
        TestType::SuiToEvm
        | TestType::EvmToSui
        | TestType::SuiToSol
        | TestType::SolToSui
        | TestType::SuiToStellar
        | TestType::StellarToSui
        | TestType::SuiToXrpl
        | TestType::XrplToSui => {
            // Sui as source/dest: just require both chains explicitly to keep
            // the auto-detect simple. The user knows which env they're on.
            let source = source_override
                .ok_or_else(|| eyre::eyre!("{test_type} requires --source-chain"))?;
            let dest = dest_override
                .ok_or_else(|| eyre::eyre!("{test_type} requires --destination-chain"))?;
            Ok((source, dest))
        }
    }
}

fn auto_detect_stellar_pair(
    chains: &HashMap<String, ChainConfig>,
    tt: TestType,
    src_override: Option<String>,
    dst_override: Option<String>,
) -> Result<(String, String)> {
    let (src_type, dst_type): (&str, &str) = match tt {
        TestType::StellarToEvm => ("stellar", "evm"),
        TestType::EvmToStellar => ("evm", "stellar"),
        TestType::StellarToSol => ("stellar", "svm"),
        TestType::SolToStellar => ("svm", "stellar"),
        _ => unreachable!(),
    };
    let skip_core_on_evm = |t: &str| t == "evm";
    let pick = |t: &str, override_: Option<String>, role: &str| -> Result<String> {
        if let Some(x) = override_ {
            return Ok(x);
        }
        let found = find_chains_by_type(chains, t, skip_core_on_evm(t));
        match found.len() {
            0 => Err(eyre::eyre!("no {} chain found in config", t)),
            1 => {
                ui::info(&format!("auto-detected {role}: {}", found[0]));
                Ok(found[0].clone())
            }
            _ => {
                ui::info(&format!(
                    "auto-detected {role}: {} (multiple available, pass --{role}-chain to override)",
                    found[0]
                ));
                Ok(found[0].clone())
            }
        }
    };
    let source = pick(src_type, src_override, "source")?;
    let dest = pick(dst_type, dst_override, "destination")?;
    Ok((source, dest))
}

/// Auto-detect test type and chains when nothing is specified.
/// Looks at what chain types exist in the config and picks the best match.
fn auto_detect_all(
    chains: &HashMap<String, ChainConfig>,
    source_override: Option<&String>,
    dest_override: Option<String>,
) -> Result<(TestType, String, String)> {
    // If one chain is given, figure out the other
    if let Some(src) = source_override {
        let src_type = chain_type(chains, src)
            .ok_or_else(|| eyre::eyre!("source chain '{src}' not found in config"))?;
        if src_type == "svm" {
            let evm = find_chains_by_type(chains, "evm", true);
            let dst = dest_override.unwrap_or_else(|| {
                ui::info(&format!(
                    "auto-detected destination: {} (use --destination-chain to override)",
                    evm[0]
                ));
                evm[0].clone()
            });
            ui::info("inferred test type: sol-to-evm");
            return Ok((TestType::SolToEvm, src.clone(), dst));
        }
        if src_type == "evm" {
            let svm = find_chains_by_type(chains, "svm", false);
            if svm.is_empty() {
                return Err(eyre::eyre!(
                    "no SVM chain found in config to pair with EVM source"
                ));
            }
            let dst = dest_override.unwrap_or_else(|| {
                ui::info(&format!(
                    "auto-detected destination: {} (use --destination-chain to override)",
                    svm[0]
                ));
                svm[0].clone()
            });
            ui::info("inferred test type: evm-to-sol");
            return Ok((TestType::EvmToSol, src.clone(), dst));
        }
        return Err(eyre::eyre!(
            "cannot infer test type from source chain type '{src_type}'. Use --test-type to specify."
        ));
    }

    if let Some(ref dst) = dest_override {
        let dst_type = chain_type(chains, dst)
            .ok_or_else(|| eyre::eyre!("destination chain '{dst}' not found in config"))?;
        if dst_type == "evm" {
            let svm = find_chains_by_type(chains, "svm", false);
            if svm.is_empty() {
                return Err(eyre::eyre!(
                    "no SVM chain found in config to pair with EVM destination"
                ));
            }
            ui::info(&format!("auto-detected source: {}", svm[0]));
            ui::info("inferred test type: sol-to-evm");
            return Ok((TestType::SolToEvm, svm[0].clone(), dst.clone()));
        }
        if dst_type == "svm" {
            let evm = find_chains_by_type(chains, "evm", true);
            if evm.is_empty() {
                return Err(eyre::eyre!(
                    "no EVM chain found in config to pair with SVM destination"
                ));
            }
            ui::info(&format!("auto-detected source: {}", evm[0]));
            ui::info("inferred test type: evm-to-sol");
            return Ok((TestType::EvmToSol, evm[0].clone(), dst.clone()));
        }
        return Err(eyre::eyre!(
            "cannot infer test type from destination chain type '{dst_type}'. Use --test-type to specify."
        ));
    }

    // Nothing specified — look for valid combinations
    let svm = find_chains_by_type(chains, "svm", false);
    let evm = find_chains_by_type(chains, "evm", true);

    if !svm.is_empty() && !evm.is_empty() {
        ui::info(&format!(
            "auto-detected: {} -> {} (sol-to-evm)",
            svm[0], evm[0]
        ));
        return Ok((TestType::SolToEvm, svm[0].clone(), evm[0].clone()));
    }

    Err(eyre::eyre!(
        "cannot auto-detect test type from config. Use --test-type, or --source-chain + --destination-chain."
    ))
}
pub(crate) fn its_cache_path(src: &str, dst: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("its-load-test-{src}-{dst}.json"))
}

pub(crate) fn read_its_cache(src: &str, dst: &str) -> ItsCache {
    read_json_cache(&its_cache_path(src, dst))
}

pub(crate) fn save_its_cache(src: &str, dst: &str, cache: &ItsCache) -> Result<()> {
    let path = its_cache_path(src, dst);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}

/// Find the interchain-token deploy `salt` for a given `tokenId` by scanning the
/// local ITS caches (`its-load-test-*.json`). axe records `{tokenId, salt}` for
/// every token it deploys, so an existing AXE can be remote-deployed to a new
/// chain without re-minting. Matches `tokenId` case-insensitively, with or
/// without the `0x` prefix. Returns the salt hex (as stored).
pub(crate) fn find_cached_salt(token_id: &str) -> Option<String> {
    let want = token_id.trim_start_matches("0x").to_lowercase();
    let dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("its-load-test-") && name.ends_with(".json")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(cache) = serde_json::from_str::<ItsCache>(&text) else {
            continue;
        };
        let tid = cache.token_id.as_deref().unwrap_or_default();
        if tid.trim_start_matches("0x").to_lowercase() == want
            && let Some(salt) = cache.salt
        {
            return Some(salt);
        }
    }
    None
}

/// Try to detect the target network from the config file path.
/// Looks for known network names in the filename (e.g. "stagenet.json", "devnet-amplifier.json").
pub(crate) fn detect_network_from_config(
    config: &std::path::Path,
) -> Option<crate::types::Network> {
    let name = config.file_stem()?.to_str()?;
    crate::types::Network::ALL
        .into_iter()
        .find(|network| name == network.as_str())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn cache_fixture(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "axe-{name}-{}-{}.json",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn missing_cache_is_an_empty_object() {
        let path = cache_fixture("missing-cache");

        let cache: ItsCache = read_json_cache(&path);

        assert!(cache.token_id.is_none());
        assert!(cache.extra.is_empty());
    }

    #[test]
    fn malformed_cache_preserves_empty_cache_fallback() {
        let path = cache_fixture("malformed-cache");
        std::fs::write(&path, "{").unwrap();

        let cache = read_json_cache::<ItsCache>(&path);
        std::fs::remove_file(&path).unwrap();

        assert!(cache.token_id.is_none());
        assert!(cache.extra.is_empty());
    }

    #[test]
    fn valid_cache_is_parsed() {
        let path = cache_fixture("valid-cache");
        std::fs::write(&path, r#"{"tokenId":"0x01","futureField":7}"#).unwrap();

        let cache: ItsCache = read_json_cache(&path);
        std::fs::remove_file(&path).unwrap();

        assert_eq!(cache.token_id.as_deref(), Some("0x01"));
        assert_eq!(cache.extra["futureField"], 7);
    }
}
