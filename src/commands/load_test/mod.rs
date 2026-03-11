pub mod evm_to_sol;
pub mod keypairs;
pub mod metrics;
pub mod sol_to_evm;
mod verify;


use std::path::PathBuf;
use std::time::Instant;

use alloy::{
    primitives::Bytes,
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
    network::TransactionBuilder,
};
use eyre::Result;
use serde_json::json;

use crate::evm::read_artifact_bytecode;
use crate::ui;
use crate::utils::read_contract_address;

use self::metrics::LoadTestReport;

/// Load test type (extensible for future directions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TestType {
    /// Solana -> EVM cross-chain load test
    SolToEvm,
    /// EVM -> Solana cross-chain load test
    EvmToSol,
}

impl std::fmt::Display for TestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestType::SolToEvm => write!(f, "sol-to-evm"),
            TestType::EvmToSol => write!(f, "evm-to-sol"),
        }
    }
}

/// CLI arguments for the load test command.
pub struct LoadTestArgs {
    pub config: PathBuf,
    pub test_type: TestType,
    pub destination_chain: String,
    pub source_chain: String,
    pub solana_rpc: String,
    pub private_key: Option<String>,
    pub num_txs: u64,
    pub keypair: Option<String>,
    pub payload: Option<String>,
    pub output_dir: PathBuf,
}

/// Cache file for storing SenderReceiver address per chain.
fn cache_path(axelar_id: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axe");
    data_dir.join(format!("load-test-{axelar_id}.json"))
}

