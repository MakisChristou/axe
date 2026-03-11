use std::path::Path;
use std::time::{Duration, Instant};

use alloy::primitives::{keccak256, Address, FixedBytes};
use alloy::providers::Provider;
use eyre::Result;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;

use super::metrics::{AmplifierTiming, FailureCategory, TxMetrics, VerificationReport};
use crate::cosmos::{lcd_cosmwasm_smart_query, read_axelar_config, read_axelar_contract_field};
use crate::evm::AxelarAmplifierGateway;
use crate::ui;

/// Maximum total time for all verification phases (3 minutes).
const VERIFY_TIMEOUT: Duration = Duration::from_secs(180);
/// Delay between poll attempts.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Per-tx state tracked during batch verification.
struct PendingTx {
    idx: usize,
    message_id: String,
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

    // Read Cosmos config
    let (lcd, _, _, _) = read_axelar_config(config)?;

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

    let gw_contract = AxelarAmplifierGateway::new(gateway_addr, provider);
    let contract_addr: Address = destination_address.parse()?;

    let deadline = Instant::now() + VERIFY_TIMEOUT;

    // Build pending tx list
    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            PendingTx {
                idx,
                message_id: format!("{}-1.1", tx.signature),
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
            deadline,
            "axelar",
        )
        .await;
    } else {
        ui::info("VotingVerifier not in config, skipping voted step");
    }

    // === Step 2/4: Routed (Cosmos Gateway) ===
    batch_poll_routed(&mut txs, &lcd, &cosm_gateway, source_chain, deadline, "axelar").await;

    // === Step 3/4: Approved (EVM gateway) ===
    batch_poll_approved(&mut txs, &gw_contract, source_chain, destination_chain, deadline).await;

    // === Step 4/4: Executed (approval consumed) ===
    batch_poll_executed(&mut txs, &gw_contract, source_chain, deadline, destination_chain).await;

    // Write timings + compute stats
    let report = compute_verification_report(&txs, metrics);

    Ok(report)
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
    deadline: Instant,
    annotation: &str,
) {
    let pending: Vec<usize> = (0..txs.len()).filter(|&i| !txs[i].failed).collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be voted (0/{pending_count} done)..."
    ));
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
    // Mark timed-out txs as failed
    for &i in &pending {
        if txs[i].timing.voted_secs.is_none() && !txs[i].failed {
            txs[i].failed = true;
            txs[i].fail_reason = Some("VotingVerifier: timed out".into());
        }
    }

    let timed_out = pending_count - done_count;
    spinner.finish_and_clear();
    if timed_out > 0 {
        ui::success_annotated(&format!("voted {done_count}/{pending_count} ({timed_out} timed out)"), annotation);
    } else {
        ui::success_annotated(&format!("voted {done_count}/{pending_count}"), annotation);
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
    deadline: Instant,
    annotation: &str,
) {
    let pending: Vec<usize> = (0..txs.len()).filter(|&i| !txs[i].failed).collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be routed (0/{pending_count} done)..."
    ));
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
    // Mark timed-out txs as failed
    for &i in &pending {
        if txs[i].timing.routed_secs.is_none() && !txs[i].failed {
            txs[i].failed = true;
            txs[i].fail_reason = Some("cosmos routing: timed out".into());
        }
    }

    let timed_out = pending_count - done_count;
    spinner.finish_and_clear();
    if timed_out > 0 {
        ui::success_annotated(&format!("routed {done_count}/{pending_count} ({timed_out} timed out)"), annotation);
    } else {
        ui::success_annotated(&format!("routed {done_count}/{pending_count}"), annotation);
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
    deadline: Instant,
) {
    let pending: Vec<usize> = (0..txs.len()).filter(|&i| !txs[i].failed).collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be approved on {destination_chain} (0/{pending_count} done)..."
    ));
    let mut done_count = 0usize;

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
    // Mark timed-out txs as failed
    for &i in &pending {
        if txs[i].timing.approved_secs.is_none() && !txs[i].failed {
            txs[i].failed = true;
            txs[i].fail_reason = Some("EVM approval: timed out".into());
        }
    }

    let timed_out = pending_count - done_count;
    spinner.finish_and_clear();
    if timed_out > 0 {
        ui::success_annotated(&format!("approved {done_count}/{pending_count} ({timed_out} timed out)"), destination_chain);
    } else {
        ui::success_annotated(&format!("approved {done_count}/{pending_count}"), destination_chain);
    }
}

