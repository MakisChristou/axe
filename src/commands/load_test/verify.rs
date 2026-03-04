use std::path::Path;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use eyre::Result;
use serde_json::json;

use super::metrics::{AmplifierTiming, FailureCategory, TxMetrics, VerificationReport};
use crate::cosmos::{lcd_cosmwasm_smart_query, read_axelar_config, read_axelar_contract_field};
use crate::evm::AxelarAmplifierGateway;
use crate::ui;

/// Maximum time to wait for each checkpoint (10 minutes).
const POLL_TIMEOUT: Duration = Duration::from_secs(600);
/// Delay between poll attempts.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Per-tx state tracked during batch verification.
struct PendingTx {
    idx: usize,
    message_id: String,
    sig_short: String,
    send_instant: Instant,
    source_address: String,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
    payload_hash_hex: String,
    timing: AmplifierTiming,
    failed: bool,
    fail_reason: Option<String>,
}

/// Verify transactions on-chain through 4 Amplifier pipeline checkpoints:
///
/// 1. **Voted** — VotingVerifier verification (source chain)
/// 2. **Routed** — Destination Gateway outgoing_messages
/// 3. **Approved** — EVM gateway isMessageApproved
/// 4. **Executed** — EVM approval consumed
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain<P: Provider>(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    gateway_addr: Address,
    provider: &P,
    metrics: &mut [TxMetrics],
) -> Result<VerificationReport> {
    let confirmed: Vec<usize> = metrics
        .iter()
        .enumerate()
        .filter(|(_, m)| m.success && !m.signature.is_empty())
        .map(|(i, _)| i)
        .collect();

    let total = confirmed.len();
    if total == 0 {
        ui::warn("no confirmed transactions to verify");
        return Ok(VerificationReport::default());
    }

    ui::kv("transactions to verify", &total.to_string());

    // Read Cosmos config
    let (lcd, _, _, _) = read_axelar_config(config)?;
    ui::kv("cosmos LCD", &lcd);

    // Contract addresses (VotingVerifier is optional)
    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    if let Some(ref vv) = voting_verifier {
        ui::address(&format!("voting verifier ({source_chain})"), vv);
    }
    ui::address(
        &format!("cosmos gateway ({destination_chain})"),
        &cosm_gateway,
    );

    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, provider);
    let contract_addr: Address = destination_address.parse()?;

    // Build pending tx list
    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            PendingTx {
                idx,
                message_id: format!("{}-1.1", tx.signature),
                sig_short: truncate_sig(&tx.signature),
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
            }
        })
        .collect();

    // === Step 1/4: Voted (VotingVerifier) ===
    if let Some(ref vv) = voting_verifier {
        batch_poll_voted(
            &mut txs,
            &lcd,
            vv,
            source_chain,
            destination_chain,
            destination_address,
        )
        .await;
    } else {
        ui::info("VotingVerifier not in config, skipping voted step");
    }

    // === Step 2/4: Routed (Cosmos Gateway) ===
    batch_poll_routed(&mut txs, &lcd, &cosm_gateway, source_chain).await;

    // === Step 3/4: Approved (EVM gateway) ===
    batch_poll_approved(&mut txs, &gw_contract, source_chain, destination_chain).await;

    // === Step 4/4: Executed (approval consumed) ===
    batch_poll_executed(&mut txs, &gw_contract, source_chain).await;

    // Write timings + compute stats
    let mut successful = 0u64;
    let mut failed = 0u64;
    let mut failure_reasons: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();

    for tx in &txs {
        metrics[tx.idx].amplifier_timing = Some(tx.timing.clone());
        if tx.failed {
            failed += 1;
            if let Some(ref reason) = tx.fail_reason {
                *failure_reasons.entry(reason.clone()).or_insert(0) += 1;
            }
        } else if tx.timing.executed_ok == Some(true) {
            successful += 1;
        }
    }

    let total_verified = successful + failed;
    #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
    let success_rate = if total_verified > 0 {
        successful as f64 / total_verified as f64
    } else {
        0.0
    };

    let failure_categories: Vec<FailureCategory> = failure_reasons
        .into_iter()
        .map(|(reason, count)| FailureCategory { reason, count })
        .collect();

    let all_timings: Vec<&AmplifierTiming> = txs.iter().map(|t| &t.timing).collect();
    let avg_voted = avg_option(all_timings.iter().filter_map(|t| t.voted_secs));
    let avg_routed = avg_option(all_timings.iter().filter_map(|t| t.routed_secs));
    let avg_approved = avg_option(all_timings.iter().filter_map(|t| t.approved_secs));
    let avg_executed = avg_option(all_timings.iter().filter_map(|t| t.executed_secs));

    println!();
    ui::kv("successful", &successful.to_string());
    ui::kv("failed", &failed.to_string());
    ui::kv(
        "success rate",
        &format!("{:.1}%", success_rate * 100.0),
    );
    if let Some(v) = avg_voted {
        ui::kv("avg voted", &format!("{v:.1}s"));
    }
    if let Some(v) = avg_routed {
        ui::kv("avg routed", &format!("{v:.1}s"));
    }
    if let Some(v) = avg_approved {
        ui::kv("avg approved", &format!("{v:.1}s"));
    }
    if let Some(v) = avg_executed {
        ui::kv("avg executed", &format!("{v:.1}s"));
    }

    Ok(VerificationReport {
        total_verified,
        successful,
        pending: 0,
        failed,
        success_rate,
        failure_reasons: failure_categories,
        avg_voted_secs: avg_voted,
        avg_routed_secs: avg_routed,
        avg_approved_secs: avg_approved,
        avg_executed_secs: avg_executed,
    })
}

