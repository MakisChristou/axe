//! Shared helpers used by every per-pair load-test orchestrator:
//! SenderReceiver deploy/reuse, RPC validation, the report finalizer, and the
//! per-chain config readers (Stellar/Sui wallet loaders, JSON-pointer
//! contract-address lookups). Most of this module is `pub(super)` — only
//! `ensure_sender_receiver_on_evm_chain` is `pub(crate)` because
//! `commands::test_gmp` calls into it for the `--config` sol→evm flow.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::{Result, WrapErr};
use owo_colors::OwoColorize;
use serde::Deserialize;
use serde_json::json;

use super::metrics::{AmplifierTiming, LoadTestReport};
use super::verify;
use super::{LoadTestArgs, read_cache, save_cache};
use crate::config::ChainsConfig;
use crate::evm::read_artifact_bytecode;
use crate::ui;

/// Subset of the chains-config JSON read by the per-chain helpers in this
/// module. Each helper picks one field; fields stay `Option<T>` because not
/// every chain entry carries every key (e.g. only Stellar entries set
/// `tokenAddress`).
#[derive(Deserialize)]
struct ChainsFile {
    chains: HashMap<String, ChainEntry>,
}

#[derive(Deserialize)]
struct ChainEntry {
    #[serde(rename = "axelarId")]
    axelar_id: Option<String>,
    #[serde(rename = "networkType")]
    network_type: Option<String>,
    #[serde(rename = "tokenAddress")]
    token_address: Option<String>,
    contracts: Option<HashMap<String, ContractEntry>>,
}

#[derive(Deserialize)]
struct ContractEntry {
    address: Option<String>,
}

pub(crate) async fn ensure_sender_receiver_on_evm_chain(
    chain: &str,
    rpc_url: &str,
    evm_private_key: &str,
    gateway_addr: Address,
    gas_service_addr: Address,
) -> Result<Address> {
    use alloy::signers::local::PrivateKeySigner;
    let signer: PrivateKeySigner = evm_private_key.parse()?;
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let cache = read_cache(chain);
    deploy_or_reuse_sender_receiver(
        &cache,
        chain,
        &read_provider,
        &write_provider,
        gateway_addr,
        gas_service_addr,
        chain,
    )
    .await
}

/// Deploy or reuse a cached SenderReceiver contract.
/// Read `eth_getCode` at `addr`, retrying transient `0x` (empty) responses
/// with a short backoff. Some EVM RPCs (Hyperliquid mainnet observed) return
/// empty code for live contracts under concurrent load; without retries that
/// triggers a wasteful on-chain redeploy of a SenderReceiver that's still there.
async fn get_code_with_retry<P: Provider>(provider: &P, addr: Address) -> Result<Bytes> {
    const ATTEMPTS: u32 = 3;
    let mut last = Bytes::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
        }
        last = provider.get_code_at(addr).await?;
        if !last.is_empty() {
            return Ok(last);
        }
    }
    Ok(last)
}

