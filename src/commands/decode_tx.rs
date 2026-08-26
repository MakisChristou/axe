use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::consensus::Transaction;
use alloy::hex;
use alloy::primitives::{B256, TxHash};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use eyre::{Result, bail};
use futures::future::join_all;
use owo_colors::OwoColorize;

use super::decode;
use crate::config_source;
use crate::types::Network;

type RpcTx = alloy::rpc::types::Transaction;

/// Resolve a chains config for every network, skipping the ones that can't
/// be found or fetched; error only when none resolve.
async fn resolve_all_configs() -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for network in Network::ALL {
        if let Ok(source) = config_source::resolve(network, None).await {
            found.push(source.into_path());
        }
    }
    if found.is_empty() {
        bail!(
            "no chains config found for any network. Pass --config <path> \
             or connect to the network."
        );
    }
    Ok(found)
}

pub async fn run(txid: &str, config: Option<&Path>, chain_filter: Option<&str>) -> Result<()> {
    // Detect Solana vs EVM: Solana signatures are base58, ~88 chars, no 0x prefix
    if !txid.starts_with("0x") && txid.len() > 60 {
        // Likely a Solana signature. Config SVM RPCs (private/QuickNode) are
        // tried first; the public cluster RPCs are appended as fallbacks
        // unless --chain pinned a specific one, so this works with no config.
        let mut rpcs = solana_rpcs_from_configs(config, chain_filter).await;
        match chain_filter {
            Some(filter) if rpcs.is_empty() => {
                bail!("no SVM chain '{filter}' found in the resolved config(s)")
            }
            Some(_) => {}
            None => rpcs.extend(
                super::decode_sol_tx::SOLANA_RPCS
                    .iter()
                    .map(|(n, u)| (n.to_string(), u.to_string())),
            ),
        }
        return super::decode_sol_tx::run(txid, &rpcs).await;
    }

    let tx_hash: TxHash = txid.parse().map_err(|_| eyre::eyre!("invalid tx hash"))?;

    let configs: Vec<PathBuf> = if let Some(c) = config {
        vec![c.to_path_buf()]
    } else {
        resolve_all_configs().await?
    };

    // Collect RPCs from all configs, dedup by RPC URL
    let mut rpcs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cfg in &configs {
        for chain in load_evm_rpcs(cfg, chain_filter).await? {
            if seen.insert(chain.rpc.clone()) {
                rpcs.push(chain);
            }
        }
    }

    if rpcs.is_empty() {
        bail!("no EVM chains found in config(s)");
    }

    println!("Searching {} EVM chains...", rpcs.len());
    let (chain_name, tx, receipt) = fetch_tx(&rpcs, tx_hash).await?;

    // Print summary
    println!("{:<8} {}", "Chain:".bold(), chain_name);
    println!("{:<8} {txid}", "Tx:".bold());
    if let Some(block) = tx.block_number {
        println!("{:<8} {block}", "Block:".bold());
    }
    println!("{:<8} {}", "From:".bold(), tx.inner.signer());
    if let Some(to) = tx.inner.to() {
        println!("{:<8} {to}", "To:".bold());
    }
    if !tx.inner.value().is_zero() {
        println!("{:<8} {} wei", "Value:".bold(), tx.inner.value());
    }
    if let Some(ref r) = receipt {
        let status = if r.status() {
            "Success".green().to_string()
        } else {
            "Failed".red().to_string()
        };
        println!("{:<8} {status}", "Status:".bold());
    } else {
        println!(
            "{:<8} {}",
            "Status:".bold(),
            "(receipt unavailable)".dimmed()
        );
    }

    // Decode calldata
    let input = tx.inner.input();
    if !input.is_empty() {
        println!("\n{}", "━━ Calldata ━━".bold());
        if let Err(e) = decode::decode_bytes(input, "") {
            println!("  Could not decode: {e}");
            let h = hex::encode(input);
            if h.len() > 128 {
                println!("  Raw: 0x{}… ({} bytes)", &h[..64], input.len());
            } else {
                println!("  Raw: 0x{h}");
            }
        }
    }

    // Decode logs
    if let Some(ref receipt) = receipt {
        print_decoded_logs(receipt);
    }

    Ok(())
}