// ---------------------------------------------------------------------------
// Step 4: Executed — approval consumed
// ---------------------------------------------------------------------------

async fn batch_poll_executed<P: Provider>(
    txs: &mut [PendingTx],
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    source_chain: &str,
    deadline: Instant,
    annotation: &str,
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
    // Mark timed-out txs as failed
    for &i in &pending {
        if txs[i].timing.executed_secs.is_none() && !txs[i].failed {
            txs[i].failed = true;
            txs[i].timing.executed_ok = Some(false);
            txs[i].fail_reason = Some("EVM execution: timed out".into());
        }
    }

    let timed_out = pending_count - done_count;
    spinner.finish_and_clear();
    if timed_out > 0 {
        ui::success_annotated(&format!("executed {done_count}/{pending_count} ({timed_out} timed out)"), annotation);
    } else {
        ui::success_annotated(&format!("executed {done_count}/{pending_count}"), annotation);
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
// Shared report computation
// ---------------------------------------------------------------------------

/// Compute the `VerificationReport` from pending tx results, writing timings
/// back into the original metrics array.
#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
fn compute_verification_report(
    txs: &[PendingTx],
    metrics: &mut [TxMetrics],
) -> VerificationReport {
    let mut successful = 0u64;
    let mut failed = 0u64;
    let mut failure_reasons: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    let mut stuck_count = 0u64;
    let mut stuck_phases: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();

    for tx in txs {
        metrics[tx.idx].amplifier_timing = Some(tx.timing.clone());
        if tx.failed {
            failed += 1;
            if let Some(ref reason) = tx.fail_reason {
                *failure_reasons.entry(reason.clone()).or_insert(0) += 1;

                // Categorize stuck txs by the phase they got stuck at
                if reason.contains("timed out") {
                    stuck_count += 1;
                    let phase = stuck_phase(tx);
                    *stuck_phases.entry(phase).or_insert(0) += 1;
                }
            }
        } else if tx.timing.executed_ok == Some(true) {
            successful += 1;
        }
    }

    let total_verified = successful + failed;
    let success_rate = if total_verified > 0 {
        successful as f64 / total_verified as f64
    } else {
        0.0
    };

    let failure_categories: Vec<FailureCategory> = failure_reasons
        .into_iter()
        .map(|(reason, count)| FailureCategory { reason, count })
        .collect();

    let stuck_at: Vec<FailureCategory> = stuck_phases
        .into_iter()
        .map(|(reason, count)| FailureCategory { reason, count })
        .collect();

    let all_timings: Vec<&AmplifierTiming> = txs.iter().map(|t| &t.timing).collect();
    let avg_voted = avg_option(all_timings.iter().filter_map(|t| t.voted_secs));
    let avg_routed = avg_option(all_timings.iter().filter_map(|t| t.routed_secs));
    let avg_approved = avg_option(all_timings.iter().filter_map(|t| t.approved_secs));
    let avg_executed = avg_option(all_timings.iter().filter_map(|t| t.executed_secs));
    let max_executed = max_option(all_timings.iter().filter_map(|t| t.executed_secs));

    VerificationReport {
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
        max_executed_secs: max_executed,
        stuck: stuck_count,
        stuck_at,
    }
}

/// Determine which phase a timed-out tx got stuck at (the last phase it didn't complete).
fn stuck_phase(tx: &PendingTx) -> String {
    if tx.timing.voted_secs.is_none() {
        "voted".into()
    } else if tx.timing.routed_secs.is_none() {
        "routed".into()
    } else if tx.timing.approved_secs.is_none() {
        "approved".into()
    } else {
        "executed".into()
    }
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

#[allow(clippy::float_arithmetic)]
fn avg_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    let vals: Vec<f64> = iter.collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }
}

fn max_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.reduce(f64::max)
}

// ===========================================================================
// EVM -> Solana verification
// ===========================================================================