pub(crate) async fn deploy_or_reuse_sender_receiver<R: Provider, W: Provider>(
    cache: &serde_json::Value,
    cache_key: &str,
    read_provider: &R,
    write_provider: &W,
    gateway_addr: Address,
    gas_service_addr: Address,
    label: &str,
) -> Result<Address> {
    if let Some(addr_str) = cache.get("senderReceiverAddress").and_then(|v| v.as_str()) {
        let addr: Address = addr_str.parse()?;
        let code = get_code_with_retry(read_provider, addr).await?;
        let needs_redeploy = if code.is_empty() {
            ui::warn(&format!(
                "cached SenderReceiver ({label}) has no code, redeploying..."
            ));
            true
        } else {
            // Verify the cached contract's gateway matches the current config.
            let sr = crate::evm::SenderReceiver::new(addr, read_provider);
            match sr.gateway().call().await {
                Ok(onchain_gw) => {
                    if onchain_gw != gateway_addr {
                        ui::warn(&format!(
                            "cached SenderReceiver ({label}) points to old gateway {onchain_gw}, expected {gateway_addr}, redeploying..."
                        ));
                        true
                    } else {
                        false
                    }
                }
                Err(_) => {
                    ui::warn(&format!(
                        "cached SenderReceiver ({label}) gateway check failed, redeploying..."
                    ));
                    true
                }
            }
        };
        if needs_redeploy {
            let new_addr =
                deploy_sender_receiver(write_provider, gateway_addr, gas_service_addr).await?;
            let mut cache = cache.clone();
            cache["senderReceiverAddress"] = json!(format!("{new_addr}"));
            save_cache(cache_key, &cache)?;
            Ok(new_addr)
        } else {
            ui::info(&format!("SenderReceiver ({label}): reusing {addr}"));
            Ok(addr)
        }
    } else {
        ui::info(&format!("deploying SenderReceiver on {label} chain..."));
        let addr = deploy_sender_receiver(write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache.clone();
        cache["senderReceiverAddress"] = json!(format!("{addr}"));
        save_cache(cache_key, &cache)?;
        Ok(addr)
    }
}

/// Replace any `http://…` / `https://…` substring with `<redacted-url>`,
/// preserving the surrounding text. Used to keep RPC URLs (which can come
/// from repo secrets) out of the load-test JSON report and other surfaces
/// that may include propagated error messages.
///
/// Terminators recognised as the end of a URL: whitespace, `'`, `"`, `)`,
/// `]`, `,`, `;`, `<`, `>`.
pub(crate) fn scrub_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '\'' | '"' | ')' | ']' | ',' | ';' | '<' | '>')
                })
                .unwrap_or(rest.len());
            out.push_str("<redacted-url>");
            i += end;
        } else {
            let ch_len = rest.chars().next().map_or(1, char::len_utf8);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

pub(crate) fn finish_report(
    args: &LoadTestArgs,
    report: &mut LoadTestReport,
    run_start: Instant,
) -> Result<()> {
    report.protocol = format!("{}", args.protocol);
    report.tps = args.tps;
    report.duration_secs = args.duration_secs;
    // Scrub any URLs that upstream-crate errors may have folded into
    // per-tx error strings. Belt-and-suspenders alongside the error-template
    // refactor: private RPC URLs (from repo secrets) must not appear in the
    // JSON artifact, regardless of where the underlying error came from.
    for tx in &mut report.transactions {
        if let Some(err) = tx.error_mut() {
            *err = scrub_urls(err);
        }
    }
    print_final_report(report);

    // Write full JSON report to a timestamped file so failures can be inspected afterwards.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_dir = std::path::Path::new("axe-load-test-logs");
    let log_path = log_dir.join(format!("axe-load-test-{ts}.json"));
    let mut report_written = false;
    match std::fs::create_dir_all(log_dir) {
        Ok(()) => match serde_json::to_string_pretty(report) {
            Ok(json) => match std::fs::write(&log_path, &json) {
                Ok(()) => {
                    report_written = true;
                    ui::info(&format!("report written to {}", log_path.display()));
                }
                Err(e) => ui::warn(&format!(
                    "could not write report to {}: {e}",
                    log_path.display()
                )),
            },
            Err(e) => ui::warn(&format!("could not serialize report: {e}")),
        },
        Err(e) => ui::warn(&format!(
            "could not create log dir {}: {e}",
            log_dir.display()
        )),
    }

    let source_failures = report.total_failed;
    let verification_failures = report.verification.as_ref().map_or(0, |v| v.failed);
    if source_failures > 0 || verification_failures > 0 {
        let report_hint = if report_written {
            format!("; report written to {}", log_path.display())
        } else {
            String::new()
        };
        return Err(eyre::eyre!(
            "load test failed: {source_failures} source tx failures, \
             {verification_failures} verification failures{report_hint}"
        ));
    }

    ui::success(&format!(
        "load test complete ({})",
        ui::format_elapsed(run_start)
    ));

    Ok(())
}

/// List chain names that have a Cosmos Gateway address in the config.
/// Used by the load-test runners to print a remediation hint when the user
/// supplies a destination chain whose Gateway has not been deployed yet.
pub(crate) fn list_gateway_chains(cfg: &ChainsConfig) -> Vec<String> {
    cfg.axelar
        .contracts
        .as_ref()
        .and_then(|c| c.get("Gateway"))
        .map(|gateway_map| {
            gateway_map
                .iter()
                .filter(|(_, v)| v.get("address").and_then(|a| a.as_str()).is_some())
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Validate that an RPC endpoint speaks EVM JSON-RPC (eth_chainId).
///
/// Error messages intentionally omit the URL — RPC endpoints can come from
/// repo secrets (private/paid providers) and should not surface in logs or
/// in the JSON load-test report. The URL is still useful for debugging, so
/// we log the chain it failed for instead.
pub(crate) async fn validate_evm_rpc(rpc_url: &str) -> Result<()> {
    let provider = ProviderBuilder::new().connect_http(
        rpc_url
            .parse()
            .map_err(|e| eyre::eyre!("invalid EVM RPC URL: {e}"))?,
    );
    provider.get_chain_id().await.map_err(|_| {
        eyre::eyre!(
            "configured EVM RPC does not appear to be a valid endpoint \
             (eth_chainId failed). Check the chain-config or RPC override."
        )
    })?;
    Ok(())
}

/// Pre-flight check that a contract address actually has bytecode at the
/// given EVM RPC. Without this, the EVM destination verifier silently reports
/// false-positive 30/30 executed — `eth_call` against an EOA returns `0x`,
/// which alloy decodes as `false`, which our pipeline interprets as
/// "approval consumed by execution = success." See verify.rs:266 for the
/// dependent decode logic.
pub(crate) async fn ensure_evm_contract_deployed(
    rpc_url: &str,
    contract_label: &str,
    addr: alloy::primitives::Address,
) -> Result<()> {
    let provider =
        ProviderBuilder::new().connect_http(rpc_url.parse().map_err(|e| eyre::eyre!("{e}"))?);
    let code = provider
        .get_code_at(addr)
        .await
        .map_err(|e| eyre::eyre!("eth_getCode for {contract_label} ({addr}) failed: {e}"))?;
    if code.is_empty() {
        eyre::bail!(
            "{contract_label} at {addr} has no bytecode on the configured RPC. \
             The chain config likely points at an undeployed/stale address — \
             this environment cannot relay messages to that contract. \
             Pick a different chain pair or update the chain config to a deployed address."
        );
    }
    Ok(())
}

/// Validate that an RPC endpoint speaks Solana JSON-RPC (getVersion).
pub(crate) async fn validate_solana_rpc(rpc_url: &str) -> Result<()> {
    let client = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.to_string());
    client.get_version().await.map_err(|_| {
        eyre::eyre!(
            "RPC '{rpc_url}' does not appear to be a Solana endpoint \
             (getVersion failed). Check that you're using the correct RPC URL."
        )
    })?;
    Ok(())
}

pub(crate) async fn check_evm_balance<P: alloy::providers::Provider>(
    provider: &P,
    address: alloy::primitives::Address,
) -> Result<()> {
    let balance = provider.get_balance(address).await?;
    if balance.is_zero() {
        eyre::bail!(
            "EVM wallet {address} has no funds. Fund it first:\n  \
             Use a faucet or transfer native tokens to {address}"
        );
    }
    Ok(())
}

pub(crate) async fn deploy_sender_receiver<P: alloy::providers::Provider>(
    provider: &P,
    gateway: alloy::primitives::Address,
    gas_service: alloy::primitives::Address,
) -> Result<alloy::primitives::Address> {
    // Legacy (pre-1559) chains break alloy's default fee estimation; detect and
    // send a type-0 tx with an explicit gas_price instead.
    let fee_mode = super::gas_mode::EvmFeeMode::detect(provider).await?;

    // Pre-Shanghai chains (e.g. Kava) reject the default bytecode's PUSH0
    // opcode. Probe with eth_call (no nonce consumed) so we send exactly one
    // deploy tx with the bytecode the chain accepts.
    let artifact = choose_deploy_artifact(provider, gateway, gas_service).await?;
    deploy_with_artifact(provider, artifact, gateway, gas_service, fee_mode).await
}

/// Pick the SenderReceiver bytecode the chain accepts. Simulates the default
/// (Shanghai) deploy with `eth_call`; if the chain rejects an opcode (`PUSH0`
/// on a pre-Shanghai EVM) it returns the paris-build path instead. Any other
/// simulation outcome keeps the default — the real deploy surfaces real errors.
async fn choose_deploy_artifact<P: alloy::providers::Provider>(
    provider: &P,
    gateway: alloy::primitives::Address,
    gas_service: alloy::primitives::Address,
) -> Result<&'static str> {
    const DEFAULT: &str = "artifacts/SenderReceiver.json";
    const PARIS: &str = "artifacts/SenderReceiver.paris.json";
    let mut code = read_artifact_bytecode(DEFAULT)?;
    code.extend_from_slice(&(gateway, gas_service).abi_encode_params());
    let probe = TransactionRequest::default().with_deploy_code(Bytes::from(code));
    match provider.call(probe).await {
        Err(e) if is_unsupported_opcode(&e.to_string()) => {
            ui::info("destination rejects PUSH0 (pre-Shanghai EVM); using paris bytecode...");
            Ok(PARIS)
        }
        _ => Ok(DEFAULT),
    }
}

/// True if an error is a chain rejecting an opcode the bytecode uses (e.g.
/// `PUSH0` on a pre-Shanghai EVM) — the signal to use the paris build.
fn is_unsupported_opcode(err: &str) -> bool {
    let s = err.to_lowercase();
    s.contains("push0") || s.contains("invalid opcode")
}

/// Deploy the SenderReceiver from a specific bytecode artifact and return its
/// address. Split out so `deploy_sender_receiver` can retry with an alternate
/// (paris) artifact when the chain rejects the default bytecode's opcodes.
async fn deploy_with_artifact<P: alloy::providers::Provider>(
    provider: &P,
    artifact_path: &str,
    gateway: alloy::primitives::Address,
    gas_service: alloy::primitives::Address,
    fee_mode: super::gas_mode::EvmFeeMode,
) -> Result<alloy::primitives::Address> {
    let mut deploy_code = read_artifact_bytecode(artifact_path)?;
    deploy_code.extend_from_slice(&(gateway, gas_service).abi_encode_params());

    // Wrap `send_transaction` with retry on transient transport / 5xx /
    // 429 errors — observed flakes on HL and Hedera mainnet RPCs where
    // the same submission succeeds on retry. Real reverts (insufficient
    // funds, custom errors) skip the retry per `is_transient_default`.
    let pending = crate::retry::retry_async(
        "deploy_sender_receiver.send_transaction",
        crate::retry::DEFAULT_ATTEMPTS,
        crate::retry::is_transient_default,
        || {
            let tx = fee_mode.apply(
                TransactionRequest::default().with_deploy_code(Bytes::from(deploy_code.clone())),
            );
            async { provider.send_transaction(tx).await }
        },
    )
    .await?;
    let tx_hash = *pending.tx_hash();
    ui::tx_hash("deploy tx", &format!("{tx_hash}"));
    ui::info("waiting for confirmation...");

    // 240 s tolerates HL's ~60 s big-blocks cadence + Hedera HTS-create
    // latency. Fast EVMs (Solana, Stellar destinations) still return as
    // soon as the receipt is available; this is an upper bound, not a
    // floor.
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(240), pending.get_receipt())
        .await
        .map_err(|_| eyre::eyre!("deploy tx timed out after 240s"))??;

    let addr = receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("no contract address in receipt"))?;

    ui::success(&format!(
        "deployed in block {}",
        receipt.block_number.unwrap_or(0)
    ));
    Ok(addr)
}