fn print_decoded_logs(receipt: &TransactionReceipt) {
    let logs = receipt.inner.logs();
    if logs.is_empty() {
        return;
    }
    println!("\n{}", format!("━━ Logs ({}) ━━", logs.len()).bold());
    for (i, log) in logs.iter().enumerate() {
        let addr = log.address();
        let topics: Vec<B256> = log.topics().to_vec();
        let data = log.data().data.as_ref();

        println!("\n[{i}] {}", addr.dimmed());

        if topics.is_empty() {
            println!("    (anonymous event, {} bytes data)", data.len());
            continue;
        }

        match decode::decode_log(&topics, data) {
            Some((sig, params)) => {
                println!("    {}", sig.bold());
                for (name, value) in &params {
                    println!("      {name}: {}", decode::format_value_pub(value));
                }
                // Try nested decode on bytes params
                for (_, value) in &params {
                    if let alloy::dyn_abi::DynSolValue::Bytes(b) = value
                        && b.len() >= 4
                    {
                        let _ = decode::decode_bytes(b, "        ");
                    }
                }
            }
            None => {
                println!(
                    "    Unknown event (topic0: 0x{}…)",
                    hex::encode(&topics[0][..8])
                );
                if !data.is_empty() {
                    let h = hex::encode(data);
                    if h.len() > 128 {
                        println!("    data: 0x{}… ({} bytes)", &h[..64], data.len());
                    } else {
                        println!("    data: 0x{h}");
                    }
                }
            }
        }
    }
}

struct EvmChain {
    name: String,
    rpc: String,
}

fn alchemy_slug(chain_id: u64) -> Option<&'static str> {
    Some(match chain_id {
        1 => "eth-mainnet",
        11155111 => "eth-sepolia",
        137 => "polygon-mainnet",
        80002 => "polygon-amoy",
        42161 => "arb-mainnet",
        421614 => "arb-sepolia",
        10 => "opt-mainnet",
        11155420 => "opt-sepolia",
        8453 => "base-mainnet",
        84532 => "base-sepolia",
        43114 => "avax-mainnet",
        43113 => "avax-fuji",
        56 => "bnb-mainnet",
        97 => "bnb-testnet",
        59144 => "linea-mainnet",
        59141 => "linea-sepolia",
        534352 => "scroll-mainnet",
        534351 => "scroll-sepolia",
        81457 => "blast-mainnet",
        168587773 => "blast-sepolia",
        252 => "frax-mainnet",
        2522 => "frax-sepolia",
        1284 => "moonbeam-mainnet",
        42220 => "celo-mainnet",
        5000 => "mantle-mainnet",
        _ => return None,
    })
}

async fn load_evm_rpcs(config: &Path, chain_filter: Option<&str>) -> Result<Vec<EvmChain>> {
    let content = tokio::fs::read_to_string(config)
        .await
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    let chains = root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' object in config"))?;

    let alchemy_token = std::env::var("ALCHEMY_TOKEN").ok();

    let mut result = Vec::new();
    for (key, value) in chains {
        let chain_type = value
            .get("chainType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if chain_type != "evm" {
            continue;
        }

        let axelar_id = value
            .get("axelarId")
            .and_then(|v| v.as_str())
            .unwrap_or(key);

        if let Some(filter) = chain_filter
            && axelar_id != filter
            && key != filter
        {
            continue;
        }

        let chain_id = value.get("chainId").and_then(|v| v.as_u64());

        // Prefer Alchemy RPC when token is available and chain is supported
        let rpc = alchemy_token
            .as_deref()
            .and_then(|token| {
                chain_id
                    .and_then(alchemy_slug)
                    .map(|slug| format!("https://{slug}.g.alchemy.com/v2/{token}"))
            })
            .or_else(|| value.get("rpc").and_then(|v| v.as_str()).map(String::from));

        if let Some(rpc) = rpc {
            result.push(EvmChain {
                name: axelar_id.to_string(),
                rpc,
            });
        }
    }

    Ok(result)
}

async fn fetch_tx(
    rpcs: &[EvmChain],
    tx_hash: TxHash,
) -> Result<(String, RpcTx, Option<TransactionReceipt>)> {
    let futures: Vec<_> = rpcs
        .iter()
        .map(|chain| {
            let rpc = chain.rpc.clone();
            let name = chain.name.clone();
            async move {
                let Ok(url) = rpc.parse() else { return None };
                let provider = ProviderBuilder::new().connect_http(url);
                let result = tokio::time::timeout(Duration::from_secs(15), async {
                    let tx = match provider.get_transaction_by_hash(tx_hash).await {
                        Ok(Some(tx)) => tx,
                        Ok(None) => return Err(eyre::eyre!("tx not found")),
                        Err(e) => return Err(eyre::eyre!("tx fetch: {e}")),
                    };
                    let receipt = match provider.get_transaction_receipt(tx_hash).await {
                        Ok(Some(r)) => Some(r),
                        Ok(None) | Err(_) => None,
                    };
                    Ok((tx, receipt))
                })
                .await;
                match result {
                    Ok(Ok((tx, receipt))) => Some((name, tx, receipt)),
                    _ => None,
                }
            }
        })
        .collect();

    let results = join_all(futures).await;

    if let Some((name, tx, receipt)) = results.into_iter().flatten().next() {
        return Ok((name, tx, receipt));
    }

    bail!(
        "transaction not found on any chain (tried {} RPCs)",
        rpcs.len()
    )
}