fn read_cache(axelar_id: &str) -> serde_json::Value {
    let path = cache_path(axelar_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_cache(axelar_id: &str, cache: &serde_json::Value) -> Result<()> {
    let path = cache_path(axelar_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
    Ok(())
}

/// Look up a chain's `chainType` from the config.
fn chain_type(chains: &serde_json::Map<String, serde_json::Value>, chain_id: &str) -> Option<String> {
    chains
        .get(chain_id)?
        .get("chainType")?
        .as_str()
        .map(String::from)
}

/// Find chains by chainType, optionally skipping core-* prefixed chains.
fn find_chains_by_type(
    chains: &serde_json::Map<String, serde_json::Value>,
    chain_type_filter: &str,
    skip_core: bool,
) -> Vec<String> {
    chains
        .iter()
        .filter(|(k, v)| {
            v.get("chainType").and_then(|t| t.as_str()) == Some(chain_type_filter)
                && !(skip_core && k.starts_with("core-"))
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Infer test type from source and destination chain types.
fn infer_test_type(source_type: &str, dest_type: &str) -> Result<TestType> {
    match (source_type, dest_type) {
        ("svm", "evm") => Ok(TestType::SolToEvm),
        ("evm", "svm") => Ok(TestType::EvmToSol),
        _ => Err(eyre::eyre!(
            "unsupported chain type combination: {source_type} -> {dest_type}. \
             Supported: svm -> evm, evm -> svm"
        )),
    }
}

/// Resolved configuration from the config JSON.
pub struct ResolvedConfig {
    pub test_type: TestType,
    pub source_chain: String,
    pub destination_chain: String,
    pub solana_rpc: String,
    pub private_key: Option<String>,
}

/// Resolve chains, RPCs, and test type from the config JSON.
///
/// Supports three modes:
/// 1. `--test-type` only → auto-detect source and destination chains
/// 2. `--source-chain` + `--destination-chain` only → infer test type from chainType
/// 3. All three → validate consistency
pub fn resolve_from_config(
    config: &PathBuf,
    test_type_override: Option<TestType>,
    source_chain_override: Option<String>,
    destination_chain_override: Option<String>,
    private_key_override: Option<String>,
    source_rpc_override: Option<String>,
) -> Result<ResolvedConfig> {
    let config_content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let chains = config_root
        .get("chains")
        .and_then(|v| v.as_object())
        .ok_or_else(|| eyre::eyre!("no 'chains' object in config"))?;

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
            let (tt, src, dst) = auto_detect_all(chains, src_opt, dst_opt)?;
            (tt, src, dst)
        }
    };

    // --- Read RPCs ---
    let solana_rpc = match test_type {
        TestType::SolToEvm => chains
            .get(&source_chain)
            .and_then(|v| v.get("rpc"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                eyre::eyre!("no RPC URL for source chain '{source_chain}' in config")
            })?
            .to_string(),
        TestType::EvmToSol => chains
            .get(&destination_chain)
            .and_then(|v| v.get("rpc"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                eyre::eyre!("no RPC URL for destination chain '{destination_chain}' in config")
            })?
            .to_string(),
    };

    let solana_rpc = source_rpc_override.unwrap_or(solana_rpc);

    Ok(ResolvedConfig {
        test_type,
        source_chain,
        destination_chain,
        solana_rpc,
        private_key: private_key_override,
    })
}

/// Auto-detect source/destination chains for a known test type.
fn auto_detect_chains(
    chains: &serde_json::Map<String, serde_json::Value>,
    test_type: TestType,
    source_override: Option<String>,
    dest_override: Option<String>,
) -> Result<(String, String)> {
    match test_type {
        TestType::SolToEvm => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let svm = find_chains_by_type(chains, "svm", false);
                    match svm.len() {
                        0 => return Err(eyre::eyre!("no SVM (Solana) chain found in config")),
                        1 => {
                            ui::info(&format!("auto-detected source: {}", svm[0]));
                            svm[0].clone()
                        }
                        _ => return Err(eyre::eyre!(
                            "multiple SVM chains found: {}. Use --source-chain to pick one.",
                            svm.join(", ")
                        )),
                    }
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    if evm.is_empty() {
                        return Err(eyre::eyre!("no EVM chain found in config"));
                    }
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        evm[0]
                    ));
                    evm[0].clone()
                }
            };
            Ok((source, dest))
        }
        TestType::EvmToSol => {
            let source = match source_override {
                Some(s) => s,
                None => {
                    let evm = find_chains_by_type(chains, "evm", true);
                    match evm.len() {
                        0 => return Err(eyre::eyre!("no EVM chain found in config")),
                        1 => {
                            ui::info(&format!("auto-detected source: {}", evm[0]));
                            evm[0].clone()
                        }
                        _ => return Err(eyre::eyre!(
                            "multiple EVM chains found: {}. Use --source-chain to pick one.",
                            evm.join(", ")
                        )),
                    }
                }
            };
            let dest = match dest_override {
                Some(d) => d,
                None => {
                    let svm = find_chains_by_type(chains, "svm", false);
                    if svm.is_empty() {
                        return Err(eyre::eyre!("no SVM (Solana) chain found in config"));
                    }
                    ui::info(&format!(
                        "auto-detected destination: {} (use --destination-chain to override)",
                        svm[0]
                    ));
                    svm[0].clone()
                }
            };
            Ok((source, dest))
        }
    }
}

/// Auto-detect test type and chains when nothing is specified.
/// Looks at what chain types exist in the config and picks the best match.
fn auto_detect_all(
    chains: &serde_json::Map<String, serde_json::Value>,
    source_override: Option<String>,
    dest_override: Option<String>,
) -> Result<(TestType, String, String)> {
    // If one chain is given, figure out the other
    if let Some(ref src) = source_override {
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
                return Err(eyre::eyre!("no SVM chain found in config to pair with EVM source"));
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
                return Err(eyre::eyre!("no SVM chain found in config to pair with EVM destination"));
            }
            ui::info(&format!("auto-detected source: {}", svm[0]));
            ui::info("inferred test type: sol-to-evm");
            return Ok((TestType::SolToEvm, svm[0].clone(), dst.clone()));
        }
        if dst_type == "svm" {
            let evm = find_chains_by_type(chains, "evm", true);
            if evm.is_empty() {
                return Err(eyre::eyre!("no EVM chain found in config to pair with SVM destination"));
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
        ui::info(&format!("auto-detected: {} -> {} (sol-to-evm)", svm[0], evm[0]));
        return Ok((TestType::SolToEvm, svm[0].clone(), evm[0].clone()));
    }

    Err(eyre::eyre!(
        "cannot auto-detect test type from config. Use --test-type, or --source-chain + --destination-chain."
    ))
}