/// Print the per-stage Amplifier pipeline progress line. Phases that don't
/// apply to the route stay 0 and are omitted — a legacy (consensus) source has
/// no `voted`, a legacy destination has no `routed` — so a legacy run reads
/// cleanly as approved/executed. The "stuck at X" line elsewhere still reports
/// where a stalled message sits.
fn print_pipeline_counts(report: &LoadTestReport) {
    let total = report.total_confirmed;
    let count = |sel: fn(&AmplifierTiming) -> bool| -> u64 {
        report
            .transactions
            .iter()
            .filter(|t| t.amplifier_timing.as_ref().is_some_and(sel))
            .count() as u64
    };
    let voted = count(|a| a.voted_secs.is_some());
    let routed = count(|a| a.routed_secs.is_some());
    let approved = count(|a| a.approved_secs.is_some());
    let executed = count(|a| a.executed_secs.is_some());

    let mut parts = Vec::new();
    if voted > 0 {
        parts.push(format!("voted {voted}/{total}"));
    }
    if routed > 0 {
        parts.push(format!("routed {routed}/{total}"));
    }
    parts.push(format!("approved {approved}/{total}"));
    parts.push(format!("executed {executed}/{total}"));
    println!("  pipeline         {}", parts.join("  "));
}

pub(crate) fn print_final_report(report: &LoadTestReport) {
    println!();
    println!(
        "\u{2550}\u{2550}\u{2550} SUMMARY \u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}"
    );
    // Protocol + mode line
    {
        let mut mode_parts = vec![report.protocol.to_uppercase()];
        if let (Some(tps), Some(dur)) = (report.tps, report.duration_secs) {
            mode_parts.push(format!("{tps} tx/s"));
            mode_parts.push(format!("{dur}s"));
        }
        println!(
            "  {} -> {}  ({})",
            report.source_chain,
            report.destination_chain,
            mode_parts.join(", "),
        );
    }
    println!(
        "  transactions     {}/{} confirmed ({:.1}% landed)",
        report.total_confirmed,
        report.total_submitted,
        report.landing_rate * 100.0,
    );

    if let Some(ref v) = report.verification {
        println!();

        // Per-stage pipeline counts (computed from transaction records).
        // Shown even when verification times out so partial progress is visible.
        print_pipeline_counts(report);

        // End-to-end line
        match (
            v.avg_executed_secs,
            v.min_executed_secs,
            v.max_executed_secs,
        ) {
            (Some(avg), Some(min), Some(max)) => {
                println!(
                    "  end-to-end       avg {avg:.1}s \u{2502} min {min:.1}s \u{2502} max {max:.1}s"
                );
            }
            (Some(avg), _, Some(max)) => {
                println!("  end-to-end       avg {avg:.1}s \u{2502} max {max:.1}s");
            }
            (Some(avg), _, _) => {
                println!("  end-to-end       avg {avg:.1}s");
            }
            _ => {}
        }

        // Sustained throughput per pipeline step (tx/s over each step's full span).
        {
            let p = &v.peak_throughput;
            let mut parts = Vec::new();
            if let Some(t) = p.voted_tps {
                parts.push(("voted", t));
            }
            if let Some(t) = p.routed_tps {
                parts.push(("routed", t));
            }
            if let Some(t) = p.hub_approved_tps {
                parts.push(("hub approved", t));
            }
            if let Some(t) = p.approved_tps {
                parts.push(("approved", t));
            }
            if let Some(t) = p.executed_tps {
                parts.push(("executed", t));
            }
            if !parts.is_empty() {
                println!("  throughput (sustained, tx/s)");
                for (name, rate) in &parts {
                    println!("    {name:<14} {rate:.1}");
                }
            }
        }

        // Latency percentiles (end-to-end, send → executed).
        {
            let mut latencies: Vec<f64> = report
                .transactions
                .iter()
                .filter_map(|t| t.amplifier_timing.as_ref()?.executed_secs)
                .collect();
            latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = latencies.len();
            if n > 0 {
                let pct = |p: f64| -> f64 {
                    let idx = ((n as f64 * p) as usize).min(n - 1);
                    latencies[idx]
                };
                println!(
                    "  latency          p50 {:.1}s │ p90 {:.1}s │ p99 {:.1}s",
                    pct(0.50),
                    pct(0.90),
                    pct(0.99),
                );
            }
        }

        // Segment breakdown — show step duration + cumulative total.
        // Each avg_*_secs value is cumulative from tx send. Step = current - previous.
        // Steps are sorted by cumulative time so ITS hub_approved (which can precede
        // routed) displays in the correct chronological order.
        let src = &report.source_chain;
        let dst = &report.destination_chain;

        // Collect all available steps with their cumulative totals and labels.
        let mut steps: Vec<(f64, &str, String)> = Vec::new();
        if let Some(total) = report.avg_latency_ms.map(|ms| ms / 1000.0) {
            steps.push((total, "confirm", format!("({src})")));
        }
        if let Some(total) = v.avg_voted_secs {
            steps.push((total, "voted", "(axelar)".to_string()));
        }
        if let Some(total) = v.avg_routed_secs {
            steps.push((total, "routed", "(axelar)".to_string()));
        }
        if let Some(total) = v.avg_hub_approved_secs {
            steps.push((total, "hub approved", "(axelar hub)".to_string()));
        }
        if let Some(total) = v.avg_approved_secs {
            steps.push((total, "approved", format!("({dst})")));
        }
        if let Some(total) = v.avg_executed_secs {
            steps.push((total, "executed", format!("({dst})")));
        }

        // Sort by cumulative time so steps display in chronological order.
        steps.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut prev: Option<f64> = None;
        for (i, (total, name, location)) in steps.iter().enumerate() {
            let step = prev.map_or(*total, |p| total - p);
            let is_last = i == steps.len() - 1;
            let connector = if is_last {
                "\u{2514}\u{2500}"
            } else {
                "\u{251c}\u{2500}"
            };
            println!(
                "  {} step {step:.1}s \u{2502} total {total:.1}s  {}",
                format!("{connector} {name:<13}").dimmed(),
                location.dimmed(),
            );
            prev = Some(*total);
        }

        // Recovered via the final GMP-API check (executed on-chain despite a
        // polling-loop timeout — counted as successful, surfaced distinctly).
        if v.recovered_via_api > 0 {
            println!();
            println!(
                "  recovered via GMP-API final check: {}",
                v.recovered_via_api
            );
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
                v.total_verified,
                if v.total_verified > 0 {
                    v.stuck as f64 / v.total_verified as f64 * 100.0
                } else {
                    0.0
                },
                stuck_detail.join(", "),
            );
        }

        // Failures
        println!("  failures         {}", v.failed - v.stuck,);
        for cat in &v.failure_reasons {
            if !cat.reason.contains("timed out") {
                println!("                   {} \u{00d7} {}", cat.count, cat.reason);
            }
        }
    }
    println!();
}
pub(crate) fn axelar_id_for_chain(config: &std::path::Path, chain_id: &str) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let file: ChainsFile = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse config {}", config.display()))?;
    Ok(file
        .chains
        .get(chain_id)
        .and_then(|c| c.axelar_id.clone())
        .unwrap_or_else(|| chain_id.to_string()))
}