/// Verify EVM->Solana transactions through the Amplifier pipeline:
///
/// 1. **Voted** — VotingVerifier verification (source EVM chain)
/// 2. **Routed** — Cosmos Gateway outgoing_messages (dest Solana chain)
/// 3. **Approved** — Solana IncomingMessage PDA exists
/// 4. **Executed** — Solana IncomingMessage PDA status = executed
#[allow(clippy::too_many_arguments)]
pub async fn verify_onchain_solana(
    config: &Path,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    solana_rpc: &str,
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

    let deadline = Instant::now() + VERIFY_TIMEOUT;

    let (lcd, _, _, _) = read_axelar_config(config)?;

    let voting_verifier = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/VotingVerifier/{source_chain}/address"),
    )
    .ok();
    let cosm_gateway = read_axelar_contract_field(
        config,
        &format!("/axelar/contracts/Gateway/{destination_chain}/address"),
    )?;

    // For EVM->Sol, message_id is stored directly in TxMetrics.signature
    let mut txs: Vec<PendingTx> = confirmed
        .iter()
        .map(|&idx| {
            let tx = &metrics[idx];
            let payload_hash = parse_payload_hash(&tx.payload_hash).unwrap_or_default();
            let message_id = tx.signature.clone();

            PendingTx {
                idx,
                message_id,
                send_instant: tx.send_instant.unwrap_or_else(Instant::now),
                source_address: tx.source_address.clone(),
                contract_addr: Address::ZERO,
                payload_hash,
                payload_hash_hex: tx.payload_hash.clone(),
                timing: AmplifierTiming::default(),
                failed: false,
                fail_reason: None,
            }
        })
        .collect();

    // Precompute command_ids for Solana PDA lookups
    let command_ids: Vec<[u8; 32]> = confirmed
        .iter()
        .map(|&idx| {
            let message_id = &metrics[idx].signature;
            let input = [source_chain.as_bytes(), b"-", message_id.as_bytes()].concat();
            keccak256(&input).into()
        })
        .collect();

    // Step 1/4: Voted (reused)
    if let Some(ref vv) = voting_verifier {
        batch_poll_voted(
            &mut txs,
            &lcd,
            vv,
            source_chain,
            destination_chain,
            destination_address,
            deadline,
            "axelar",
        )
        .await;
    } else {
        ui::info("VotingVerifier not in config, skipping voted step");
    }

    // Step 2/4: Routed (reused)
    batch_poll_routed(&mut txs, &lcd, &cosm_gateway, source_chain, deadline, "axelar").await;

    // Step 3/4: Approved on Solana
    batch_poll_solana_approved(&mut txs, &command_ids, solana_rpc, destination_chain, deadline).await;

    // Step 4/4: Executed on Solana
    batch_poll_solana_executed(&mut txs, &command_ids, solana_rpc, destination_chain, deadline).await;

    // Compute stats (same as verify_onchain)
    let report = compute_verification_report(&txs, metrics);

    Ok(report)
}

// ---------------------------------------------------------------------------
// Step 3 (Solana): Approved — IncomingMessage PDA exists
// ---------------------------------------------------------------------------

async fn batch_poll_solana_approved(
    txs: &mut [PendingTx],
    command_ids: &[[u8; 32]],
    solana_rpc: &str,
    destination_chain: &str,
    deadline: Instant,
) {
    let pending: Vec<usize> = (0..txs.len()).filter(|&i| !txs[i].failed).collect();
    let pending_count = pending.len();
    if pending_count == 0 {
        return;
    }

    let spinner = ui::wait_spinner(&format!(
        "waiting to be approved on {destination_chain} (0/{pending_count} done)..."
    ));
    let mut done_count = 0usize;

    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    );

    loop {
        for &i in &pending {
            if txs[i].timing.approved_secs.is_some() || txs[i].failed {
                continue;
            }
            match check_solana_incoming_message(&rpc_client, &command_ids[i]) {
                Ok(Some(_status)) => {
                    // PDA exists = message approved (or already executed)
                    txs[i].timing.approved_secs =
                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                    done_count += 1;
                    spinner.set_message(format!(
                        "waiting to be approved on {destination_chain} ({done_count}/{pending_count} done)..."
                    ));
                }
                Ok(None) => {} // PDA doesn't exist yet
                Err(e) => {
                    spinner.set_message(format!("Solana PDA query error: {e}"));
                }
            }
        }

        if done_count >= pending_count || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    // Mark timed-out txs as failed
    for &i in &pending {
        if txs[i].timing.approved_secs.is_none() && !txs[i].failed {
            txs[i].failed = true;
            txs[i].fail_reason = Some("Solana approval: timed out".into());
        }
    }

    let timed_out = pending_count - done_count;
    spinner.finish_and_clear();
    if timed_out > 0 {
        ui::success_annotated(&format!("approved {done_count}/{pending_count} ({timed_out} timed out)"), destination_chain);
    } else {
        ui::success_annotated(&format!("approved {done_count}/{pending_count}"), destination_chain);
    }
}