pub async fn run(args: LoadTestArgs) -> Result<()> {
    let run_start = Instant::now();

    ui::section(&format!("Load Test: {} -> {}", args.source_chain, args.destination_chain));
    std::fs::create_dir_all(&args.output_dir)?;

    match args.test_type {
        TestType::SolToEvm => run_sol_to_evm(args, run_start).await,
        TestType::EvmToSol => run_evm_to_sol(args, run_start).await,
    }
}

async fn run_sol_to_evm(args: LoadTestArgs, run_start: Instant) -> Result<()> {
    let dest = &args.destination_chain;
    let src = &args.source_chain;

    // --- Read chain info from chains config JSON ---
    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let rpc_url = config_root
        .pointer(&format!("/chains/{dest}/rpc"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no rpc URL for chain '{dest}' in config"))?;

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = read_contract_address(&args.config, dest, "AxelarGateway")?;
    let gas_service_addr = read_contract_address(&args.config, dest, "AxelarGasService")?;

    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // --- Deploy/reuse SenderReceiver on destination EVM chain ---
    let cache = read_cache(dest);

    let (sender_receiver_addr, provider) = if let Some(addr_str) = cache
        .get("senderReceiverAddress")
        .and_then(|v| v.as_str())
    {
        // Try to reuse cached address — only need a read-only provider for the check
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr: alloy::primitives::Address = addr_str.parse()?;
        let code = read_provider.get_code_at(addr).await?;
        if code.is_empty() {
            ui::warn("cached SenderReceiver has no code, redeploying...");
            let private_key = args.private_key.as_ref().ok_or_else(|| {
                eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
            })?;
            let signer: PrivateKeySigner = private_key.parse()?;
            let write_provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            let new_addr =
                deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
            let mut cache = cache;
            cache["senderReceiverAddress"] = json!(format!("{new_addr}"));
            save_cache(dest, &cache)?;
            (new_addr, write_provider)
        } else {
            ui::info(&format!("SenderReceiver: reusing {addr}"));
            let private_key = args.private_key.as_deref().unwrap_or(
                "0x0000000000000000000000000000000000000000000000000000000000000001",
            );
            let signer: PrivateKeySigner = private_key.parse()?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            (addr, provider)
        }
    } else {
        ui::info("deploying SenderReceiver on destination chain...");
        let private_key = args.private_key.as_ref().ok_or_else(|| {
            eyre::eyre!("EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key")
        })?;
        let signer: PrivateKeySigner = private_key.parse()?;
        let write_provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(rpc_url.parse()?);
        let addr = deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache;
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(dest, &cache)?;
        (addr, write_provider)
    };

    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    let mut report =
        sol_to_evm::run_load_test_with_metrics(&args, &destination_address).await?;

    let verification = verify::verify_onchain(
        &args.config,
        &args.source_chain,
        &args.destination_chain,
        &destination_address,
        gateway_addr,
        &provider,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &report, run_start)
}

async fn run_evm_to_sol(args: LoadTestArgs, run_start: Instant) -> Result<()> {
    let src = &args.source_chain;
    let dest = &args.destination_chain;

    // --- Read SOURCE EVM chain info ---
    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let evm_rpc_url = config_root
        .pointer(&format!("/chains/{src}/rpc"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no rpc URL for source chain '{src}' in config"))?;

    ui::kv("source", src);
    ui::kv("destination", dest);

    let gateway_addr = read_contract_address(&args.config, src, "AxelarGateway")?;
    ui::address("EVM gateway (source)", &format!("{gateway_addr}"));

    // --- Set up EVM signer (no SenderReceiver needed, calls gateway directly) ---
    let private_key = args.private_key.as_ref().ok_or_else(|| {
        eyre::eyre!(
            "EVM private key required. Set EVM_PRIVATE_KEY env var or use --private-key"
        )
    })?;
    let signer: PrivateKeySigner = private_key.parse()?;
    let signer_address = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(evm_rpc_url.parse()?);

    // Destination on Solana: memo program
    let destination_address = evm_to_sol::MEMO_PROGRAM_ADDRESS;
    ui::kv("destination program", destination_address);

    let mut report = evm_to_sol::run_load_test_with_metrics(
        &args,
        gateway_addr,
        signer_address,
        destination_address,
        &provider,
    )
    .await?;

    let verification = verify::verify_onchain_solana(
        &args.config,
        &args.source_chain,
        &args.destination_chain,
        destination_address,
        &args.solana_rpc,
        &mut report.transactions,
    )
    .await?;
    report.verification = Some(verification);

    finish_report(&args, &report, run_start)
}

fn finish_report(
    args: &LoadTestArgs,
    report: &LoadTestReport,
    run_start: Instant,
) -> Result<()> {
    let report_output = args.output_dir.join("report.json");
    let report_json = serde_json::to_string_pretty(report)?;
    std::fs::write(&report_output, &report_json)?;

    print_final_report(report);
    ui::kv("full report saved to", &report_output.display().to_string());
    ui::success(&format!(
        "load test complete ({})",
        ui::format_elapsed(run_start)
    ));

    Ok(())
}

async fn deploy_sender_receiver<P: alloy::providers::Provider>(
    provider: &P,
    gateway: alloy::primitives::Address,
    gas_service: alloy::primitives::Address,
) -> Result<alloy::primitives::Address> {
    let bytecode = read_artifact_bytecode("artifacts/SenderReceiver.json")?;
    let mut deploy_code = bytecode;
    deploy_code.extend_from_slice(&(gateway, gas_service).abi_encode_params());

    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(deploy_code));
    let pending = provider.send_transaction(tx).await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");

    let receipt = tokio::time::timeout(std::time::Duration::from_secs(120), pending.get_receipt())
        .await
        .map_err(|_| eyre::eyre!("deploy tx timed out after 120s"))??;

    let addr = receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in receipt"))?;

    ui::success(&format!(
        "deployed in block {}",
        receipt.block_number.unwrap_or(0)
    ));
    Ok(addr)
}

#[allow(clippy::float_arithmetic)]
fn print_final_report(report: &LoadTestReport) {
    println!();
    println!("\u{2550}\u{2550}\u{2550} SUMMARY \u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}");
    println!(
        "  transactions     {}/{} confirmed ({:.1}% landed)",
        report.total_confirmed,
        report.total_submitted,
        report.landing_rate * 100.0,
    );

    if let Some(ref v) = report.verification {
        println!();

        // End-to-end line
        match (v.avg_executed_secs, v.max_executed_secs) {
            (Some(avg), Some(max)) => {
                println!("  end-to-end       avg {avg:.1}s \u{2502} max {max:.1}s");
            }
            (Some(avg), None) => {
                println!("  end-to-end       avg {avg:.1}s");
            }
            _ => {}
        }

        // Segment breakdown
        if let Some(val) = report.avg_latency_ms {
            println!("  \u{251c}\u{2500} source tx      avg {:.1}s", val / 1000.0);
        }
        if let Some(val) = v.avg_voted_secs {
            println!("  \u{251c}\u{2500} voted          avg {val:.1}s");
        }
        if let Some(val) = v.avg_routed_secs {
            println!("  \u{251c}\u{2500} routed         avg {val:.1}s");
        }
        if let Some(val) = v.avg_approved_secs {
            println!("  \u{251c}\u{2500} approved       avg {val:.1}s");
        }
        if let Some(val) = v.avg_executed_secs {
            println!("  \u{2514}\u{2500} executed       avg {val:.1}s");
        }

        // Stuck
        if v.stuck > 0 {
            let stuck_detail: Vec<String> = v
                .stuck_at
                .iter()
                .map(|c| format!("{} at {}", c.count, c.reason))
                .collect();
            println!();
            println!(
                "  stuck            {}/{} ({:.1}%) \u{2014} {}",
                v.stuck,
                v.total_verified + v.stuck,
                v.stuck as f64 / (v.total_verified + v.stuck) as f64 * 100.0,
                stuck_detail.join(", "),
            );
        }

        // Failures
        println!(
            "  failures         {}",
            v.failed - v.stuck,
        );
        for cat in &v.failure_reasons {
            if !cat.reason.contains("timed out") {
                println!("                   {} \u{00d7} {}", cat.count, cat.reason);
            }
        }
    }
    println!();
}