pub(crate) fn read_stellar_network_type(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let file: ChainsFile = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse config {}", config.display()))?;
    Ok(file
        .chains
        .get(chain_id)
        .and_then(|c| c.network_type.clone())
        .unwrap_or_else(|| "testnet".to_string()))
}

pub(crate) fn read_stellar_token_address(
    config: &std::path::Path,
    chain_id: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let file: ChainsFile = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse config {}", config.display()))?;
    file.chains
        .get(chain_id)
        .and_then(|c| c.token_address.clone())
        .ok_or_else(|| {
            eyre::eyre!("no tokenAddress (XLM Soroban contract) for Stellar chain {chain_id}")
        })
}

pub(crate) fn read_stellar_contract_address(
    config: &std::path::Path,
    chain_id: &str,
    contract: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let file: ChainsFile = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse config {}", config.display()))?;
    file.chains
        .get(chain_id)
        .and_then(|c| c.contracts.as_ref())
        .and_then(|cs| cs.get(contract))
        .and_then(|c| c.address.clone())
        .ok_or_else(|| eyre::eyre!("no Stellar contract {contract} for chain {chain_id}"))
}

pub(crate) fn load_stellar_main_wallet(
    private_key: Option<&str>,
) -> Result<crate::stellar::StellarWallet> {
    let key = private_key
        .map(String::from)
        .or_else(|| std::env::var("STELLAR_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre::eyre!(
                "Stellar main wallet required. Set STELLAR_PRIVATE_KEY to either an S... secret key \
                 or a 32-byte hex seed."
            )
        })?;
    if key.starts_with('S') && key.len() > 50 {
        crate::stellar::StellarWallet::from_secret_str(&key)
    } else {
        crate::stellar::StellarWallet::from_hex_seed(&key)
    }
}
pub(crate) async fn ensure_sender_receiver(
    args: &LoadTestArgs,
    rpc_url: &str,
    gateway_addr: Address,
    gas_service_addr: Address,
    cache: serde_json::Value,
    evm_private_key: Option<&str>,
) -> Result<(Address, impl Provider)> {
    if let Some(addr_str) = cache.get("senderReceiverAddress").and_then(|v| v.as_str()) {
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr: Address = addr_str.parse()?;
        let code = get_code_with_retry(&read_provider, addr).await?;
        let needs_redeploy = if code.is_empty() {
            true
        } else {
            let sr = crate::evm::SenderReceiver::new(addr, &read_provider);
            !matches!(sr.gateway().call().await, Ok(onchain_gw) if onchain_gw == gateway_addr)
        };
        if !needs_redeploy {
            // Wallet provider — caller may submit txs through it. Fail loud
            // when no key is configured rather than substitute the historic
            // `0x0…01` placeholder, which was a sweepable-funds footgun.
            let pk = args
                .private_key
                .as_deref()
                .or(evm_private_key)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "EVM private key required to reuse the cached SenderReceiver. \
                     Set EVM_PRIVATE_KEY env var or use --private-key"
                    )
                })?;
            let signer: PrivateKeySigner = pk.parse()?;
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(rpc_url.parse()?);
            return Ok((addr, provider));
        }
    }

    let pk = args.private_key.as_deref().or(evm_private_key).ok_or_else(|| {
        eyre::eyre!(
            "EVM private key required to deploy SenderReceiver. Set EVM_PRIVATE_KEY env var or use --private-key"
        )
    })?;
    let signer: PrivateKeySigner = pk.parse()?;
    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let addr = deploy_sender_receiver(&write_provider, gateway_addr, gas_service_addr).await?;
    let mut cache = cache;
    cache["senderReceiverAddress"] = json!(format!("{addr}"));
    save_cache(&args.destination_chain, &cache)?;
    Ok((addr, write_provider))
}
/// Resolve the Sui-side ITS token id (32B) and optional coin type tag for
/// AXE, defaulting to whatever `axelar-contract-deployments/sui/its.js
/// register-coin-from-info` wrote into the chain config (`chains.<sui>.contracts.AXE`).
///
/// Returning `None` for the coin type lets the caller fall back to the
/// dev-inspect path. CLI flags (`--token-id`, `--coin-type`) override.
pub(crate) fn resolve_sui_axe_token(
    config: &std::path::Path,
    sui_chain_id: &str,
    cli_token_id: Option<&str>,
    cli_coin_type: Option<&str>,
) -> Result<([u8; 32], Option<String>)> {
    let parse_tid = |s: &str| -> Result<[u8; 32]> {
        let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s))
            .map_err(|e| eyre::eyre!("invalid token id hex: {e}"))?;
        if bytes.len() != 32 {
            eyre::bail!("token id must be 32 bytes (got {})", bytes.len());
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    };

    if let Some(t) = cli_token_id {
        return Ok((parse_tid(t)?, cli_coin_type.map(str::to_string)));
    }

    // Fall back to the chain config's AXE entry.
    let content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    let axe = root
        .pointer(&format!("/chains/{sui_chain_id}/contracts/AXE"))
        .ok_or_else(|| {
            eyre::eyre!(
                "no `contracts.AXE` entry under chain `{sui_chain_id}` in config — \
                 either pass --token-id, or pre-register AXE via\n  \
                 cd axelar-contract-deployments && PRIVATE_KEY=$SUI_PRIVATE_KEY \\\n    \
                 SUI_RPC=https://fullnode.testnet.sui.io:443 \\\n    \
                 node sui/its.js register-coin-from-info AXE AXE 9 -e testnet -n {sui_chain_id} -y"
            )
        })?;
    let tid_str = axe
        .pointer("/objects/TokenId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("contracts.AXE.objects.TokenId missing in config"))?;
    let coin_type = cli_coin_type.map(str::to_string).or_else(|| {
        axe.get("typeArgument")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });
    Ok((parse_tid(tid_str)?, coin_type))
}