// ---------------------------------------------------------------------------
// Step 1: Voted — VotingVerifier verification_statuses
// ---------------------------------------------------------------------------

async fn batch_poll_voted(
    txs: &mut [PendingTx],
    lcd: &str,
    voting_verifier: &str,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
) {
    let pending: Vec<usize> = (0..txs.len()).filter(|&i| !txs[i].failed).collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be voted (0/{pending_count} done)..."
    ));
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut done_count = 0usize;

    loop {
        for &i in &pending {
            if txs[i].timing.voted_secs.is_some() || txs[i].failed {
                continue;
            }
            match check_voting_verifier(
                lcd,
                voting_verifier,
                source_chain,
                &txs[i].message_id,
                &txs[i].source_address,
                destination_chain,
                destination_address,
                &txs[i].payload_hash_hex,
            )
            .await
            {
                Ok(true) => {
                    txs[i].timing.voted_secs =
                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                    done_count += 1;
                    spinner.set_message(format!(
                        "waiting to be voted ({done_count}/{pending_count} done)..."
                    ));
                }
                Ok(false) => {}
                Err(e) => {
                    spinner.set_message(format!("VotingVerifier query error: {e}"));
                }
            }
        }

        if done_count >= pending_count || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    spinner.finish_and_clear();

    println!();
    for &i in &pending {
        if let Some(secs) = txs[i].timing.voted_secs {
            println!(
                "    {}  T + {secs:>6.1}s  \u{2714} voted",
                txs[i].sig_short
            );
        } else {
            let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
            println!(
                "    {}  T + {elapsed:>6.1}s  \u{2718} voted (timed out)",
                txs[i].sig_short
            );
            txs[i].failed = true;
            txs[i].fail_reason = Some("VotingVerifier: timed out".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Step 2: Routed — Cosmos Gateway outgoing_messages
// ---------------------------------------------------------------------------

async fn batch_poll_routed(
    txs: &mut [PendingTx],
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
) {
    let pending: Vec<usize> = (0..txs.len()).filter(|&i| !txs[i].failed).collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be routed (0/{pending_count} done)..."
    ));
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut done_count = 0usize;

    loop {
        for &i in &pending {
            if txs[i].timing.routed_secs.is_some() || txs[i].failed {
                continue;
            }
            match check_cosmos_routed(lcd, cosm_gateway, source_chain, &txs[i].message_id).await {
                Ok(true) => {
                    txs[i].timing.routed_secs =
                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                    done_count += 1;
                    spinner.set_message(format!(
                        "waiting to be routed ({done_count}/{pending_count} done)..."
                    ));
                }
                Ok(false) => {}
                Err(e) => {
                    spinner.set_message(format!("Gateway query error: {e}"));
                }
            }
        }

        if done_count >= pending_count || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    spinner.finish_and_clear();

    println!();
    for &i in &pending {
        if let Some(secs) = txs[i].timing.routed_secs {
            println!(
                "    {}  T + {secs:>6.1}s  \u{2714} routed",
                txs[i].sig_short
            );
        } else {
            let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
            println!(
                "    {}  T + {elapsed:>6.1}s  \u{2718} routed (timed out)",
                txs[i].sig_short
            );
            txs[i].failed = true;
            txs[i].fail_reason = Some("cosmos routing: timed out".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Step 3: Approved — EVM isMessageApproved
// ---------------------------------------------------------------------------

async fn batch_poll_approved<P: Provider>(
    txs: &mut [PendingTx],
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
    destination_chain: &str,
) {
    let pending: Vec<usize> = (0..txs.len()).filter(|&i| !txs[i].failed).collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be approved on {destination_chain} (0/{pending_count} done)..."
    ));
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut done_count = 0usize;

    // Print params for first tx so we can verify correctness
    if let Some(&i) = pending.first() {
        ui::info(&format!(
            "isMessageApproved params: sourceChain={}, messageId={}, sourceAddress={}, contractAddress={:?}, payloadHash=0x{}",
            source_chain,
            txs[i].message_id,
            txs[i].source_address,
            txs[i].contract_addr,
            alloy::hex::encode(txs[i].payload_hash),
        ));
    }

    loop {
        for &i in &pending {
            if txs[i].timing.approved_secs.is_some() || txs[i].failed {
                continue;
            }
            match check_evm_is_message_approved(
                gw_contract,
                source_chain,
                &txs[i].message_id,
                &txs[i].source_address,
                txs[i].contract_addr,
                txs[i].payload_hash,
            )
            .await
            {
                Ok(true) => {
                    txs[i].timing.approved_secs =
                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                    done_count += 1;
                    spinner.set_message(format!(
                        "waiting to be approved on {destination_chain} ({done_count}/{pending_count} done)..."
                    ));
                }
                Ok(false) => {}
                Err(e) => {
                    spinner.set_message(format!("EVM isMessageApproved error: {e}"));
                }
            }
        }

        if done_count >= pending_count || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    spinner.finish_and_clear();

    println!();
    for &i in &pending {
        if let Some(secs) = txs[i].timing.approved_secs {
            println!(
                "    {}  T + {secs:>6.1}s  \u{2714} approved",
                txs[i].sig_short
            );
        } else {
            let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
            println!(
                "    {}  T + {elapsed:>6.1}s  \u{2718} approved (timed out)",
                txs[i].sig_short
            );
            txs[i].failed = true;
            txs[i].fail_reason = Some("EVM approval: timed out".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Step 4: Executed — approval consumed
// ---------------------------------------------------------------------------

async fn batch_poll_executed<P: Provider>(
    txs: &mut [PendingTx],
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
) {
    let pending: Vec<usize> = (0..txs.len())
        .filter(|&i| !txs[i].failed && txs[i].timing.executed_ok.is_none())
        .collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be executed (0/{pending_count} done)..."
    ));
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut done_count = 0usize;

    loop {
        for &i in &pending {
            if txs[i].timing.executed_secs.is_some() || txs[i].failed {
                continue;
            }
            match check_evm_is_message_approved(
                gw_contract,
                source_chain,
                &txs[i].message_id,
                &txs[i].source_address,
                txs[i].contract_addr,
                txs[i].payload_hash,
            )
            .await
            {
                Ok(false) => {
                    txs[i].timing.executed_secs =
                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                    txs[i].timing.executed_ok = Some(true);
                    done_count += 1;
                    spinner.set_message(format!(
                        "waiting to be executed ({done_count}/{pending_count} done)..."
                    ));
                }
                Ok(true) => {} // still approved, not yet executed
                Err(e) => {
                    spinner.set_message(format!("EVM isMessageApproved error: {e}"));
                }
            }
        }

        if done_count >= pending_count || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    spinner.finish_and_clear();

    println!();
    for &i in &pending {
        if let Some(secs) = txs[i].timing.executed_secs {
            println!(
                "    {}  T + {secs:>6.1}s  \u{2714} executed",
                txs[i].sig_short
            );
        } else {
            let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
            println!(
                "    {}  T + {elapsed:>6.1}s  \u{2718} executed (timed out)",
                txs[i].sig_short
            );
            txs[i].failed = true;
            txs[i].timing.executed_ok = Some(false);
            txs[i].fail_reason = Some("EVM execution: timed out".into());
        }
    }
}

// ---------------------------------------------------------------------------
// Single-shot check helpers
// ---------------------------------------------------------------------------

/// Check VotingVerifier `messages_status` for a message.
/// Returns true if status contains "succeeded" (quorum reached).
#[allow(clippy::too_many_arguments)]
async fn check_voting_verifier(
    lcd: &str,
    voting_verifier: &str,
    source_chain: &str,
    message_id: &str,
    source_address: &str,
    destination_chain: &str,
    destination_address: &str,
    payload_hash_hex: &str,
) -> Result<bool> {
    let query = json!({
        "messages_status": [{
            "cc_id": {
                "source_chain": source_chain,
                "message_id": message_id,
            },
            "source_address": source_address,
            "destination_chain": destination_chain,
            "destination_address": destination_address,
            "payload_hash": payload_hash_hex,
        }]
    });

    let resp = lcd_cosmwasm_smart_query(lcd, voting_verifier, &query).await?;
    let resp_str = serde_json::to_string(&resp)?;
    // Look for "succeeded" in any casing — covers SucceededOnSourceChain,
    // succeeded_on_source_chain, etc.
    Ok(resp_str.to_lowercase().contains("succeeded"))
}

/// Check if message is routed on destination Cosmos Gateway via `outgoing_messages`.
async fn check_cosmos_routed(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = json!({
        "outgoing_messages": [{
            "source_chain": source_chain,
            "message_id": message_id,
        }]
    });

    let resp = lcd_cosmwasm_smart_query(lcd, cosm_gateway, &query).await?;
    let data = resp
        .get("data")
        .or_else(|| resp.as_array().map(|_| &resp));
    Ok(match data {
        Some(arr) if arr.is_array() => {
            let items = arr.as_array().unwrap();
            !items.is_empty() && !items.iter().all(|v| v.is_null())
        }
        _ => false,
    })
}

/// Check `isMessageApproved` on the EVM gateway (single attempt).
async fn check_evm_is_message_approved<P: Provider>(
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
    message_id: &str,
    source_address: &str,
    contract_addr: Address,
    payload_hash: FixedBytes<32>,
) -> Result<bool> {
    let approved = gw_contract
        .isMessageApproved(
            source_chain.to_string(),
            message_id.to_string(),
            source_address.to_string(),
            contract_addr,
            payload_hash,
        )
        .call()
        .await?;
    Ok(approved)
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn parse_payload_hash(hex_str: &str) -> Result<FixedBytes<32>> {
    let bytes = alloy::hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?;
    if bytes.len() != 32 {
        return Err(eyre::eyre!(
            "payload_hash must be 32 bytes, got {}",
            bytes.len()
        ));
    }
    Ok(FixedBytes::from_slice(&bytes))
}

fn truncate_sig(sig: &str) -> String {
    if sig.len() > 24 {
        format!("{}..{}", &sig[..16], &sig[sig.len() - 8..])
    } else {
        sig.to_string()
    }
}

#[allow(clippy::float_arithmetic)]
fn avg_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    let vals: Vec<f64> = iter.collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }
}