// ---------------------------------------------------------------------------
// Step 4 (Solana): Executed — IncomingMessage PDA status = 1
// ---------------------------------------------------------------------------

async fn batch_poll_solana_executed(
    txs: &mut [PendingTx],
    command_ids: &[[u8; 32]],
    solana_rpc: &str,
    annotation: &str,
    deadline: Instant,
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
    let mut done_count = 0usize;

    let rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(
        solana_rpc,
        solana_commitment_config::CommitmentConfig::confirmed(),
    );

    loop {
        for &i in &pending {
            if txs[i].timing.executed_secs.is_some() || txs[i].failed {
                continue;
            }
            match check_solana_incoming_message(&rpc_client, &command_ids[i]) {
                Ok(Some(status)) if status != 0 => {
                    // status != 0 means executed (MessageStatus::is_executed)
                    txs[i].timing.executed_secs =
                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                    txs[i].timing.executed_ok = Some(true);
                    done_count += 1;
                    spinner.set_message(format!(
                        "waiting to be executed ({done_count}/{pending_count} done)..."
                    ));
                }
                Ok(Some(_)) => {} // status=0, still approved not executed
                Ok(None) => {}    // PDA doesn't exist (shouldn't happen after approval)
                Err(e) => {
                    spinner.set_message(format!("Solana PDA query error: {e}"));
                }
            }
        }

        if done_count >= pending_count || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    // Mark timed-out txs as failed
    for &i in &pending {
        if txs[i].timing.executed_secs.is_none() && !txs[i].failed {
            txs[i].failed = true;
            txs[i].timing.executed_ok = Some(false);
            txs[i].fail_reason = Some("Solana execution: timed out".into());
        }
    }

    let timed_out = pending_count - done_count;
    spinner.finish_and_clear();
    if timed_out > 0 {
        ui::success_annotated(&format!("executed {done_count}/{pending_count} ({timed_out} timed out)"), annotation);
    } else {
        ui::success_annotated(&format!("executed {done_count}/{pending_count}"), annotation);
    }
}

// ---------------------------------------------------------------------------
// Solana IncomingMessage PDA check
// ---------------------------------------------------------------------------

/// Incoming message account data offset for the status byte.
/// Layout: 8 (discriminator) + 1 (bump) + 1 (signing_pda_bump) + 3 (pad) = 13
const INCOMING_MESSAGE_STATUS_OFFSET: usize = 13;

/// Check the Solana IncomingMessage PDA for a given command_id.
/// Returns `Some(status_byte)` if the account exists, `None` otherwise.
/// Status: 0 = approved, non-zero = executed.
fn check_solana_incoming_message(
    rpc_client: &solana_client::rpc_client::RpcClient,
    command_id: &[u8; 32],
) -> Result<Option<u8>> {
    let (pda, _bump) = Pubkey::find_program_address(
        &[b"incoming message", command_id],
        &solana_axelar_gateway::id(),
    );

    match rpc_client.get_account_data(&pda) {
        Ok(data) => {
            if data.len() <= INCOMING_MESSAGE_STATUS_OFFSET {
                return Err(eyre::eyre!(
                    "IncomingMessage account too small: {} bytes",
                    data.len()
                ));
            }
            Ok(Some(data[INCOMING_MESSAGE_STATUS_OFFSET]))
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("AccountNotFound")
                || err_str.contains("could not find account")
            {
                Ok(None)
            } else {
                Err(eyre::eyre!("Solana RPC error: {e}"))
            }
        }
    }
}