/// Read a pre-registered AXE ITS token id from chains-config, if one is
/// recorded for `<chain_axelar_id>`. Returns `None` when the entry is absent
/// — callers fall through to their normal cache-or-deploy path.
///
/// Schema (per source chain in chains-config):
/// ```json
/// "chains": { "<axelar_id>": { "contracts": { "AXE": { "tokenId": "0x..." } } } }
/// ```
///
/// Once an AXE token has been deployed on a source chain (and its remote
/// counterparts registered on each destination of interest), recording the
/// tokenId here lets axe skip the full deploy + hub-routed remote-deploy
/// dance on every CI run. Each run then collapses to a single
/// `interchainTransfer` call — much smaller surface area for transient
/// Amplifier failures (e.g. "message not approved" race during execute).
pub(crate) fn read_pre_registered_axe_token(
    config: &std::path::Path,
    chain_axelar_id: &str,
) -> Result<Option<alloy::primitives::FixedBytes<32>>> {
    let content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    let tid_str = root
        .pointer(&format!("/chains/{chain_axelar_id}/contracts/AXE/tokenId"))
        .and_then(|v| v.as_str());
    match tid_str {
        Some(s) => {
            let stripped = s.strip_prefix("0x").unwrap_or(s);
            let bytes = hex::decode(stripped).map_err(|e| {
                eyre::eyre!("invalid AXE.tokenId hex for chain {chain_axelar_id}: {e}")
            })?;
            if bytes.len() != 32 {
                eyre::bail!(
                    "AXE.tokenId for chain {chain_axelar_id} must be 32 bytes (got {})",
                    bytes.len()
                );
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            Ok(Some(alloy::primitives::FixedBytes::from(out)))
        }
        None => Ok(None),
    }
}

/// Companion to `read_pre_registered_axe_token`: returns the AXE
/// `tokenAddress` (the chain-side ERC20 / HTS address) recorded under
/// `chains.<chain>.contracts.AXE.tokenAddress`, if present.
///
/// **Why this exists alongside the on-chain `interchainTokenAddress(tid)`
/// view**: on Hedera mainnet/testnet the standard view reverts because
/// Hedera's HTS-fork of ITS doesn't expose the EVM-style getter — the
/// underlying token is an HTS-native asset whose EVM address is
/// determined at HTS-create time and surfaced only in the deploy
/// receipt, not retrievable later via a generic ITS function. For those
/// chains we record the deploy-receipt address directly in chains-config
/// and read it from there. Returns `None` when no override is set,
/// letting the caller fall back to the on-chain view (the right call
/// for non-Hedera EVMs).
pub(crate) fn read_pre_registered_axe_token_address(
    config: &std::path::Path,
    chain_axelar_id: &str,
) -> Result<Option<Address>> {
    let content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    // The chains-config schema uses `address` on PascalCase contract entries
    // (validated in axelar-chains-config/tests/schema). Earlier drafts called
    // this `tokenAddress` — reading under that name silently returned None and
    // the caller fell back to ITS.interchainTokenAddress(tokenId), which
    // reverts on the Hedera HTS-fork (its `registeredTokenAddress` view
    // replaces it).
    let addr_str = root
        .pointer(&format!("/chains/{chain_axelar_id}/contracts/AXE/address"))
        .and_then(|v| v.as_str());
    match addr_str {
        Some(s) => Ok(Some(s.parse().map_err(|e| {
            eyre::eyre!("invalid AXE.address for chain {chain_axelar_id}: {e}")
        })?)),
        None => Ok(None),
    }
}

/// Reuse the chains-config pre-registered AXE token *only* when the configured
/// wallet actually holds enough of it to run. Returns `Some((tokenId, addr))`
/// when reuse is viable, or `None` (so the caller falls through to its local
/// cache / fresh-deploy path) when there's no config entry or the holder's
/// balance is below `needed`.
///
/// This keeps the two intended cases working: a workflow / CI run whose wallet
/// already holds the AXE supply reuses the deployed token (no source + remote
/// deploy), while a different wallet with no AXE balance deploys fresh exactly
/// as the manual path does today.
pub(crate) async fn reusable_config_axe<P: Provider>(
    config: &std::path::Path,
    chain_axelar_id: &str,
    its_proxy: Address,
    provider: &P,
    holder: Address,
    needed: alloy::primitives::U256,
) -> Result<Option<(alloy::primitives::FixedBytes<32>, Address)>> {
    let Some(tid) = read_pre_registered_axe_token(config, chain_axelar_id)? else {
        return Ok(None);
    };
    // Prefer the config-supplied tokenAddress when set (required for
    // Hedera HTS — see `read_pre_registered_axe_token_address` docs).
    // Fall back to the on-chain `interchainTokenAddress(tid)` view for
    // standard EVMs that don't pre-record the address in chains-config.
    let addr = if let Some(config_addr) =
        read_pre_registered_axe_token_address(config, chain_axelar_id)?
    {
        config_addr
    } else {
        let its = crate::evm::InterchainTokenService::new(its_proxy, provider);
        its.interchainTokenAddress(tid)
            .call()
            .await
            .map_err(|e| eyre::eyre!("failed to look up token address for {tid}: {e}"))?
    };
    // `needed` from the caller is computed assuming 18-decimal EVM AXE
    // (the convention in compute_run_sizing). For Hedera HTS-fork (6 dec) a
    // wallet holding 10000 AXE = 1e10 sub-units would be treated as
    // "insufficient" against needed=1e18, falling through to a fresh deploy
    // that then reverts with InitialSupplyUnsupported. Scale `needed` by the
    // source token's actual decimals so the comparison is apples-to-apples.
    let token = crate::evm::ERC20::new(addr, provider);
    let decimals: u8 = token.decimals().call().await.unwrap_or(18);
    let scaled_needed = if decimals < 18 {
        needed
            / alloy::primitives::U256::from(10)
                .pow(alloy::primitives::U256::from(18 - u32::from(decimals)))
    } else {
        needed
    };
    let balance = token.balanceOf(holder).call().await.unwrap_or_default();
    if balance >= scaled_needed {
        Ok(Some((tid, addr)))
    } else {
        ui::warn(&format!(
            "chains-config AXE balance too low for {holder} \
             ({balance} < {scaled_needed} at {decimals} decimals); \
             configured wallet isn't the workflow deployer — deploying fresh..."
        ));
        Ok(None)
    }
}

/// Print a hint suggesting the caller add the freshly-deployed AXE tokenId to
/// chains-config so subsequent CI runs skip the deploy. Called from the
/// per-chain deploy helpers right after the source-side deploy succeeds.
pub(crate) fn hint_persist_axe_token(
    chain_axelar_id: &str,
    token_id: &alloy::primitives::FixedBytes<32>,
) {
    ui::info(&format!(
        "💡 To skip the deploy on future runs, add to chains-config:\n  \
         chains.{chain_axelar_id}.contracts.AXE.tokenId = \"{token_id}\"\n  \
         (assumes the destination chains you care about already have the remote registered)"
    ));
}

/// Resolve the pre-registered AXE token ID for Hedera-source ITS runs.
///
/// axe can't auto-deploy on Hedera because the Hedera fork of the
/// InterchainTokenFactory rejects `initialSupply > 0` (selector `0x6ea43cd7`,
/// `InitialSupplyUnsupported()`) and the HTS-backed receive side needs WHBAR
/// fund + factory approval to mint. Pre-deploy the token via the
/// axelar-contract-deployments Hedera flow (fund-whbar.js →
/// approve-factory-whbar.js → deploy-interchain-token --initialSupply 0 →
/// HTS mint), then either pass `--token-id` or record the token id at
/// `chains.<hedera-id>.contracts.AXE.tokenId` in chains-config.
pub(crate) fn resolve_hedera_axe_token(
    config: &std::path::Path,
    hedera_chain_id: &str,
    cli_token_id: Option<&str>,
) -> Result<alloy::primitives::FixedBytes<32>> {
    let parse_tid = |s: &str| -> Result<alloy::primitives::FixedBytes<32>> {
        let stripped = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(stripped).map_err(|e| eyre::eyre!("invalid hex token id: {e}"))?;
        if bytes.len() != 32 {
            eyre::bail!("token id must be 32 bytes (got {})", bytes.len());
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(alloy::primitives::FixedBytes::from(out))
    };

    if let Some(t) = cli_token_id {
        return parse_tid(t);
    }

    let content = std::fs::read_to_string(config)
        .map_err(|e| eyre::eyre!("failed to read config {}: {e}", config.display()))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    let tid_str = root
        .pointer(&format!(
            "/chains/{hedera_chain_id}/contracts/AXE/tokenId"
        ))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            eyre::eyre!(
                "no `contracts.AXE.tokenId` entry under chain `{hedera_chain_id}` in config — \
                 either pass --token-id, or pre-register AXE via the deployments repo:\n  \
                 cd axelar-contract-deployments && \\\n  \
                 PRIVATE_KEY=$EVM_PRIVATE_KEY node hedera/fund-whbar.js <addr> --amount 10 -e testnet -n {hedera_chain_id} -y && \\\n  \
                 PRIVATE_KEY=$EVM_PRIVATE_KEY node hedera/approve-factory-whbar.js --amount max -e testnet -n {hedera_chain_id} -y && \\\n  \
                 PRIVATE_KEY=$EVM_PRIVATE_KEY node evm/interchainTokenFactory.js deploy-interchain-token \\\n    \
                 --salt 'axe-loadtest' --name AXE --symbol AXE --decimals 18 --initialSupply 0 \\\n    \
                 --minter $(cast wallet address $EVM_PRIVATE_KEY) -e testnet -n {hedera_chain_id}\n  \
                 # then mint via HTS and add the printed token id to chains-config under contracts.AXE.tokenId"
            )
        })?;
    parse_tid(tid_str)
}

