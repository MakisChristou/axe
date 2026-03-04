pub mod metrics;
mod test;
mod verify;

pub use test::ContentionMode;

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

/// CLI arguments for the load test command.
pub struct LoadTestArgs {
    pub config: PathBuf,
    pub destination_chain: String,
    pub source_chain: String,
    pub private_key: String,
    pub time: u64,
    pub delay: u64,
    pub keypair: Option<String>,
    pub mnemonic: Option<String>,
    pub addresses_to_derive: Option<usize>,
    pub contention_mode: ContentionMode,
    pub payload: Option<String>,
    pub output_dir: PathBuf,
    pub skip_gmp_verify: bool,
}

/// Cache file for storing SenderReceiver address per chain.
fn cache_path(axelar_id: &str) -> PathBuf {
    let data_dir = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("axelar-evm-deployer");
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

pub async fn run(args: LoadTestArgs) -> Result<()> {
    let run_start = Instant::now();
    let axelar_id = &args.destination_chain;

    ui::section(&format!("Solana Load Test -> {axelar_id}"));

    // --- Read chain info from chains config JSON ---
    let config_content = std::fs::read_to_string(&args.config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", args.config.display()))?;
    let config_root: serde_json::Value = serde_json::from_str(&config_content)?;

    let rpc_url = config_root
        .pointer(&format!("/chains/{axelar_id}/rpc"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("no rpc URL for chain '{axelar_id}' in config"))?;

    ui::kv("EVM RPC", rpc_url);

    let signer: PrivateKeySigner = args.private_key.parse()?;
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);

    let gateway_addr = read_contract_address(&args.config, axelar_id, "AxelarGateway")?;
    let gas_service_addr = read_contract_address(&args.config, axelar_id, "AxelarGasService")?;

    ui::address("EVM gateway", &format!("{gateway_addr}"));

    // --- Deploy/reuse SenderReceiver on destination EVM chain ---
    let mut cache = read_cache(axelar_id);

    let sender_receiver_addr = if let Some(addr_str) = cache
        .get("senderReceiverAddress")
        .and_then(|v| v.as_str())
    {
        let addr: alloy::primitives::Address = addr_str.parse()?;
        let code = provider.get_code_at(addr).await?;
        if code.is_empty() {
            ui::warn("cached SenderReceiver has no code, redeploying...");
            let addr =
                deploy_sender_receiver(&provider, gateway_addr, gas_service_addr).await?;
            cache["senderReceiverAddress"] = json!(format!("{addr}"));
            save_cache(axelar_id, &cache)?;
            addr
        } else {
            ui::info(&format!("SenderReceiver: reusing {addr}"));
            addr
        }
    } else {
        ui::info("deploying SenderReceiver on destination chain...");
        let addr = deploy_sender_receiver(&provider, gateway_addr, gas_service_addr).await?;
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(axelar_id, &cache)?;
        addr
    };

    ui::address("SenderReceiver", &format!("{sender_receiver_addr}"));
    let destination_address = format!("{sender_receiver_addr}");

    std::fs::create_dir_all(&args.output_dir)?;

    // --- Phase 1: Send transactions ---
    println!("\n{}", "=".repeat(60));
    println!("PHASE 1: LOAD TEST");
    println!("{}\n", "=".repeat(60));

    let mut report = test::run_load_test_with_metrics(&args, &destination_address).await?;

    // --- Phase 2: On-chain verification ---
    println!("\n{}", "=".repeat(60));
    println!("PHASE 2: ON-CHAIN VERIFICATION");
    println!("{}\n", "=".repeat(60));

    if args.skip_gmp_verify {
        ui::info("skipped (--skip-gmp-verify)");
    } else {
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
    }

    // --- Phase 3: Final report ---
    println!("\n{}", "=".repeat(60));
    println!("PHASE 3: FINAL REPORT");
    println!("{}\n", "=".repeat(60));

    let report_output = args.output_dir.join("report.json");
    let report_json = serde_json::to_string_pretty(&report)?;
    std::fs::write(&report_output, &report_json)?;

    print_final_report(&report);
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
    ui::section("SUMMARY");
    ui::kv(
        "txs",
        &format!(
            "{}/{} confirmed, {:.1}% landed",
            report.total_confirmed,
            report.total_submitted,
            report.landing_rate * 100.0,
        ),
    );
    if let Some(avg) = report.avg_latency_ms {
        ui::kv("solana latency", &format!("{avg:.0}ms avg"));
    }
    if let Some(ref v) = report.verification {
        ui::kv(
            "cross-chain",
            &format!(
                "{}/{} executed ({:.0}%)",
                v.successful,
                v.total_verified,
                v.success_rate * 100.0,
            ),
        );
        if let Some(avg) = v.avg_executed_secs {
            ui::kv("end-to-end", &format!("{avg:.1}s avg"));
        }
    }
}
