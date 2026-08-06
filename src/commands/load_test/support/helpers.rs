//! Shared load test helpers.
//! SenderReceiver deploy/reuse, RPC validation, the report finalizer, and the
//! per-chain config readers (Stellar/Sui wallet loaders, JSON-pointer
//! contract-address lookups). Most of this module is `pub(super)` — only
//! `ensure_sender_receiver_on_evm_chain` is `pub(crate)` because
//! `commands::test_gmp` calls into it for the `--config` sol→evm flow.

use std::cmp::Ordering;
use std::env;
use std::path::Path;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::transports::TransportError;
use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::Result;
use owo_colors::OwoColorize;
use solana_client::nonblocking::rpc_client::RpcClient as AsyncSolanaRpcClient;
use tokio::{fs, time};

use super::metrics::{AmplifierTiming, LoadTestReport, VerificationReport};
use super::verify;
use super::{GmpCache, LoadTestArgs, read_cache, save_cache};
use crate::config::{ChainContract, ChainsConfig};
use crate::evm::{ERC20, InterchainTokenService, SenderReceiver, read_artifact_bytecode};
use crate::retry::{FALLBACK_ATTEMPTS, retry_all, retry_async};
use crate::stellar::StellarWallet;
use crate::sui::{SuiWallet, read_sui_chain_config};
use crate::ui;

pub(crate) async fn ensure_sender_receiver_on_evm_chain(
    chain: &str,
    rpc_url: &str,
    evm_private_key: &str,
    gateway_addr: Address,
    gas_service_addr: Address,
) -> Result<Address> {
    let signer: PrivateKeySigner = evm_private_key.parse()?;
    let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);
    let cache = read_cache(chain).await;
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
            time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
        }
        last = provider.get_code_at(addr).await?;
        if !last.is_empty() {
            return Ok(last);
        }
    }
    Ok(last)
}