/// SVM RPC candidates from the explicit config, or from every resolvable
/// network's config when none is given, optionally narrowed to one chain
/// (matched by key or axelarId, like the EVM path). Failures resolve to
/// "no candidates".
async fn solana_rpcs_from_configs(
    config: Option<&Path>,
    chain_filter: Option<&str>,
) -> Vec<(String, String)> {
    let configs: Vec<(String, PathBuf)> = match config {
        Some(c) => vec![("config".to_string(), c.to_path_buf())],
        None => {
            let mut v = Vec::new();
            for network in crate::types::Network::ALL {
                if let Ok(src) = crate::config_source::resolve(network, None).await {
                    v.push((network.to_string(), src.into_path()));
                }
            }
            v
        }
    };
    let mut rpcs = Vec::new();
    for (label, path) in configs {
        let Ok(cfg) = crate::config::ChainsConfig::load(&path).await else {
            continue;
        };
        for (key, chain) in &cfg.chains {
            if chain.chain_type.as_deref() != Some("svm") {
                continue;
            }
            let axelar_id = chain.axelar_id.as_deref().unwrap_or(key);
            if let Some(filter) = chain_filter
                && axelar_id != filter
                && key != filter
            {
                continue;
            }
            if let Some(rpc) = chain.rpc.clone() {
                rpcs.push((label.clone(), rpc));
            }
        }
    }
    rpcs
}

/// One decoded event from a transaction receipt.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DecodedLog {
    pub address: String,
    /// The event signature when its topic0 is in the embedded ABI database.
    pub event: Option<String>,
    pub params: Vec<super::decode::DecodedArgument>,
}

/// An EVM transaction, decoded.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DecodedTransaction {
    /// Which chain the transaction was found on. axe searches every configured
    /// EVM chain, so this is a result rather than an input.
    pub chain: String,
    pub tx_hash: String,
    pub block: Option<u64>,
    pub from: String,
    pub to: Option<String>,
    pub value_wei: String,
    /// Absent when the receipt could not be fetched, which is not the same as
    /// a failed transaction.
    pub succeeded: Option<bool>,
    /// The decoded call, when the transaction carried input.
    pub input: Option<super::decode::DecodedPayload>,
    pub logs: Vec<DecodedLog>,
}

fn decode_logs(receipt: &TransactionReceipt) -> Vec<DecodedLog> {
    receipt
        .inner
        .logs()
        .iter()
        .map(|log| {
            let topics: Vec<B256> = log.topics().to_vec();
            let data = log.data().data.as_ref();

            match decode::decode_log(&topics, data) {
                Some((signature, params)) => DecodedLog {
                    address: format!("{:#x}", log.address()),
                    event: Some(signature),
                    params: params
                        .iter()
                        .map(|(name, value)| super::decode::DecodedArgument {
                            name: name.clone(),
                            ty: String::new(),
                            value: super::decode_evm_activity::sol_value_to_json(value),
                        })
                        .collect(),
                },
                None => DecodedLog {
                    address: format!("{:#x}", log.address()),
                    event: None,
                    params: Vec::new(),
                },
            }
        })
        .collect()
}

/// Fetch and decode an EVM transaction without printing.
///
/// Solana signatures are not handled: `decode_sol_tx` only exposes a printing
/// entry point, so structuring that path is separate work. Callers should
/// reject non-EVM input before calling this.
pub(crate) async fn resolve_evm(
    txid: &str,
    config: Option<&Path>,
    chain_filter: Option<&str>,
) -> Result<DecodedTransaction> {
    let tx_hash: TxHash = txid.parse().map_err(|_| eyre::eyre!("invalid tx hash"))?;

    let configs: Vec<PathBuf> = match config {
        Some(c) => vec![c.to_path_buf()],
        None => resolve_all_configs().await?,
    };

    let mut rpcs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cfg in &configs {
        for chain in load_evm_rpcs(cfg, chain_filter).await? {
            if seen.insert(chain.rpc.clone()) {
                rpcs.push(chain);
            }
        }
    }
    if rpcs.is_empty() {
        bail!("no EVM chains found in config(s)");
    }

    let (chain, tx, receipt) = fetch_tx(&rpcs, tx_hash).await?;

    let input = tx.inner.input();
    let decoded_input = if input.is_empty() {
        None
    } else {
        decode::decode_payload(input).ok()
    };

    Ok(DecodedTransaction {
        chain,
        tx_hash: format!("{tx_hash:#x}"),
        block: tx.block_number,
        from: format!("{:#x}", tx.inner.signer()),
        to: tx.inner.to().map(|to| format!("{to:#x}")),
        value_wei: tx.inner.value().to_string(),
        succeeded: receipt.as_ref().map(TransactionReceipt::status),
        input: decoded_input,
        logs: receipt.as_ref().map(decode_logs).unwrap_or_default(),
    })
}