pub(crate) fn load_sui_main_wallet() -> Result<crate::sui::SuiWallet> {
    let key = std::env::var("SUI_PRIVATE_KEY").map_err(|_| {
        eyre::eyre!(
            "SUI_PRIVATE_KEY required (a `suiprivkey1...` bech32 secret from `sui keytool` or 64-char hex). Add it to .env."
        )
    })?;
    crate::sui::SuiWallet::from_secret_str(&key)
}
pub(crate) fn sui_object_id(
    config: &std::path::Path,
    chain_id: &str,
    pointer_within_chain: &str,
) -> Result<String> {
    let content =
        std::fs::read_to_string(config).map_err(|e| eyre::eyre!("failed to read config: {e}"))?;
    let root: serde_json::Value = serde_json::from_str(&content)?;
    root.pointer(&format!("/chains/{chain_id}{pointer_within_chain}"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| eyre::eyre!("no Sui object {pointer_within_chain} for chain {chain_id}"))
}

/// Read `(sui_channel_id, sui_rpc)` from the chains config. `rpc_override`
/// lets the caller honor `--destination-rpc` / `DESTINATION_RPC` from
/// `LoadTestArgs::destination_rpc`. An empty/None override falls back to
/// the chain config's `rpc` field.
pub(crate) fn sui_dest_lookup(
    config: &std::path::Path,
    sui_chain_id: &str,
    rpc_override: Option<&str>,
) -> Result<(String, String)> {
    let channel = sui_object_id(
        config,
        sui_chain_id,
        "/contracts/Example/objects/GmpChannelId",
    )?;
    let (config_rpc, _contracts) = crate::sui::read_sui_chain_config(config, sui_chain_id)?;
    let rpc = match rpc_override {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => config_rpc,
    };
    Ok((channel, rpc))
}

/// Same shape as [`sui_dest_lookup`] but resolves the channel id for the
/// ITS-example contract. Inbound ITS messages are delivered to the
/// `ItsChannelId` (not the GMP one) — `MessageExecuted` events fire with
/// this address, and the cgp-sui relayer auto-calls
/// `example::its::receive_interchain_transfer<T>` against it on
/// `MessageApproved`.
pub(crate) fn sui_its_dest_lookup(
    config: &std::path::Path,
    sui_chain_id: &str,
    rpc_override: Option<&str>,
) -> Result<(String, String)> {
    let channel = sui_object_id(
        config,
        sui_chain_id,
        "/contracts/Example/objects/ItsChannelId",
    )?;
    let (config_rpc, _contracts) = crate::sui::read_sui_chain_config(config, sui_chain_id)?;
    let rpc = match rpc_override {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => config_rpc,
    };
    Ok((channel, rpc))
}

/// Read the Sui-side AXE token id (32 bytes) from the chain config. The
/// off-axe `sui/its.js register-coin-from-info` step populates this field
/// — for *→Sui ITS runs, the source chain must have a matching tokenId
/// linked via `link-token` so source-side `interchainToken(tokenId)` returns
/// a non-zero address. CLI overrides via `--token-id` honored.
pub(crate) fn read_sui_axe_token_id(
    config: &std::path::Path,
    sui_chain_id: &str,
    cli_override: Option<&str>,
) -> Result<[u8; 32]> {
    let hex_str = if let Some(s) = cli_override.filter(|s| !s.is_empty()) {
        s.to_string()
    } else {
        sui_object_id(config, sui_chain_id, "/contracts/AXE/objects/TokenId")?
    };
    let bytes = hex::decode(hex_str.trim_start_matches("0x"))
        .map_err(|e| eyre::eyre!("Sui AXE TokenId hex decode: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| eyre::eyre!("Sui AXE TokenId must be exactly 32 bytes"))
}

/// Run the Sui destination verifier and stamp the report. Shared between
/// `run_evm_to_sui`, `run_sol_to_sui`, `run_stellar_to_sui`.
pub(crate) async fn finalize_sui_dest_run(
    args: &LoadTestArgs,
    report: &mut crate::commands::load_test::metrics::LoadTestReport,
    sui_channel: &str,
    sui_rpc: &str,
    source_type: verify::SourceChainType,
    test_start: Instant,
) -> Result<()> {
    super::gmp_verification::finish_batch(
        args,
        &super::gmp_verification::SuiGmpTarget {
            address: sui_channel,
            rpc_url: sui_rpc,
            source_type,
        },
        report,
        test_start,
    )
    .await
}

/// ITS-to-Sui finalizer: like [`finalize_sui_dest_run`] but routes through the
/// two-leg hub verifier ([`verify::verify_onchain_sui_its`]) so the hub→Sui
/// second leg (routed → approved → executed) is actually tracked. Raw
/// GMP-to-Sui stays on `finalize_sui_dest_run` (single leg).
pub(crate) async fn finalize_sui_dest_run_its(
    args: &LoadTestArgs,
    report: &mut crate::commands::load_test::metrics::LoadTestReport,
    sui_rpc: &str,
    test_start: Instant,
) -> Result<()> {
    super::its_verification::finish_batch(
        args,
        &super::its_verification::SuiItsTarget {
            rpc_url: sui_rpc.to_string(),
        },
        report,
        test_start,
    )
    .await
}