pub(crate) async fn deploy_or_reuse_sender_receiver<R: Provider, W: Provider>(
    cache: &GmpCache,
    cache_key: &str,
    read_provider: &R,
    write_provider: &W,
    gateway_addr: Address,
    gas_service_addr: Address,
    label: &str,
) -> Result<Address> {
    if let Some(addr_str) = cache.sender_receiver_address.as_deref() {
        let addr: Address = addr_str.parse()?;
        let code = get_code_with_retry(read_provider, addr).await?;
        let needs_redeploy = if code.is_empty() {
            ui::warn(&format!(
                "cached SenderReceiver ({label}) has no code, redeploying..."
            ));
            true
        } else {
            // Verify the cached contract's gateway matches the current config.
            let sr = SenderReceiver::new(addr, read_provider);
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
            cache.sender_receiver_address = Some(new_addr.to_string());
            save_cache(cache_key, &cache).await?;
            Ok(new_addr)
        } else {
            ui::info(&format!("SenderReceiver ({label}): reusing {addr}"));
            Ok(addr)
        }
    } else {
        ui::info(&format!("deploying SenderReceiver on {label} chain..."));
        let addr = deploy_sender_receiver(write_provider, gateway_addr, gas_service_addr).await?;
        let mut cache = cache.clone();
        cache.sender_receiver_address = Some(addr.to_string());
        save_cache(cache_key, &cache).await?;
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

pub(crate) async fn finish_report(report: &mut LoadTestReport, run_start: Instant) -> Result<()> {
    // Scrub any URLs that upstream-crate errors may have folded into
    // per-tx error strings. Belt-and-suspenders alongside the error-template
    // refactor: private RPC URLs (from repo secrets) must not appear in the
    // JSON artifact, regardless of where the underlying error came from.
    for tx in &mut report.transactions {
        tx.map_error(scrub_urls);
    }
    print_final_report(report);

    // Write full JSON report to a timestamped file so failures can be inspected afterwards.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_dir = Path::new("axe-load-test-logs");
    let log_path = log_dir.join(format!("axe-load-test-{ts}.json"));
    let mut report_written = false;
    match fs::create_dir_all(log_dir).await {
        Ok(()) => match serde_json::to_string_pretty(report) {
            Ok(json) => match fs::write(&log_path, &json).await {
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
    // Retried: a transient blip on the pre-flight probe must not kill a run
    // whose actual sends carry full retry + fallback.
    retry_all("validate_evm_rpc", || provider.get_chain_id())
        .await
        .map_err(|_| {
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
    addr: Address,
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
    let client = AsyncSolanaRpcClient::new(rpc_url.to_string());
    client.get_version().await.map_err(|_| {
        eyre::eyre!(
            "RPC '{rpc_url}' does not appear to be a Solana endpoint \
             (getVersion failed). Check that you're using the correct RPC URL."
        )
    })?;
    Ok(())
}

pub(crate) async fn check_evm_balance<P: Provider>(provider: &P, address: Address) -> Result<()> {
    let balance = provider.get_balance(address).await?;
    if balance.is_zero() {
        eyre::bail!(
            "EVM wallet {address} has no funds. Fund it first:\n  \
             Use a faucet or transfer native tokens to {address}"
        );
    }
    Ok(())
}

pub(crate) async fn deploy_sender_receiver<P: Provider>(
    provider: &P,
    gateway: Address,
    gas_service: Address,
) -> Result<Address> {
    // Legacy (pre-1559) chains break alloy's default fee estimation; detect and
    // send a type-0 tx with an explicit gas_price instead. Retried: two
    // read-only calls whose transient failure would otherwise abort the deploy.
    let fee_mode = retry_all("detect fee mode", || {
        super::gas_mode::EvmFeeMode::detect(provider)
    })
    .await?;

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
async fn choose_deploy_artifact<P: Provider>(
    provider: &P,
    gateway: Address,
    gas_service: Address,
) -> Result<&'static str> {
    const DEFAULT: &str = "artifacts/SenderReceiver.json";
    const PARIS: &str = "artifacts/SenderReceiver.paris.json";
    let mut code = read_artifact_bytecode(DEFAULT).await?;
    code.extend_from_slice(&(gateway, gas_service).abi_encode_params());
    let probe = TransactionRequest::default().with_deploy_code(Bytes::from(code));
    match provider.call(probe).await {
        Err(e) if is_unsupported_opcode(&e) => {
            ui::info("destination rejects PUSH0 (pre-Shanghai EVM); using paris bytecode...");
            Ok(PARIS)
        }
        _ => Ok(DEFAULT),
    }
}

/// True if an error is a chain rejecting an opcode the bytecode uses (e.g.
/// `PUSH0` on a pre-Shanghai EVM) — the signal to use the paris build.
fn is_unsupported_opcode(error: &TransportError) -> bool {
    let Some(response) = error.as_error_resp() else {
        return false;
    };
    let message = response.message.to_lowercase();
    if message.contains("push0") || message.contains("invalid opcode") {
        return true;
    }
    response.data.as_ref().is_some_and(|data| {
        let data = data.get().to_lowercase();
        data.contains("push0") || data.contains("invalid opcode")
    })
}

fn is_retryable_evm_transport(error: &TransportError) -> bool {
    if error.as_error_resp().is_some_and(|response| {
        // Avalanche's pending-state rejection isn't in alloy's retryable set
        // but is endpoint-transient (see `retry::is_transient_default`).
        response.is_retry_err()
            || response
                .message
                .contains("state not available for pending block")
    }) {
        return true;
    }
    let Some(transport) = error.as_transport_err() else {
        return false;
    };
    if transport.is_retry_err() {
        return true;
    }
    if transport
        .as_http_error()
        .is_some_and(|http| matches!(http.status, 502..=504))
    {
        return true;
    }
    transport
        .as_custom()
        .and_then(|source| source.downcast_ref::<reqwest::Error>())
        .is_some_and(|source| source.is_timeout() || source.is_connect())
}

/// Deploy the SenderReceiver from a specific bytecode artifact and return its
/// address. Split out so `deploy_sender_receiver` can retry with an alternate
/// (paris) artifact when the chain rejects the default bytecode's opcodes.
async fn deploy_with_artifact<P: Provider>(
    provider: &P,
    artifact_path: &str,
    gateway: Address,
    gas_service: Address,
    fee_mode: super::gas_mode::EvmFeeMode,
) -> Result<Address> {
    let mut deploy_code = read_artifact_bytecode(artifact_path).await?;
    deploy_code.extend_from_slice(&(gateway, gas_service).abi_encode_params());

    // Wrap `send_transaction` with retry on transient transport / 5xx /
    // 429 / pending-state errors — observed flakes on HL, Hedera, and
    // Avalanche RPCs where the same submission succeeds on retry. Real
    // reverts (insufficient funds, custom errors) are typed RPC responses
    // and skip the retry. FALLBACK_ATTEMPTS (5): the Avalanche pending-state
    // stretch outlasts the 3-attempt budget (proven by stagenet run
    // 31115899822, which died here).
    let pending = retry_async(
        "deploy_sender_receiver.send_transaction",
        FALLBACK_ATTEMPTS,
        is_retryable_evm_transport,
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
    let receipt = time::timeout(Duration::from_secs(240), pending.get_receipt())
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

fn print_end_to_end_latency(verification: &VerificationReport) {
    match (
        verification.avg_executed_secs,
        verification.min_executed_secs,
        verification.max_executed_secs,
    ) {
        (Some(avg), Some(min), Some(max)) => {
            println!("  end-to-end       avg {avg:.1}s │ min {min:.1}s │ max {max:.1}s");
        }
        (Some(avg), _, Some(max)) => println!("  end-to-end       avg {avg:.1}s │ max {max:.1}s"),
        (Some(avg), _, _) => println!("  end-to-end       avg {avg:.1}s"),
        _ => {}
    }
}

fn print_latency_percentiles(report: &LoadTestReport) {
    let mut latencies: Vec<f64> = report
        .transactions
        .iter()
        .filter_map(|tx| tx.amplifier_timing.as_ref()?.executed_secs)
        .collect();
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    if !latencies.is_empty() {
        let percentile = |p: f64| {
            let index = ((latencies.len() as f64 * p) as usize).min(latencies.len() - 1);
            latencies[index]
        };
        println!(
            "  latency          p50 {:.1}s │ p90 {:.1}s │ p99 {:.1}s",
            percentile(0.50),
            percentile(0.90),
            percentile(0.99),
        );
    }
}

fn print_verification_throughput(verification: &VerificationReport) {
    let throughput = &verification.peak_throughput;
    let rates = [
        ("voted", throughput.voted_tps),
        ("routed", throughput.routed_tps),
        ("hub approved", throughput.hub_approved_tps),
        ("approved", throughput.approved_tps),
        ("executed", throughput.executed_tps),
    ];
    let rates = rates
        .into_iter()
        .filter_map(|(name, rate)| rate.map(|rate| (name, rate)))
        .collect::<Vec<_>>();
    if !rates.is_empty() {
        println!("  throughput (sustained, tx/s)");
        for (name, rate) in rates {
            println!("    {name:<14} {rate:.1}");
        }
    }
}

fn print_verification_segments(report: &LoadTestReport, verification: &VerificationReport) {
    let source = &report.source_chain;
    let destination = &report.destination_chain;
    let mut steps: Vec<(f64, &str, String)> = Vec::new();
    if let Some(total) = report.avg_latency_ms.map(|millis| millis / 1000.0) {
        steps.push((total, "confirm", format!("({source})")));
    }
    let pipeline_steps = [
        (verification.avg_voted_secs, "voted", "(axelar)".to_string()),
        (
            verification.avg_routed_secs,
            "routed",
            "(axelar)".to_string(),
        ),
        (
            verification.avg_hub_approved_secs,
            "hub approved",
            "(axelar hub)".to_string(),
        ),
        (
            verification.avg_approved_secs,
            "approved",
            format!("({destination})"),
        ),
        (
            verification.avg_executed_secs,
            "executed",
            format!("({destination})"),
        ),
    ];
    steps.extend(
        pipeline_steps
            .into_iter()
            .filter_map(|(total, name, location)| total.map(|total| (total, name, location))),
    );
    steps.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));

    let mut previous = None;
    let last = steps.len().saturating_sub(1);
    for (index, (total, name, location)) in steps.into_iter().enumerate() {
        let step = previous.map_or(total, |prior| total - prior);
        let connector = if index == last { "└─" } else { "├─" };
        println!(
            "  {} step {step:.1}s │ total {total:.1}s  {}",
            format!("{connector} {name:<13}").dimmed(),
            location.dimmed(),
        );
        previous = Some(total);
    }
}

fn print_verification_failures(verification: &VerificationReport) {
    if verification.recovered_via_api > 0 {
        println!();
        println!(
            "  recovered via GMP-API final check: {}",
            verification.recovered_via_api
        );
    }
    if verification.stuck > 0 {
        let detail = verification
            .stuck_at
            .iter()
            .map(|category| format!("{} at {}", category.count, category.reason))
            .collect::<Vec<_>>()
            .join(", ");
        println!();
        println!(
            "  stuck            {}/{} ({:.1}%) — {detail}",
            verification.stuck,
            verification.total_verified,
            if verification.total_verified > 0 {
                verification.stuck as f64 / verification.total_verified as f64 * 100.0
            } else {
                0.0
            },
        );
    }
    println!(
        "  failures         {}",
        verification.failed - verification.stuck
    );
    for category in &verification.failure_reasons {
        if !category.is_timed_out() {
            println!(
                "                   {} × {}",
                category.count, category.reason
            );
        }
    }
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
        print_pipeline_counts(report);
        print_end_to_end_latency(v);
        print_verification_throughput(v);
        print_latency_percentiles(report);
        print_verification_segments(report, v);
        print_verification_failures(v);
    }
    println!();
}
pub(crate) async fn axelar_id_for_chain(config: &Path, chain_id: &str) -> Result<String> {
    let config = ChainsConfig::load(config).await?;
    Ok(config.chain(chain_id)?.axelar_id_or(chain_id))
}

pub(crate) async fn read_stellar_network_type(config: &Path, chain_id: &str) -> Result<String> {
    let config = ChainsConfig::load(config).await?;
    Ok(config
        .chain(chain_id)?
        .network_type
        .clone()
        .unwrap_or_else(|| "testnet".to_string()))
}

pub(crate) async fn read_stellar_token_address(config: &Path, chain_id: &str) -> Result<String> {
    let config = ChainsConfig::load(config).await?;
    config
        .chain(chain_id)?
        .token_address
        .clone()
        .ok_or_else(|| {
            eyre::eyre!("no tokenAddress (XLM Soroban contract) for Stellar chain {chain_id}")
        })
}

pub(crate) async fn read_stellar_contract_address(
    config: &Path,
    chain_id: &str,
    contract: ChainContract,
) -> Result<String> {
    let config = ChainsConfig::load(config).await?;
    Ok(config
        .chain(chain_id)?
        .contract_address(contract, chain_id)?
        .to_string())
}

pub(crate) fn load_stellar_main_wallet(private_key: Option<&str>) -> Result<StellarWallet> {
    let key = private_key
        .map(String::from)
        .or_else(|| env::var("STELLAR_PRIVATE_KEY").ok())
        .ok_or_else(|| {
            eyre::eyre!(
                "Stellar main wallet required. Set STELLAR_PRIVATE_KEY to either an S... secret key \
                 or a 32-byte hex seed."
            )
        })?;
    if key.starts_with('S') && key.len() > 50 {
        StellarWallet::from_secret_str(&key)
    } else {
        StellarWallet::from_hex_seed(&key)
    }
}
pub(crate) async fn ensure_sender_receiver(
    args: &LoadTestArgs,
    rpc_url: &str,
    gateway_addr: Address,
    gas_service_addr: Address,
    cache: GmpCache,
    evm_private_key: Option<&str>,
) -> Result<(Address, impl Provider + use<>)> {
    if let Some(addr_str) = cache.sender_receiver_address.as_deref() {
        let read_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let addr: Address = addr_str.parse()?;
        let code = get_code_with_retry(&read_provider, addr).await?;
        let needs_redeploy = if code.is_empty() {
            true
        } else {
            let sr = SenderReceiver::new(addr, &read_provider);
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
    cache.sender_receiver_address = Some(addr.to_string());
    save_cache(&args.destination_chain, &cache).await?;
    Ok((addr, write_provider))
}
/// Resolve the Sui-side ITS token id (32B) and optional coin type tag for
/// AXE, defaulting to whatever `axelar-contract-deployments/sui/its.js
/// register-coin-from-info` wrote into the chain config (`chains.<sui>.contracts.AXE`).
///
/// Returning `None` for the coin type lets the caller fall back to the
/// dev-inspect path. CLI flags (`--token-id`, `--coin-type`) override.
pub(crate) async fn resolve_sui_axe_token(
    config: &Path,
    sui_chain_id: &str,
    cli_token_id: Option<&str>,
    cli_coin_type: Option<&str>,
) -> Result<([u8; 32], Option<String>)> {
    if let Some(t) = cli_token_id {
        return Ok((
            parse_token_id_hex(t, "token id")?,
            cli_coin_type.map(str::to_string),
        ));
    }

    // Fall back to the chain config's AXE entry.
    let config = ChainsConfig::load(config).await?;
    let axe = config
        .chain(sui_chain_id)?
        .contract(ChainContract::Axe, sui_chain_id)
        .map_err(|_| {
            eyre::eyre!(
                "no `contracts.AXE` entry under chain `{sui_chain_id}` in config — \
                 either pass --token-id, or pre-register AXE via\n  \
                 cd axelar-contract-deployments && PRIVATE_KEY=$SUI_PRIVATE_KEY \\\n    \
                 SUI_RPC=https://fullnode.testnet.sui.io:443 \\\n    \
                 node sui/its.js register-coin-from-info AXE AXE 9 -e testnet -n {sui_chain_id} -y"
            )
        })?;
    let tid_str = axe
        .objects
        .as_ref()
        .and_then(|objects| objects.token_id.as_deref())
        .ok_or_else(|| eyre::eyre!("contracts.AXE.objects.TokenId missing in config"))?;
    let coin_type = cli_coin_type
        .map(str::to_string)
        .or_else(|| axe.type_argument.clone());
    Ok((parse_token_id_hex(tid_str, "token id")?, coin_type))
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
pub(crate) async fn read_pre_registered_axe_token(
    config: &Path,
    chain_axelar_id: &str,
) -> Result<Option<FixedBytes<32>>> {
    let config = ChainsConfig::load(config).await?;
    let tid_str = config
        .chains
        .get(chain_axelar_id)
        .and_then(|chain| chain.contracts.as_ref())
        .and_then(|contracts| contracts.get("AXE"))
        .and_then(|axe| axe.token_id.as_deref());
    match tid_str {
        Some(value) => Ok(Some(FixedBytes::from(parse_token_id_hex(
            value,
            &format!("AXE.tokenId for chain {chain_axelar_id}"),
        )?))),
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
pub(crate) async fn read_pre_registered_axe_token_address(
    config: &Path,
    chain_axelar_id: &str,
) -> Result<Option<Address>> {
    // The chains-config schema uses `address` on PascalCase contract entries
    // (validated in axelar-chains-config/tests/schema). Earlier drafts called
    // this `tokenAddress` — reading under that name silently returned None and
    // the caller fell back to ITS.interchainTokenAddress(tokenId), which
    // reverts on the Hedera HTS-fork (its `registeredTokenAddress` view
    // replaces it).
    let config = ChainsConfig::load(config).await?;
    let addr_str = config
        .chains
        .get(chain_axelar_id)
        .and_then(|chain| chain.contracts.as_ref())
        .and_then(|contracts| contracts.get("AXE"))
        .and_then(|axe| axe.address.as_deref());
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
    config: &Path,
    chain_axelar_id: &str,
    its_proxy: Address,
    provider: &P,
    holder: Address,
    needed: U256,
) -> Result<Option<(FixedBytes<32>, Address)>> {
    let Some(tid) = read_pre_registered_axe_token(config, chain_axelar_id).await? else {
        return Ok(None);
    };
    // Prefer the config-supplied tokenAddress when set (required for
    // Hedera HTS — see `read_pre_registered_axe_token_address` docs).
    // Fall back to the on-chain `interchainTokenAddress(tid)` view for
    // standard EVMs that don't pre-record the address in chains-config.
    let addr = if let Some(config_addr) =
        read_pre_registered_axe_token_address(config, chain_axelar_id).await?
    {
        config_addr
    } else {
        let its = InterchainTokenService::new(its_proxy, provider);
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
    let token = ERC20::new(addr, provider);
    let decimals: u8 = retry_all("config AXE decimals", || async {
        token.decimals().call().await
    })
    .await
    .unwrap_or(18);
    let scaled_needed = if decimals < 18 {
        needed / U256::from(10).pow(U256::from(18 - u32::from(decimals)))
    } else {
        needed
    };
    // Retried: a transient error that silently reads as balance 0 would
    // trigger a spurious fresh deploy of the config AXE token.
    let balance = retry_all("config AXE balance", || async {
        token.balanceOf(holder).call().await
    })
    .await
    .unwrap_or_default();
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
pub(crate) fn hint_persist_axe_token(chain_axelar_id: &str, token_id: &FixedBytes<32>) {
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
pub(crate) async fn resolve_hedera_axe_token(
    config: &Path,
    hedera_chain_id: &str,
    cli_token_id: Option<&str>,
) -> Result<FixedBytes<32>> {
    if let Some(t) = cli_token_id {
        return Ok(FixedBytes::from(parse_token_id_hex(t, "token id")?));
    }

    let config = ChainsConfig::load(config).await?;
    let tid_str = config
        .chain(hedera_chain_id)?
        .contract(ChainContract::Axe, hedera_chain_id)?
        .token_id
        .as_deref()
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
    Ok(FixedBytes::from(parse_token_id_hex(
        tid_str,
        &format!("AXE.tokenId for chain {hedera_chain_id}"),
    )?))
}

pub(super) fn parse_token_id_hex(value: &str, label: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(value.strip_prefix("0x").unwrap_or(value))
        .map_err(|error| eyre::eyre!("invalid {label} hex: {error}"))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| eyre::eyre!("{label} must be 32 bytes (got {})", bytes.len()))
}

pub(crate) fn load_sui_main_wallet() -> Result<SuiWallet> {
    let key = env::var("SUI_PRIVATE_KEY").map_err(|_| {
        eyre::eyre!(
            "SUI_PRIVATE_KEY required (a `suiprivkey1...` bech32 secret from `sui keytool` or 64-char hex). Add it to .env."
        )
    })?;
    SuiWallet::from_secret_str(&key)
}
#[derive(Clone, Copy)]
enum SuiChannel {
    Gmp,
    Its,
}

async fn read_sui_channel(config: &Path, chain_id: &str, channel: SuiChannel) -> Result<String> {
    let config = ChainsConfig::load(config).await?;
    let objects = config
        .chain(chain_id)?
        .contract(ChainContract::Example, chain_id)?
        .objects
        .as_ref()
        .ok_or_else(|| eyre::eyre!("Example.objects missing for Sui chain {chain_id}"))?;
    let (label, value) = match channel {
        SuiChannel::Gmp => ("GmpChannelId", objects.gmp_channel_id.as_ref()),
        SuiChannel::Its => ("ItsChannelId", objects.its_channel_id.as_ref()),
    };
    value
        .cloned()
        .ok_or_else(|| eyre::eyre!("Example.objects.{label} missing for Sui chain {chain_id}"))
}

/// Read `(sui_channel_id, sui_rpc)` from the chains config. `rpc_override`
/// lets the caller honor `--destination-rpc` / `DESTINATION_RPC` from
/// `LoadTestArgs::destination_rpc`. An empty/None override falls back to
/// the chain config's `rpc` field.
pub(crate) async fn sui_dest_lookup(
    config: &Path,
    sui_chain_id: &str,
    rpc_override: Option<&str>,
) -> Result<(String, String)> {
    let channel = read_sui_channel(config, sui_chain_id, SuiChannel::Gmp).await?;
    let (config_rpc, _contracts) = read_sui_chain_config(config, sui_chain_id).await?;
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
pub(crate) async fn sui_its_dest_lookup(
    config: &Path,
    sui_chain_id: &str,
    rpc_override: Option<&str>,
) -> Result<(String, String)> {
    let channel = read_sui_channel(config, sui_chain_id, SuiChannel::Its).await?;
    let (config_rpc, _contracts) = read_sui_chain_config(config, sui_chain_id).await?;
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
pub(crate) async fn read_sui_axe_token_id(
    config: &Path,
    sui_chain_id: &str,
    cli_override: Option<&str>,
) -> Result<[u8; 32]> {
    let hex_str = if let Some(s) = cli_override.filter(|s| !s.is_empty()) {
        s.to_string()
    } else {
        let config = ChainsConfig::load(config).await?;
        config
            .chain(sui_chain_id)?
            .contract(ChainContract::Axe, sui_chain_id)?
            .objects
            .as_ref()
            .and_then(|objects| objects.token_id.clone())
            .ok_or_else(|| {
                eyre::eyre!("contracts.AXE.objects.TokenId missing for Sui chain {sui_chain_id}")
            })?
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
    report: &mut LoadTestReport,
    sui_channel: &str,
    sui_rpc: &str,
    source_type: verify::SourceChainType,
    test_start: Instant,
) -> Result<()> {
    super::gmp_verification::finish_batch(
        args,
        super::gmp_verification::SuiGmpTarget {
            address: sui_channel.to_string(),
            rpc_url: sui_rpc.to_string(),
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
    report: &mut LoadTestReport,
    sui_rpc: &str,
    test_start: Instant,
) -> Result<()> {
    super::its_verification::finish_batch(
        args,
        super::its_verification::SuiItsTarget {
            rpc_url: sui_rpc.to_string(),
        },
        report,
        test_start,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::parse_token_id_hex;

    #[test]
    fn token_id_parser_accepts_prefixed_and_unprefixed_32_byte_hex() {
        let encoded = "2a".repeat(32);

        assert_eq!(parse_token_id_hex(&encoded, "token").unwrap(), [0x2a; 32]);
        assert_eq!(
            parse_token_id_hex(&format!("0x{encoded}"), "token").unwrap(),
            [0x2a; 32]
        );
    }

    #[test]
    fn token_id_parser_rejects_malformed_and_wrong_length_values() {
        assert!(
            parse_token_id_hex("not-hex", "AXE token id")
                .unwrap_err()
                .to_string()
                .contains("invalid AXE token id hex")
        );
        assert!(
            parse_token_id_hex("00", "AXE token id")
                .unwrap_err()
                .to_string()
                .contains("must be 32 bytes")
        );
    }
}
