//! Polling pipeline machinery shared by every `verify_onchain_*` orchestrator
//! in `mod.rs`: the [`DestinationChecker`] / [`ItsHubDest`] enums that abstract
//! over destination-chain RPCs, the three [`poll_pipeline`],
//! [`poll_pipeline_its_hub`], [`poll_pipeline_its_hub_evm`] state machines that
//! drive each tx through Voted → Routed → Approved → Executed, and the
//! cosmos/EVM/Solana check helpers they call into.
//!
//! The orchestrators in `mod.rs` build [`PendingTx`] vectors, hand them to one
//! of the `poll_pipeline*` functions, and turn the resulting [`PeakThroughput`]
//! plus the populated `tx.timing` into a `VerificationReport` via
//! [`super::report::compute_verification_report`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use alloy::primitives::{Address, FixedBytes, keccak256};
use alloy::providers::Provider;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use super::PendingTx;
use super::checks::{
    batch_check_solana_incoming_messages, check_evm_command_executed,
    check_evm_is_message_approved, check_evm_is_message_executed,
};
use super::legacy;
use super::report::compute_peak_throughput;
use super::state::{Phase, RealTimeStats, phase_counts};
use super::{INACTIVITY_TIMEOUT, POLL_INTERVAL};
use crate::commands::load_test::metrics::PeakThroughput;
use crate::cosmos::{discover_second_leg, lcd_cosmwasm_smart_query};
use crate::evm::AxelarAmplifierGateway;
use crate::ui;

/// Parse a hex-encoded 32-byte payload hash, with or without the `0x`
/// prefix. Returns an error rather than silently zero-extending so a
/// truncated hash from upstream code surfaces immediately instead of
/// propagating into a downstream "wrong gateway hash" mismatch.
pub(super) fn parse_payload_hash(hex_str: &str) -> Result<FixedBytes<32>> {
    let bytes = alloy::hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?;
    if bytes.len() != 32 {
        return Err(eyre::eyre!(
            "payload_hash must be 32 bytes, got {}",
            bytes.len()
        ));
    }
    Ok(FixedBytes::from_slice(&bytes))
}

fn required_payload_hash(tx: &PendingTx) -> Result<FixedBytes<32>> {
    tx.payload_hash
        .ok_or_else(|| eyre::eyre!("tx {} has no first-leg payload_hash", tx.message_id))
}

fn required_second_leg_field(
    tx: &PendingTx,
    field: &str,
    value: Option<&String>,
) -> Result<String> {
    value
        .cloned()
        .ok_or_else(|| eyre::eyre!("tx {} missing second-leg {field}", tx.message_id))
}

fn required_second_leg_payload_hash(tx: &PendingTx) -> Result<FixedBytes<32>> {
    let payload_hash =
        required_second_leg_field(tx, "payload_hash", tx.second_leg_payload_hash.as_ref())?;
    parse_payload_hash(&payload_hash)
        .wrap_err_with(|| format!("tx {} has invalid second-leg payload_hash", tx.message_id))
}

fn is_hub_not_approved_error(error: &eyre::Report) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("not approved") || message.contains("failed to query executable messages")
}

/// LCDs (notably qubelabs) sometimes return HTTP 500 with a body like
/// `"failed to query outgoing messages: message with ID … not found"` while
/// a freshly-confirmed source-side message is still being routed by the
/// verifier set. This is a normal pending state, not an outage — treat it
/// as "not yet routed, keep polling" rather than aborting the whole run.
fn is_message_not_yet_routed_error(error: &eyre::Report) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("failed to query outgoing messages")
        || message.contains("message with id") && message.contains("not found")
}

// ---------------------------------------------------------------------------
// Destination checker abstraction
// ---------------------------------------------------------------------------

pub(super) enum DestinationChecker<'a, P: Provider> {
    Evm {
        gw_contract: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
        /// When true, only conclude execution after the approval has actually
        /// been observed (`isMessageApproved == true`) at least once. The
        /// amplifier fast-path otherwise reads an unapproved result as
        /// "approved+executed between polls" — valid only once the message is
        /// known to be en route (it passed the `routed` phase). A
        /// consensus→amplifier route enters the Approved phase immediately, so
        /// it must observe the real approval or it would false-positive on the
        /// first poll.
        require_observed_approval: bool,
    },
    /// Legacy (consensus) EVM destination — verified via the old
    /// `AxelarGateway`: locate the emitted `ContractCallApproved` (authoritative
    /// `commandId`) then confirm `isCommandExecuted`. `from_block` bounds the
    /// approval-event scan to blocks produced since verification started.
    /// `match_by_payload` selects how the approval log is matched: an EVM source
    /// pins it with the exact `sourceTxHash` (precise); a non-EVM source has no
    /// EVM tx hash, so it matches on the (unique) `payloadHash` + dest address.
    EvmLegacy {
        gw_contract: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
        from_block: u64,
        match_by_payload: bool,
    },
    Solana {
        rpc_client: Arc<solana_client::rpc_client::RpcClient>,
        network: crate::types::Network,
        _phantom: std::marker::PhantomData<&'a P>,
    },
    Stellar {
        client: crate::stellar::StellarClient,
        gateway_contract: String,
        signer_pk: [u8; 32],
        _phantom: std::marker::PhantomData<&'a P>,
    },
    /// Sui destination — query AxelarGateway events `MessageApproved`
    /// (Approved phase) and `MessageExecuted` (Executed phase) by
    /// `(source_chain, message_id)`.
    Sui {
        client: crate::sui::SuiClient,
        gateway_pkg: String,
        _phantom: std::marker::PhantomData<&'a P>,
    },
}

impl<P: Provider> DestinationChecker<'_, P> {
    fn approval_label(&self) -> &str {
        match self {
            Self::Evm { .. } => "EVM approval",
            Self::EvmLegacy { .. } => "EVM(legacy) approval",
            Self::Solana { .. } => "Solana approval",
            Self::Stellar { .. } => "Stellar approval",
            Self::Sui { .. } => "Sui approval",
        }
    }

    fn execution_label(&self) -> &str {
        match self {
            Self::Evm { .. } => "EVM execution",
            Self::EvmLegacy { .. } => "EVM(legacy) execution",
            Self::Solana { .. } => "Solana execution",
            Self::Stellar { .. } => "Stellar execution",
            Self::Sui { .. } => "Sui execution",
        }
    }
}

// ---------------------------------------------------------------------------
// Final GMP-API recheck for timed-out txs
// ---------------------------------------------------------------------------

/// Bound each GMP-API recheck attempt so a slow or unreachable API can never
/// hang the end of a verification run.
const GMP_API_RECHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Recheck attempts. The safety net is only worth anything if a transient API
/// blip — or a message still transitioning `executing → executed` at the
/// inactivity timeout — doesn't get read as a permanent failure. A few spaced
/// attempts turn "one bad poll fails the route" into "the route is only failed
/// if it's genuinely not executed."
const GMP_API_RECHECK_ATTEMPTS: u32 = 3;
/// Gap between recheck attempts (gives an in-flight final leg time to land and
/// Axelarscan time to index it).
const GMP_API_RECHECK_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

/// Best-effort, non-fatal final check: ask the Axelarscan GMP API whether this
/// message actually executed on-chain. A slow final leg or a missed poll can
/// leave an executed transfer looking failed at the inactivity timeout (the
/// MOU-3/MOU-4 failure mode); this is the safety net layered on top of those
/// fixes. Retries a few times so a momentary API error/timeout — or a message
/// that finishes executing during the recheck — is still recovered; only a
/// consistently not-executed answer keeps the failed verdict.
async fn gmp_api_reports_executed(network: crate::types::Network, message_id: &str) -> bool {
    let base = crate::gmp_api::base_url(network);
    for attempt in 0..GMP_API_RECHECK_ATTEMPTS {
        let lookup = async {
            // Prefer the exact `message_id` Axelarscan keys on. If it isn't
            // indexed in that form for this source chain, fall back to the
            // source tx id (the portion before the event-index suffix).
            if let Some(rec) = crate::gmp_api::search_by_message_id(base, message_id).await? {
                return Ok::<bool, eyre::Report>(rec.is_executed());
            }
            if let Some((source_tx, _)) = message_id.rsplit_once('-')
                && let Some(rec) = crate::gmp_api::search_by_tx(base, source_tx).await?
            {
                return Ok(rec.is_executed());
            }
            Ok(false)
        };
        // Executed ⇒ done. Not-executed / not-found / API error ⇒ retry: the
        // final leg may still be landing, or the API may be momentarily flaky.
        if let Ok(Ok(true)) = tokio::time::timeout(GMP_API_RECHECK_TIMEOUT, lookup).await {
            return true;
        }
        if attempt + 1 < GMP_API_RECHECK_ATTEMPTS {
            tokio::time::sleep(GMP_API_RECHECK_BACKOFF).await;
        }
    }
    false
}

/// Apply the final verdict to a tx that didn't reach [`Phase::Done`] before the
/// inactivity timeout. `executed` is the authoritative GMP-API answer: when
/// true the tx is recovered as successful (its slow final leg actually landed);
/// otherwise it keeps the original `"{label}: timed out"` failed verdict.
fn mark_timed_out_tx(
    tx: &mut PendingTx,
    executed: bool,
    approval_label: &str,
    execution_label: &str,
) {
    if executed {
        tx.phase = Phase::Done;
        if tx.timing.executed_secs.is_none() {
            tx.timing.executed_secs = Some(tx.send_instant.elapsed().as_secs_f64());
        }
        tx.timing.executed_ok = Some(true);
        tx.failed = false;
        tx.recovered_via_api = true;
        return;
    }
    tx.failed = true;
    let label = match tx.phase {
        Phase::Voted => "VotingVerifier",
        Phase::Routed => "cosmos routing",
        Phase::HubApproved => "hub approval",
        Phase::DiscoverSecondLeg => "second-leg discovery",
        Phase::Approved => approval_label,
        Phase::Executed => execution_label,
        Phase::Done => unreachable!(),
    };
    if tx.phase == Phase::Executed {
        tx.timing.executed_ok = Some(false);
    }
    tx.fail_reason = Some(format!("{label}: timed out"));
}

/// Finalize the polling loop: for every tx that didn't reach `Done`, do a
/// best-effort GMP-API recheck and either recover it (executed on-chain) or
/// mark it timed-out failed. Sequential by design — this runs once at the end
/// of a run (single-test CI has `num_txs=1`), so concurrency is naturally
/// bounded to one in-flight recheck.
async fn finalize_timed_out_txs(
    txs: &mut [PendingTx],
    network: crate::types::Network,
    approval_label: &str,
    execution_label: &str,
) {
    for tx in txs.iter_mut() {
        if tx.failed || tx.phase == Phase::Done {
            continue;
        }
        let executed = gmp_api_reports_executed(network, &tx.message_id).await;
        mark_timed_out_tx(tx, executed, approval_label, execution_label);
    }
}

// ---------------------------------------------------------------------------
// Unified polling pipeline
// ---------------------------------------------------------------------------

pub(super) struct PollPipelineArgs {
    pub lcd: String,
    pub voting_verifier: Option<String>,
    pub cosm_gateway: Option<String>,
    pub source_chain: String,
    pub destination_chain: String,
    pub destination_address: String,
    pub axelarnet_gateway: Option<String>,
    pub display_chain: Option<String>,
    /// Network for the final Axelarscan GMP-API recheck on timed-out txs.
    pub network: crate::types::Network,
}

#[allow(clippy::cognitive_complexity)]
pub(super) async fn poll_pipeline<P: Provider>(
    txs: &mut Vec<PendingTx>,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    checker: &DestinationChecker<'_, P>,
    external_spinner: Option<indicatif::ProgressBar>,
    args: PollPipelineArgs,
) -> Result<PeakThroughput> {
    let PollPipelineArgs {
        lcd,
        voting_verifier,
        cosm_gateway,
        source_chain,
        destination_chain,
        destination_address,
        axelarnet_gateway,
        display_chain,
        network,
    } = args;
    let lcd = lcd.as_str();
    let voting_verifier = voting_verifier.as_deref();
    let cosm_gateway = cosm_gateway.as_deref();
    let source_chain = source_chain.as_str();
    let destination_chain = destination_chain.as_str();
    let destination_address = destination_address.as_str();
    let axelarnet_gateway = axelarnet_gateway.as_deref();
    let display_chain = display_chain.as_deref();
    let spinner =
        external_spinner.unwrap_or_else(|| ui::wait_spinner("verifying pipeline (starting)..."));
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();
    let mut received_first_tx = false;

    // For EVM destinations, derive the contract_addr from destination_address
    // so streaming PendingTx entries (which may have Address::ZERO) get the right value.
    let default_contract_addr = match checker {
        DestinationChecker::Evm { .. } | DestinationChecker::EvmLegacy { .. } => {
            Some(destination_address.parse()?)
        }
        _ => None,
    };

    loop {
        // Drain any newly-confirmed txs from the streaming channel.
        if let Some(ref mut receiver) = rx {
            while let Ok(mut new_tx) = receiver.try_recv() {
                if new_tx.contract_addr == Address::ZERO
                    && let Some(contract_addr) = default_contract_addr
                {
                    new_tx.contract_addr = contract_addr;
                }
                txs.push(new_tx);
            }
        }

        let sending_complete = send_done.is_none_or(|f| f.load(Ordering::Relaxed));

        let total = txs.len();
        if total == 0 {
            if sending_complete {
                break;
            }
            // Still sending — wait for txs to arrive.
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        if !received_first_tx {
            received_first_tx = true;
            spinner.set_message(format!("verifying pipeline: 0/{total} confirmed..."));
        }

        // Collect indices of non-terminal txs
        let active: Vec<usize> = (0..txs.len())
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() && sending_complete {
            break;
        }
        if active.is_empty() {
            // All current txs done but send still in progress — wait for more.
            tokio::time::sleep(POLL_INTERVAL).await;
            last_progress = Instant::now();
            continue;
        }

        // --- Phase-grouped batch polling (all phases in parallel) ---
        // Each cycle: collect ALL txs per phase, fire batch queries for all
        // phases concurrently. Within each phase, HTTP chunks of COSMOS_BATCH_SIZE
        // run concurrently too. This keeps poll cycles fast even at 600+ txs.
        let error_msg: Option<String> = None;

        // Snapshot indices by phase (no borrows held after this).
        let voted_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Voted)
            .copied()
            .collect();
        let routed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Routed)
            .copied()
            .collect();
        let hub_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::HubApproved)
            .copied()
            .collect();
        let approved_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Approved)
            .copied()
            .collect();
        let executed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Executed)
            .copied()
            .collect();

        // Build owned data for batch queries (avoids holding borrows on txs).
        let voted_data: Vec<(usize, String, String, String)> = voted_indices
            .iter()
            .map(|&i| {
                (
                    i,
                    txs[i].message_id.clone(),
                    txs[i].source_address.clone(),
                    txs[i].payload_hash_hex.clone(),
                )
            })
            .collect();
        let routed_data: Vec<(usize, String)> = routed_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let hub_data: Vec<(usize, String)> = hub_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let dest_indices: Vec<usize> = approved_indices
            .iter()
            .chain(executed_indices.iter())
            .copied()
            .collect();
        // The VotingVerifier records the message's *first-leg* destination,
        // which for ITS-hub-routed transfers is (axelar, AxelarnetGateway) —
        // NOT the final chain. Each tx captured that as gmp_destination_* from
        // its source-side ContractCall event; for raw GMP it equals the
        // function-arg destination. Prefer the per-tx value so hub-routed
        // Sui-destination ITS (e.g. sol→sui) queries (axelar, Hub) instead of
        // the final Sui channel — the latter returns status "unknown" forever
        // and the verify phase times out at "voted".
        let (vv_dest_chain, vv_dest_addr) = voted_indices
            .first()
            .map(|&i| {
                (
                    txs[i].gmp_destination_chain.clone(),
                    txs[i].gmp_destination_address.clone(),
                )
            })
            .filter(|(c, a)| !c.is_empty() && !a.is_empty())
            .unwrap_or_else(|| {
                (
                    destination_chain.to_string(),
                    destination_address.to_string(),
                )
            });
        // Fire Cosmos phases concurrently (each internally chunks into COSMOS_BATCH_SIZE).
        let (voted_results, routed_results, hub_results) = tokio::join!(
            // Voted
            async {
                if voted_data.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(vv) = voting_verifier {
                    batch_check_voting_verifier_owned(
                        lcd,
                        vv,
                        source_chain,
                        &vv_dest_chain,
                        &vv_dest_addr,
                        &voted_data,
                    )
                    .await
                } else {
                    // No VotingVerifier — all pass immediately
                    Ok(voted_data.iter().map(|(i, ..)| (*i, true)).collect())
                }
            },
            // Routed
            async {
                if routed_data.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(gw) = cosm_gateway {
                    batch_check_cosmos_routed_owned(lcd, gw, source_chain, &routed_data).await
                } else {
                    Ok(routed_data.iter().map(|(i, _)| (*i, true)).collect())
                }
            },
            // HubApproved
            async {
                if hub_data.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(gw) = axelarnet_gateway {
                    batch_check_hub_approved_owned(lcd, gw, source_chain, &hub_data).await
                } else {
                    Ok(hub_data.iter().map(|(i, _)| (*i, true)).collect())
                }
            },
        );
        let voted_results = voted_results?;
        let routed_results = routed_results?;
        let hub_results = hub_results?;

        // Apply Cosmos results
        for (i, ok) in voted_results {
            if ok {
                txs[i].timing.voted_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                // No destination Cosmos Gateway (legacy/consensus destination)
                // means there is no `routed` phase to observe — skip straight to
                // the destination approval check so timings stay honest.
                txs[i].phase = if cosm_gateway.is_some() {
                    Phase::Routed
                } else {
                    Phase::Approved
                };
                last_progress = Instant::now();
            }
        }
        for (i, ok) in routed_results {
            if ok {
                txs[i].timing.routed_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = if axelarnet_gateway.is_some() {
                    Phase::HubApproved
                } else {
                    Phase::Approved
                };
                last_progress = Instant::now();
            }
        }
        for (i, ok) in hub_results {
            if ok {
                txs[i].timing.hub_approved_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::Approved;
                last_progress = Instant::now();
            }
        }

        // Destination checks (Solana batch / EVM individual)
        if !dest_indices.is_empty() {
            match checker {
                DestinationChecker::Solana {
                    rpc_client,
                    network,
                    ..
                } => {
                    let client = rpc_client.clone();
                    let network = *network;
                    let data: Vec<(usize, [u8; 32])> = dest_indices
                        .iter()
                        .map(|&i| {
                            let command_id = txs[i].command_id.ok_or_else(|| {
                                eyre::eyre!(
                                    "tx {} missing Solana command_id for destination check",
                                    txs[i].message_id
                                )
                            })?;
                            Ok((i, command_id))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let results = tokio::task::spawn_blocking(move || {
                        batch_check_solana_incoming_messages(&client, network, &data)
                    })
                    .await
                    .wrap_err("Solana destination check task failed")??;

                    for (i, status) in results {
                        match (txs[i].phase, status) {
                            (Phase::Approved, Some(0)) => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            (Phase::Approved, Some(_)) => {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            (Phase::Executed, Some(s)) if s > 0 => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
                DestinationChecker::Evm {
                    gw_contract,
                    require_observed_approval,
                } => {
                    let mut futs = Vec::with_capacity(dest_indices.len());
                    for &i in &dest_indices {
                        let phase = txs[i].phase;
                        let msg_id = txs[i].message_id.clone();
                        let src_addr = txs[i].source_address.clone();
                        let c_addr = txs[i].contract_addr;
                        let p_hash = required_payload_hash(&txs[i])?;
                        futs.push(async move {
                            let approved = check_evm_is_message_approved(
                                gw_contract,
                                source_chain,
                                &msg_id,
                                &src_addr,
                                c_addr,
                                p_hash,
                            )
                            .await?;
                            // `isMessageApproved` flips back to false the moment
                            // the message executes, so a fast route can be
                            // approved+executed between two polls and never show
                            // `approved == true`. `isMessageExecuted` stays true,
                            // so it catches that race regardless of source type.
                            let executed =
                                check_evm_is_message_executed(gw_contract, source_chain, &msg_id)
                                    .await?;
                            Ok((i, phase, approved, executed))
                        });
                    }
                    let results: Vec<Result<_>> = futures::stream::iter(futs)
                        .buffer_unordered(20)
                        .collect()
                        .await;
                    for result in results {
                        // A persistent destination-RPC error (transient retries
                        // already exhausted in `check_evm_*`) must NOT abort the
                        // whole run — the message may well have executed on-chain
                        // (e.g. a flaky/archive-gated dest RPC). Skip this tx for
                        // this cycle and keep polling; `finalize_timed_out_txs`
                        // does an authoritative GMP-API recheck at the inactivity
                        // timeout and recovers it if it actually executed.
                        let (i, phase, approved, executed) = match result {
                            Ok(v) => v,
                            Err(e) => {
                                ui::warn(&format!(
                                    "destination check RPC error (keeping in-flight for \
                                     GMP-API recheck): {e}"
                                ));
                                continue;
                            }
                        };
                        match phase {
                            // Authoritative fast-path: the message is already
                            // executed on the destination gateway. Catches the
                            // approve→execute-between-polls race that left fast
                            // amplifier routes (monad-3, hyperliquid) stuck in
                            // Approved until the inactivity timeout. Safe for the
                            // consensus-source case too: `isMessageExecuted` is
                            // false until the message genuinely lands, so it
                            // cannot conclude success prematurely.
                            Phase::Approved if executed => {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            Phase::Approved if approved => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            // Unapproved while in the Approved phase. The
                            // amplifier fast-path treats this as
                            // approved+executed-between-polls; a route that has
                            // not yet observed a real approval (consensus
                            // source) must keep waiting instead, or it would
                            // conclude success before the message even lands.
                            Phase::Approved if !require_observed_approval => {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            Phase::Executed if !approved => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
                DestinationChecker::EvmLegacy {
                    gw_contract,
                    from_block,
                    match_by_payload,
                } => {
                    // Sequential per-tx (one eth_getLogs each); fine for the
                    // burst sizes legacy GMP runs at today.
                    for &i in &dest_indices {
                        match txs[i].phase {
                            Phase::Approved => {
                                let payload_hash = required_payload_hash(&txs[i])?;
                                // EVM source: pin the approval log with the exact
                                // sourceTxHash. Non-EVM source: no EVM tx hash, so
                                // match on the (unique) payloadHash + dest address.
                                let found = if *match_by_payload {
                                    legacy::find_contract_call_approved_by_payload(
                                        gw_contract.provider(),
                                        *gw_contract.address(),
                                        txs[i].contract_addr,
                                        payload_hash,
                                        *from_block,
                                    )
                                    .await?
                                } else {
                                    let src_tx_hash =
                                        legacy::source_tx_hash_from_message_id(&txs[i].message_id)?;
                                    legacy::find_contract_call_approved(
                                        gw_contract.provider(),
                                        *gw_contract.address(),
                                        txs[i].contract_addr,
                                        payload_hash,
                                        src_tx_hash,
                                        *from_block,
                                    )
                                    .await?
                                };
                                let Some(cmd_id) = found else { continue };
                                txs[i].command_id = Some(cmd_id);
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                // Execution may already have landed in the same
                                // window — check now to avoid a spurious extra poll.
                                let executed =
                                    check_evm_command_executed(gw_contract, cmd_id.into()).await?;
                                if executed {
                                    txs[i].timing.executed_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                } else {
                                    txs[i].phase = Phase::Executed;
                                }
                                last_progress = Instant::now();
                            }
                            Phase::Executed => {
                                let cmd_id = txs[i].command_id.ok_or_else(|| {
                                    eyre::eyre!(
                                        "legacy tx {} in Executed phase without a commandId",
                                        txs[i].message_id
                                    )
                                })?;
                                if check_evm_command_executed(gw_contract, cmd_id.into()).await? {
                                    txs[i].timing.executed_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                DestinationChecker::Stellar {
                    client,
                    gateway_contract,
                    signer_pk,
                    ..
                } => {
                    for &i in &dest_indices {
                        let phase = txs[i].phase;
                        let approved = client
                            .gateway_is_message_approved(
                                signer_pk,
                                gateway_contract,
                                source_chain,
                                &txs[i].message_id,
                                &txs[i].source_address,
                                &txs[i].gmp_destination_address,
                                required_payload_hash(&txs[i])?.0,
                            )
                            .await?
                            .ok_or_else(|| {
                                eyre::eyre!(
                                    "Stellar gateway returned non-bool approval result for tx {}",
                                    txs[i].message_id
                                )
                            })?;
                        let executed = if matches!(phase, Phase::Executed) {
                            Some(client
                                .gateway_is_message_executed(
                                    signer_pk,
                                    gateway_contract,
                                    source_chain,
                                    &txs[i].message_id,
                                )
                                .await?
                                .ok_or_else(|| {
                                    eyre::eyre!(
                                        "Stellar gateway returned non-bool execution result for tx {}",
                                        txs[i].message_id
                                    )
                                })?)
                        } else {
                            None
                        };
                        match phase {
                            Phase::Approved if approved => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            Phase::Executed if matches!(executed, Some(true)) => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
                DestinationChecker::Sui {
                    client,
                    gateway_pkg,
                    ..
                } => {
                    let approved_event_type = format!("{gateway_pkg}::events::MessageApproved");
                    let executed_event_type = format!("{gateway_pkg}::events::MessageExecuted");
                    for &i in &dest_indices {
                        let phase = txs[i].phase;
                        let approved = client
                            .has_message_approved(
                                &approved_event_type,
                                source_chain,
                                &txs[i].message_id,
                            )
                            .await?;
                        let executed = client
                            .has_message_executed(
                                &executed_event_type,
                                source_chain,
                                &txs[i].message_id,
                            )
                            .await?;
                        match phase {
                            Phase::Approved if executed => {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            Phase::Approved if approved => {
                                txs[i].timing.approved_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].phase = Phase::Executed;
                                last_progress = Instant::now();
                            }
                            Phase::Executed if executed => {
                                txs[i].timing.executed_secs =
                                    Some(txs[i].send_instant.elapsed().as_secs_f64());
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Update spinner with multi-phase progress + real-time throughput/latency
        let (voted, routed, hub_approved, approved, executed) = phase_counts(txs);
        let counts = [voted, routed, hub_approved, approved, executed];
        rt_stats.update(counts, txs);
        if voted + routed + approved + executed > 0 || error_msg.is_some() {
            let msg = if axelarnet_gateway.is_some() {
                rt_stats.spinner_msg_its(counts, total, error_msg.as_deref())
            } else {
                rt_stats.spinner_msg_gmp(
                    counts,
                    total,
                    error_msg.as_deref(),
                    voting_verifier.is_some(),
                    cosm_gateway.is_some(),
                )
            };
            spinner.set_message(msg);
        }

        // If no tx has made progress for INACTIVITY_TIMEOUT, stop waiting.
        // During streaming (send still in progress), use 2× timeout to allow for
        // slow send phases, but still break to avoid hanging indefinitely.
        let timeout = if sending_complete {
            INACTIVITY_TIMEOUT
        } else {
            INACTIVITY_TIMEOUT * 2
        };
        if last_progress.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed — but first do an authoritative
    // GMP-API recheck so a slow final leg that executed on-chain is recovered
    // as successful rather than reported as a false timeout.
    finalize_timed_out_txs(
        txs,
        network,
        checker.approval_label(),
        checker.execution_label(),
    )
    .await;

    let total = txs.len();
    let (voted, routed, hub_approved, approved, executed) = phase_counts(txs);
    let hub_str = if axelarnet_gateway.is_some() {
        format!("  hub: {hub_approved}/{total}")
    } else {
        String::new()
    };
    spinner.finish_and_clear();
    let label = display_chain.unwrap_or(destination_chain);
    ui::success_annotated(
        &format!(
            "voted: {voted}/{total}  routed: {routed}/{total}{hub_str}  approved: {approved}/{total}  executed: {executed}/{total}"
        ),
        label,
    );

    Ok(compute_peak_throughput(txs))
}

// ---------------------------------------------------------------------------
// ITS hub-only pipeline (Voted → HubApproved)
// ---------------------------------------------------------------------------

/// Destination chain kind for `poll_pipeline_its_hub` to know how to query
/// the final approval/execution stage.
#[derive(Clone)]
pub(super) enum ItsHubDest {
    /// Solana destination — uses `incoming_message` PDA + `command_id`.
    Solana {
        rpc_url: String,
        network: crate::types::Network,
    },
    /// Stellar destination — uses Soroban `is_message_approved` /
    /// `is_message_executed` view calls on the gateway contract.
    Stellar {
        rpc_url: String,
        network_type: String,
        gateway_contract: String,
        signer_pk: [u8; 32],
    },
    /// XRPL destination — polls the recipient account's `account_tx` for an
    /// incoming `Payment` whose `message_id` memo matches the second-leg id.
    /// The XRPL relayer attaches that memo when broadcasting proofs.
    Xrpl {
        rpc_url: String,
        recipient_address: String,
    },
    /// Sui destination — query AxelarGateway events `MessageApproved`
    /// (Approved phase) and `MessageExecuted` (Executed phase) on the
    /// second-leg (`source_chain = "axelar"`, second-leg message id).
    Sui {
        rpc_url: String,
        gateway_pkg: String,
    },
}

impl ItsHubDest {
    fn approval_label(&self) -> &str {
        match self {
            Self::Solana { .. } => "Solana approval",
            Self::Stellar { .. } => "Stellar approval",
            Self::Xrpl { .. } => "XRPL approval",
            Self::Sui { .. } => "Sui approval",
        }
    }

    fn execution_label(&self) -> &str {
        match self {
            Self::Solana { .. } => "Solana execution",
            Self::Stellar { .. } => "Stellar execution",
            Self::Xrpl { .. } => "XRPL execution",
            Self::Sui { .. } => "Sui execution",
        }
    }
}

pub(super) struct PollItsHubArgs {
    pub lcd: String,
    pub voting_verifier: Option<String>,
    pub source_chain: String,
    pub axelarnet_gateway: String,
    pub rpc: String,
    pub cosm_gateway_dest: String,
    pub dest: ItsHubDest,
    /// Network for the final Axelarscan GMP-API recheck on timed-out txs.
    pub network: crate::types::Network,
}

/// Full ITS polling pipeline: Voted → HubApproved → DiscoverSecondLeg → Routed → Approved → Executed.
#[allow(clippy::cognitive_complexity)]
pub(super) async fn poll_pipeline_its_hub(
    txs: &mut Vec<PendingTx>,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    external_spinner: Option<indicatif::ProgressBar>,
    args: PollItsHubArgs,
) -> Result<PeakThroughput> {
    let PollItsHubArgs {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        dest,
        network,
    } = args;
    let lcd = lcd.as_str();
    let voting_verifier = voting_verifier.as_deref();
    let source_chain = source_chain.as_str();
    let axelarnet_gateway = axelarnet_gateway.as_str();
    let rpc = rpc.as_str();
    let cosm_gateway_dest = cosm_gateway_dest.as_str();
    let spinner = external_spinner
        .unwrap_or_else(|| ui::wait_spinner("verifying ITS pipeline (starting)..."));
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();
    let mut received_first_tx = false;

    let sol_rpc_client = match &dest {
        ItsHubDest::Solana { rpc_url, .. } => Some(Arc::new(
            solana_client::rpc_client::RpcClient::new_with_commitment(
                rpc_url.clone(),
                solana_commitment_config::CommitmentConfig::finalized(),
            ),
        )),
        _ => None,
    };
    let stellar_client = match &dest {
        ItsHubDest::Stellar {
            rpc_url,
            network_type,
            ..
        } => Some(crate::stellar::StellarClient::new(rpc_url, network_type).ok()),
        _ => None,
    }
    .flatten();
    let xrpl_client = match &dest {
        ItsHubDest::Xrpl { rpc_url, .. } => Some(crate::xrpl::XrplClient::new(rpc_url)),
        _ => None,
    };
    let sui_client = match &dest {
        ItsHubDest::Sui { rpc_url, .. } => Some(crate::sui::SuiClient::new(rpc_url)),
        _ => None,
    };

    // Skip voting phase entirely if no VotingVerifier
    if voting_verifier.is_none() {
        for tx in txs.iter_mut() {
            if tx.phase == Phase::Voted {
                tx.phase = Phase::HubApproved;
            }
        }
    }

    loop {
        // Drain any newly-confirmed txs from the streaming channel.
        if let Some(ref mut receiver) = rx {
            while let Ok(mut new_tx) = receiver.try_recv() {
                // Skip voting if no verifier
                if voting_verifier.is_none() && new_tx.phase == Phase::Voted {
                    new_tx.phase = Phase::HubApproved;
                }
                txs.push(new_tx);
            }
        }

        let sending_complete = send_done.is_none_or(|f| f.load(Ordering::Relaxed));

        let total = txs.len();
        if total == 0 {
            if sending_complete {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        if !received_first_tx {
            received_first_tx = true;
        }

        let active: Vec<usize> = (0..txs.len())
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() && sending_complete {
            break;
        }
        if active.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
            last_progress = Instant::now();
            continue;
        }

        let error_msg: Option<String> = None;

        // Snapshot indices by phase
        let voted_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Voted)
            .copied()
            .collect();
        let hub_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::HubApproved)
            .copied()
            .collect();
        let discover_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::DiscoverSecondLeg)
            .copied()
            .collect();
        let routed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Routed)
            .copied()
            .collect();
        let approved_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Approved)
            .copied()
            .collect();
        let executed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Executed)
            .copied()
            .collect();

        // Build owned data for batch queries
        let voted_data: Vec<(usize, String, String, String)> = voted_indices
            .iter()
            .map(|&i| {
                (
                    i,
                    txs[i].message_id.clone(),
                    txs[i].source_address.clone(),
                    txs[i].payload_hash_hex.clone(),
                )
            })
            .collect();
        let hub_data: Vec<(usize, String)> = hub_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let routed_data: Vec<(usize, String)> = routed_indices
            .iter()
            .map(|&i| {
                Ok((
                    i,
                    required_second_leg_field(
                        &txs[i],
                        "message_id",
                        txs[i].second_leg_message_id.as_ref(),
                    )?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        // Solana destination checks: need command_id from second_leg_message_id
        let sol_dest_indices: Vec<usize> = approved_indices
            .iter()
            .chain(executed_indices.iter())
            .copied()
            .collect();
        let sol_dest_data: Vec<(usize, [u8; 32])> = sol_dest_indices
            .iter()
            .map(|&i| {
                let sl_id = required_second_leg_field(
                    &txs[i],
                    "message_id",
                    txs[i].second_leg_message_id.as_ref(),
                )?;
                let input = [b"axelar-".as_slice(), sl_id.as_bytes()].concat();
                Ok((i, keccak256(&input).into()))
            })
            .collect::<Result<Vec<_>>>()?;

        // --- Fire Cosmos batch phases concurrently ---
        let (dest_chain_for_vv, dest_addr_for_vv) = if voted_data.is_empty() {
            (String::new(), String::new())
        } else {
            let first = txs
                .first()
                .ok_or_else(|| eyre::eyre!("voted ITS batch has no transactions"))?;
            (
                first.gmp_destination_chain.clone(),
                first.gmp_destination_address.clone(),
            )
        };

        let (voted_results, hub_results, routed_results) = tokio::join!(
            async {
                if voted_data.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(vv) = voting_verifier {
                    batch_check_voting_verifier_owned(
                        lcd,
                        vv,
                        source_chain,
                        &dest_chain_for_vv,
                        &dest_addr_for_vv,
                        &voted_data,
                    )
                    .await
                } else {
                    Ok(voted_data.iter().map(|(i, ..)| (*i, true)).collect())
                }
            },
            async {
                if hub_data.is_empty() {
                    return Ok(Vec::new());
                }
                batch_check_hub_approved_owned(lcd, axelarnet_gateway, source_chain, &hub_data)
                    .await
            },
            async {
                if routed_data.is_empty() {
                    return Ok(Vec::new());
                }
                batch_check_cosmos_routed_owned(lcd, cosm_gateway_dest, "axelar", &routed_data)
                    .await
            },
        );
        let voted_results = voted_results?;
        let hub_results = hub_results?;
        let routed_results = routed_results?;

        // Apply Cosmos results
        for (i, ok) in voted_results {
            if ok {
                txs[i].timing.voted_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::HubApproved;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in hub_results {
            if ok {
                txs[i].timing.hub_approved_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::DiscoverSecondLeg;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in routed_results {
            if ok {
                txs[i].timing.routed_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::Approved;
                last_progress = Instant::now();
            }
        }

        // --- DiscoverSecondLeg: individual RPC tx_search (can't batch) ---
        if !discover_indices.is_empty() {
            let discover_futs: Vec<_> = discover_indices
                .iter()
                .map(|&i| {
                    let msg_id = txs[i].message_id.clone();
                    async move {
                        discover_second_leg(rpc, &msg_id)
                            .await
                            .map(|info| (i, info))
                    }
                })
                .collect();
            let discover_results: Vec<Result<_>> = futures::stream::iter(discover_futs)
                .buffer_unordered(20)
                .collect()
                .await;
            for result in discover_results {
                let (i, info) = result?;
                if let Some(info) = info {
                    txs[i].second_leg_message_id = Some(info.message_id);
                    txs[i].second_leg_payload_hash = Some(info.payload_hash);
                    txs[i].second_leg_source_address = Some(info.source_address);
                    txs[i].second_leg_destination_address = Some(info.destination_address);
                    txs[i].phase = Phase::Routed;
                    last_progress = Instant::now();
                }
            }
        }

        // --- Destination checks (per-chain) ---
        if !sol_dest_data.is_empty() || !approved_indices.is_empty() || !executed_indices.is_empty()
        {
            match &dest {
                ItsHubDest::Solana { network, .. } => {
                    if let Some(client) = sol_rpc_client.as_ref()
                        && !sol_dest_data.is_empty()
                    {
                        let client = client.clone();
                        let network = *network;
                        let data = sol_dest_data;
                        let results = tokio::task::spawn_blocking(move || {
                            batch_check_solana_incoming_messages(&client, network, &data)
                        })
                        .await
                        .wrap_err("Solana ITS destination check task failed")??;

                        for (i, status) in results {
                            match (txs[i].phase, status) {
                                (Phase::Approved, Some(0)) => {
                                    txs[i].timing.approved_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].phase = Phase::Executed;
                                    last_progress = Instant::now();
                                }
                                (Phase::Approved, Some(_)) => {
                                    let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                    if txs[i].timing.approved_secs.is_none() {
                                        txs[i].timing.approved_secs = Some(elapsed);
                                    }
                                    txs[i].timing.executed_secs = Some(elapsed);
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                (Phase::Executed, Some(s)) if s > 0 => {
                                    txs[i].timing.executed_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                ItsHubDest::Stellar {
                    gateway_contract,
                    signer_pk,
                    ..
                } => {
                    if let Some(client) = stellar_client.as_ref() {
                        let pending: Vec<usize> = approved_indices
                            .iter()
                            .chain(executed_indices.iter())
                            .copied()
                            .collect();
                        for i in pending {
                            let second_leg = required_second_leg_field(
                                &txs[i],
                                "message_id",
                                txs[i].second_leg_message_id.as_ref(),
                            )?;
                            // For ITS, the destination contract on Stellar is the
                            // ITS proxy (not the example), and the second-leg
                            // source is "axelar".
                            let dest_contract = required_second_leg_field(
                                &txs[i],
                                "destination_address",
                                txs[i].second_leg_destination_address.as_ref(),
                            )?;
                            let src_addr = required_second_leg_field(
                                &txs[i],
                                "source_address",
                                txs[i].second_leg_source_address.as_ref(),
                            )?;
                            let payload_hash = required_second_leg_payload_hash(&txs[i])?;
                            let payload_hash_arr: [u8; 32] = payload_hash.0;

                            let phase = txs[i].phase;
                            // For approved phase, query is_message_approved.
                            // For executed phase, query is_message_executed.
                            let result = if phase == Phase::Approved {
                                client
                                    .gateway_is_message_approved(
                                        signer_pk,
                                        gateway_contract,
                                        "axelar",
                                        &second_leg,
                                        &src_addr,
                                        &dest_contract,
                                        payload_hash_arr,
                                    )
                                    .await?
                                    .ok_or_else(|| {
                                        eyre::eyre!(
                                            "Stellar gateway returned non-bool approval result for tx {}",
                                            txs[i].message_id
                                        )
                                    })
                                    .map(|approved| if approved { 0u8 } else { 1u8 })
                            } else {
                                client
                                    .gateway_is_message_executed(
                                        signer_pk,
                                        gateway_contract,
                                        "axelar",
                                        &second_leg,
                                    )
                                    .await?
                                    .ok_or_else(|| {
                                        eyre::eyre!(
                                            "Stellar gateway returned non-bool execution result for tx {}",
                                            txs[i].message_id
                                        )
                                    })
                                    .map(|executed| if executed { 1u8 } else { 0u8 })
                            };

                            let result = result?;
                            match (phase, result) {
                                (Phase::Approved, 0) => {
                                    // approved=true → advance
                                    txs[i].timing.approved_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].phase = Phase::Executed;
                                    last_progress = Instant::now();
                                }
                                (Phase::Executed, 1) => {
                                    // executed=true → done
                                    txs[i].timing.executed_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                ItsHubDest::Xrpl {
                    recipient_address, ..
                } => {
                    if let Some(client) = xrpl_client.as_ref() {
                        // For XRPL we collapse approved + executed into a
                        // single "delivered" check: a Payment from the
                        // multisig with the matching `message_id` memo means
                        // the relayer's proof was broadcast and validated.
                        let pending: Vec<usize> = approved_indices
                            .iter()
                            .chain(executed_indices.iter())
                            .copied()
                            .collect();
                        for i in pending {
                            let second_leg = required_second_leg_field(
                                &txs[i],
                                "message_id",
                                txs[i].second_leg_message_id.as_ref(),
                            )?;
                            let found = client
                                .find_inbound_with_message_id(recipient_address, &second_leg, None)
                                .await?;
                            if found.is_some() {
                                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                if txs[i].timing.approved_secs.is_none() {
                                    txs[i].timing.approved_secs = Some(elapsed);
                                }
                                txs[i].timing.executed_secs = Some(elapsed);
                                txs[i].timing.executed_ok = Some(true);
                                txs[i].phase = Phase::Done;
                                last_progress = Instant::now();
                            }
                        }
                    }
                }
                ItsHubDest::Sui { gateway_pkg, .. } => {
                    if let Some(client) = sui_client.as_ref() {
                        // The Sui gateway emits MessageApproved / MessageExecuted
                        // for the *second* leg (hub→Sui): source_chain="axelar",
                        // message_id = the second-leg id discovered above.
                        let approved_event_type = format!("{gateway_pkg}::events::MessageApproved");
                        let executed_event_type = format!("{gateway_pkg}::events::MessageExecuted");
                        let pending: Vec<usize> = approved_indices
                            .iter()
                            .chain(executed_indices.iter())
                            .copied()
                            .collect();
                        for i in pending {
                            let second_leg = required_second_leg_field(
                                &txs[i],
                                "message_id",
                                txs[i].second_leg_message_id.as_ref(),
                            )?;
                            let approved = client
                                .has_message_approved(&approved_event_type, "axelar", &second_leg)
                                .await?;
                            let executed = client
                                .has_message_executed(&executed_event_type, "axelar", &second_leg)
                                .await?;
                            match txs[i].phase {
                                Phase::Approved if executed => {
                                    let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                                    if txs[i].timing.approved_secs.is_none() {
                                        txs[i].timing.approved_secs = Some(elapsed);
                                    }
                                    txs[i].timing.executed_secs = Some(elapsed);
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                Phase::Approved if approved => {
                                    txs[i].timing.approved_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].phase = Phase::Executed;
                                    last_progress = Instant::now();
                                }
                                Phase::Executed if executed => {
                                    txs[i].timing.executed_secs =
                                        Some(txs[i].send_instant.elapsed().as_secs_f64());
                                    txs[i].timing.executed_ok = Some(true);
                                    txs[i].phase = Phase::Done;
                                    last_progress = Instant::now();
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
        let routed = txs
            .iter()
            .filter(|t| t.timing.routed_secs.is_some())
            .count();
        let counts = [voted, routed, hub_approved, approved, executed];
        rt_stats.update(counts, txs);

        if voted + hub_approved + routed + approved + executed > 0 || error_msg.is_some() {
            spinner.set_message(rt_stats.spinner_msg_its(counts, total, error_msg.as_deref()));
        }

        let timeout = if sending_complete {
            INACTIVITY_TIMEOUT
        } else {
            INACTIVITY_TIMEOUT * 2
        };
        if last_progress.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed — but first do an authoritative
    // GMP-API recheck so a slow final leg that executed on-chain is recovered
    // as successful rather than reported as a false timeout.
    let total = txs.len();
    finalize_timed_out_txs(txs, network, dest.approval_label(), dest.execution_label()).await;

    let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
    let routed = txs
        .iter()
        .filter(|t| t.timing.routed_secs.is_some())
        .count();

    spinner.finish_and_clear();
    ui::success(&format!(
        "ITS pipeline: voted: {voted}/{total}  hub: {hub_approved}/{total}  routed: {routed}/{total}  approved: {approved}/{total}  executed: {executed}/{total}"
    ));

    Ok(compute_peak_throughput(txs))
}

/// EVM destination kind for the ITS-via-hub pipeline. Amplifier destinations
/// have a Cosmos Gateway (the `routed` phase) and verify the second leg via
/// `isMessageApproved`. Legacy (consensus) destinations have neither — the
/// second leg lands as a `ContractCallApproved` event on the legacy gateway
/// (read for its `commandId`, then confirmed via `isCommandExecuted`).
pub(super) enum ItsEvmDest {
    Amplifier,
    Legacy { from_block: u64 },
}

pub(super) struct PollItsHubEvmArgs {
    pub lcd: String,
    pub voting_verifier: Option<String>,
    pub source_chain: String,
    pub axelarnet_gateway: String,
    pub rpc: String,
    pub cosm_gateway_dest: String,
    pub _destination_chain: String,
    pub dest: ItsEvmDest,
    /// Network for the final Axelarscan GMP-API recheck on timed-out txs.
    pub network: crate::types::Network,
}

/// Full ITS polling pipeline with EVM destination (batch + streaming):
/// Voted → HubApproved → DiscoverSecondLeg → Routed → Approved(EVM) → Executed(EVM).
#[allow(clippy::cognitive_complexity)]
pub(super) async fn poll_pipeline_its_hub_evm<P: Provider>(
    txs: &mut Vec<PendingTx>,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    gw_contract: &AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&P>,
    external_spinner: Option<indicatif::ProgressBar>,
    args: PollItsHubEvmArgs,
) -> Result<PeakThroughput> {
    let PollItsHubEvmArgs {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        cosm_gateway_dest,
        _destination_chain,
        dest,
        network,
    } = args;
    let (dest_legacy, dest_from_block) = match dest {
        ItsEvmDest::Legacy { from_block } => (true, from_block),
        ItsEvmDest::Amplifier => (false, 0),
    };
    let lcd = lcd.as_str();
    let voting_verifier = voting_verifier.as_deref();
    let source_chain = source_chain.as_str();
    let axelarnet_gateway = axelarnet_gateway.as_str();
    let rpc = rpc.as_str();
    let cosm_gateway_dest = cosm_gateway_dest.as_str();
    let spinner = external_spinner
        .unwrap_or_else(|| ui::wait_spinner("verifying ITS pipeline (starting)..."));
    let mut last_progress = Instant::now();
    let mut rt_stats = RealTimeStats::new();

    // Skip voting phase if no VotingVerifier
    if voting_verifier.is_none() {
        for tx in txs.iter_mut() {
            if tx.phase == Phase::Voted {
                tx.phase = Phase::HubApproved;
            }
        }
    }

    loop {
        // Drain streaming channel
        if let Some(ref mut receiver) = rx {
            while let Ok(mut new_tx) = receiver.try_recv() {
                if voting_verifier.is_none() && new_tx.phase == Phase::Voted {
                    new_tx.phase = Phase::HubApproved;
                }
                txs.push(new_tx);
            }
        }

        let sending_complete = send_done.is_none_or(|f| f.load(Ordering::Relaxed));
        let total = txs.len();

        if total == 0 {
            if sending_complete {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let active: Vec<usize> = (0..total)
            .filter(|&i| !txs[i].failed && txs[i].phase != Phase::Done)
            .collect();

        if active.is_empty() && sending_complete {
            break;
        }
        if active.is_empty() {
            tokio::time::sleep(POLL_INTERVAL).await;
            last_progress = Instant::now();
            continue;
        }

        let error_msg: Option<String> = None;

        // Snapshot indices by phase
        let voted_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Voted)
            .copied()
            .collect();
        let hub_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::HubApproved)
            .copied()
            .collect();
        let discover_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::DiscoverSecondLeg)
            .copied()
            .collect();
        let routed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Routed)
            .copied()
            .collect();
        let approved_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Approved)
            .copied()
            .collect();
        let executed_indices: Vec<usize> = active
            .iter()
            .filter(|&&i| txs[i].phase == Phase::Executed)
            .copied()
            .collect();

        // Build owned data for batch queries
        let voted_data: Vec<(usize, String, String, String)> = voted_indices
            .iter()
            .map(|&i| {
                (
                    i,
                    txs[i].message_id.clone(),
                    txs[i].source_address.clone(),
                    txs[i].payload_hash_hex.clone(),
                )
            })
            .collect();
        let hub_data: Vec<(usize, String)> = hub_indices
            .iter()
            .map(|&i| (i, txs[i].message_id.clone()))
            .collect();
        let routed_data: Vec<(usize, String)> = routed_indices
            .iter()
            .map(|&i| {
                Ok((
                    i,
                    required_second_leg_field(
                        &txs[i],
                        "message_id",
                        txs[i].second_leg_message_id.as_ref(),
                    )?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        let (dest_chain_for_vv, dest_addr_for_vv) = if voted_data.is_empty() {
            (String::new(), String::new())
        } else {
            let first = txs
                .first()
                .ok_or_else(|| eyre::eyre!("voted ITS EVM batch has no transactions"))?;
            (
                first.gmp_destination_chain.clone(),
                first.gmp_destination_address.clone(),
            )
        };

        // --- Batch Cosmos phases concurrently ---
        let (voted_results, hub_results, routed_results) = tokio::join!(
            async {
                if voted_data.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(vv) = voting_verifier {
                    batch_check_voting_verifier_owned(
                        lcd,
                        vv,
                        source_chain,
                        &dest_chain_for_vv,
                        &dest_addr_for_vv,
                        &voted_data,
                    )
                    .await
                } else {
                    Ok(voted_data.iter().map(|(i, ..)| (*i, true)).collect())
                }
            },
            async {
                if hub_data.is_empty() {
                    return Ok(Vec::new());
                }
                batch_check_hub_approved_owned(lcd, axelarnet_gateway, source_chain, &hub_data)
                    .await
            },
            async {
                if routed_data.is_empty() {
                    return Ok(Vec::new());
                }
                batch_check_cosmos_routed_owned(lcd, cosm_gateway_dest, "axelar", &routed_data)
                    .await
            },
        );
        let voted_results = voted_results?;
        let hub_results = hub_results?;
        let routed_results = routed_results?;

        // Apply Cosmos results
        for (i, ok) in voted_results {
            if ok {
                txs[i].timing.voted_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::HubApproved;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in hub_results {
            if ok {
                let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                if txs[i].timing.voted_secs.is_none() {
                    txs[i].timing.voted_secs = Some(elapsed);
                }
                txs[i].timing.hub_approved_secs = Some(elapsed);
                txs[i].phase = Phase::DiscoverSecondLeg;
                last_progress = Instant::now();
            }
        }
        for (i, ok) in routed_results {
            if ok {
                txs[i].timing.routed_secs = Some(txs[i].send_instant.elapsed().as_secs_f64());
                txs[i].phase = Phase::Approved;
                last_progress = Instant::now();
            }
        }

        // --- DiscoverSecondLeg: individual RPC tx_search ---
        if !discover_indices.is_empty() {
            let discover_futs: Vec<_> = discover_indices
                .iter()
                .map(|&i| {
                    let msg_id = txs[i].message_id.clone();
                    async move {
                        discover_second_leg(rpc, &msg_id)
                            .await
                            .map(|info| (i, info))
                    }
                })
                .collect();
            let discover_results: Vec<Result<_>> = futures::stream::iter(discover_futs)
                .buffer_unordered(20)
                .collect()
                .await;
            for result in discover_results {
                let (i, info) = result?;
                if let Some(info) = info {
                    txs[i].second_leg_message_id = Some(info.message_id);
                    txs[i].second_leg_payload_hash = Some(info.payload_hash);
                    txs[i].second_leg_source_address = Some(info.source_address);
                    txs[i].second_leg_destination_address = Some(info.destination_address);
                    // A legacy destination has no Cosmos Gateway, so there is no
                    // `routed` phase to observe — go straight to the on-chain
                    // approval check on the legacy gateway.
                    txs[i].phase = if dest_legacy {
                        Phase::Approved
                    } else {
                        Phase::Routed
                    };
                    last_progress = Instant::now();
                }
            }
        }

        // --- EVM Approved/Executed checks (individual, buffer_unordered) ---
        let evm_check_indices: Vec<usize> = approved_indices
            .iter()
            .chain(executed_indices.iter())
            .copied()
            .collect();
        if !evm_check_indices.is_empty() && !dest_legacy {
            // Amplifier destination: verify the second leg via `isMessageApproved`.
            let mut evm_futs = Vec::with_capacity(evm_check_indices.len());
            for &i in &evm_check_indices {
                let phase = txs[i].phase;
                let sl_id = required_second_leg_field(
                    &txs[i],
                    "message_id",
                    txs[i].second_leg_message_id.as_ref(),
                )?;
                let sl_src = required_second_leg_field(
                    &txs[i],
                    "source_address",
                    txs[i].second_leg_source_address.as_ref(),
                )?;
                let sl_dst = required_second_leg_field(
                    &txs[i],
                    "destination_address",
                    txs[i].second_leg_destination_address.as_ref(),
                )?;
                let ph = required_second_leg_payload_hash(&txs[i])?;
                evm_futs.push(async move {
                    let dst_addr: Address = sl_dst.parse().wrap_err_with(|| {
                        format!("invalid second-leg EVM destination address {sl_dst}")
                    })?;
                    let approved = check_evm_is_message_approved(
                        gw_contract,
                        "axelar",
                        &sl_id,
                        &sl_src,
                        dst_addr,
                        ph,
                    )
                    .await?;
                    // `isMessageApproved` flips back to false on execution, so a
                    // fast second leg (hyperliquid, monad-3 executed in <40s) can
                    // be approved+executed between two polls and never read
                    // `approved == true`, leaving the tx stuck in Approved until
                    // the inactivity timeout. `isMessageExecuted` stays true and
                    // catches that race.
                    let executed =
                        check_evm_is_message_executed(gw_contract, "axelar", &sl_id).await?;
                    Ok((i, phase, approved, executed))
                });
            }
            let evm_results: Vec<Result<_>> = futures::stream::iter(evm_futs)
                .buffer_unordered(20)
                .collect()
                .await;
            for result in evm_results {
                let (i, phase, approved, executed) = result?;
                match phase {
                    // Authoritative fast-path: already executed on the
                    // destination gateway (catches the approve→execute race).
                    Phase::Approved if executed => {
                        let elapsed = txs[i].send_instant.elapsed().as_secs_f64();
                        if txs[i].timing.approved_secs.is_none() {
                            txs[i].timing.approved_secs = Some(elapsed);
                        }
                        txs[i].timing.executed_secs = Some(elapsed);
                        txs[i].timing.executed_ok = Some(true);
                        txs[i].phase = Phase::Done;
                        last_progress = Instant::now();
                    }
                    Phase::Approved if approved => {
                        txs[i].timing.approved_secs =
                            Some(txs[i].send_instant.elapsed().as_secs_f64());
                        txs[i].phase = Phase::Executed;
                        last_progress = Instant::now();
                    }
                    Phase::Executed if executed => {
                        txs[i].timing.executed_secs =
                            Some(txs[i].send_instant.elapsed().as_secs_f64());
                        txs[i].timing.executed_ok = Some(true);
                        txs[i].phase = Phase::Done;
                        last_progress = Instant::now();
                    }
                    _ => {}
                }
            }
        } else if !evm_check_indices.is_empty() {
            // Legacy (consensus) destination: the second leg lands as a
            // `ContractCallApproved` event on the legacy gateway, matched by its
            // (unique) second-leg payload hash + dest ITS address; execution is
            // then confirmed directly via `isCommandExecuted` (no approve→execute
            // race). Sequential per-tx — fine for legacy burst sizes.
            for &i in &evm_check_indices {
                let sl_dst = required_second_leg_field(
                    &txs[i],
                    "destination_address",
                    txs[i].second_leg_destination_address.as_ref(),
                )?;
                let dst_addr: Address = sl_dst.parse().wrap_err_with(|| {
                    format!("invalid second-leg EVM destination address {sl_dst}")
                })?;
                let ph = required_second_leg_payload_hash(&txs[i])?;
                match txs[i].phase {
                    Phase::Approved => {
                        let found = legacy::find_contract_call_approved_by_payload(
                            gw_contract.provider(),
                            *gw_contract.address(),
                            dst_addr,
                            ph,
                            dest_from_block,
                        )
                        .await?;
                        let Some(cmd_id) = found else { continue };
                        txs[i].command_id = Some(cmd_id);
                        txs[i].timing.approved_secs =
                            Some(txs[i].send_instant.elapsed().as_secs_f64());
                        let executed =
                            check_evm_command_executed(gw_contract, cmd_id.into()).await?;
                        if executed {
                            txs[i].timing.executed_secs =
                                Some(txs[i].send_instant.elapsed().as_secs_f64());
                            txs[i].timing.executed_ok = Some(true);
                            txs[i].phase = Phase::Done;
                        } else {
                            txs[i].phase = Phase::Executed;
                        }
                        last_progress = Instant::now();
                    }
                    Phase::Executed => {
                        let cmd_id = txs[i].command_id.ok_or_else(|| {
                            eyre::eyre!(
                                "legacy ITS tx {} in Executed phase without a commandId",
                                txs[i].message_id
                            )
                        })?;
                        if check_evm_command_executed(gw_contract, cmd_id.into()).await? {
                            txs[i].timing.executed_secs =
                                Some(txs[i].send_instant.elapsed().as_secs_f64());
                            txs[i].timing.executed_ok = Some(true);
                            txs[i].phase = Phase::Done;
                            last_progress = Instant::now();
                        }
                    }
                    _ => {}
                }
            }
        }

        let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
        let routed = txs
            .iter()
            .filter(|t| t.timing.routed_secs.is_some())
            .count();
        let counts = [voted, routed, hub_approved, approved, executed];
        rt_stats.update(counts, txs);

        if voted + hub_approved + routed + approved + executed > 0 || error_msg.is_some() {
            spinner.set_message(rt_stats.spinner_msg_its(counts, total, error_msg.as_deref()));
        }

        let timeout = if sending_complete {
            INACTIVITY_TIMEOUT
        } else {
            INACTIVITY_TIMEOUT * 2
        };
        if last_progress.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed — but first do an authoritative
    // GMP-API recheck so a slow final leg that executed on-chain is recovered
    // as successful rather than reported as a false timeout.
    let total = txs.len();
    finalize_timed_out_txs(txs, network, "EVM approval", "EVM execution").await;

    let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
    let routed = txs
        .iter()
        .filter(|t| t.timing.routed_secs.is_some())
        .count();

    spinner.finish_and_clear();
    ui::success(&format!(
        "ITS pipeline: voted: {voted}/{total}  hub: {hub_approved}/{total}  routed: {routed}/{total}  approved: {approved}/{total}  executed: {executed}/{total}"
    ));

    Ok(compute_peak_throughput(txs))
}

// ---------------------------------------------------------------------------
// Single-shot check helpers
// ---------------------------------------------------------------------------

/// Check VotingVerifier `messages_status` for a message.
/// Returns true if status contains "succeeded" (quorum reached).
#[allow(clippy::too_many_arguments, dead_code)]
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
    Ok(resp_str.to_lowercase().contains("succeeded"))
}

/// Check if message is routed on destination Cosmos Gateway via `outgoing_messages`.
pub(super) async fn check_cosmos_routed(
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

    let resp = match lcd_cosmwasm_smart_query(lcd, cosm_gateway, &query).await {
        Ok(resp) => resp,
        Err(e) if is_message_not_yet_routed_error(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    let data = resp.get("data").or_else(|| resp.as_array().map(|_| &resp));
    Ok(match data {
        Some(arr) if arr.is_array() => {
            let items = arr.as_array().unwrap();
            !items.is_empty() && !items.iter().all(|v| v.is_null())
        }
        _ => false,
    })
}

/// Check if a message is approved on the AxelarnetGateway hub via `executable_messages`.
pub(super) async fn check_hub_approved(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    message_id: &str,
) -> Result<bool> {
    let query = json!({
        "executable_messages": {
            "cc_ids": [{
                "source_chain": source_chain,
                "message_id": message_id,
            }]
        }
    });

    let resp = match lcd_cosmwasm_smart_query(lcd, axelarnet_gateway, &query).await {
        Ok(resp) => resp,
        Err(e) if is_hub_not_approved_error(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    let resp_str = serde_json::to_string(&resp)?;
    // The message is executable if the response is non-null and contains the message_id
    Ok(!resp_str.contains("null") && resp_str.contains(message_id))
}

// ---------------------------------------------------------------------------
// Batch check helpers — one query per phase per poll cycle
// ---------------------------------------------------------------------------

/// Max messages per Cosmos LCD batch query. The query is base64-encoded in
/// the URL, so each message adds ~500 chars. 10 keeps us under the ~8KB URL
/// limit that most HTTP servers enforce.
const COSMOS_BATCH_SIZE: usize = 10;

// ---------------------------------------------------------------------------
// Owned-data batch helpers — chunks run concurrently via join_all
// ---------------------------------------------------------------------------

/// Batch VotingVerifier check with owned data and concurrent chunks.
async fn batch_check_voting_verifier_owned(
    lcd: &str,
    voting_verifier: &str,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    txs: &[(usize, String, String, String)], // (idx, message_id, source_address, payload_hash_hex)
) -> Result<Vec<(usize, bool)>> {
    let futs: Vec<_> = txs
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let messages: Vec<_> = chunk
                .iter()
                .map(|(_, msg_id, src_addr, ph)| {
                    json!({
                        "cc_id": { "source_chain": source_chain, "message_id": msg_id },
                        "source_address": src_addr,
                        "destination_chain": destination_chain,
                        "destination_address": destination_address,
                        "payload_hash": ph,
                    })
                })
                .collect();
            let query = json!({ "messages_status": messages });
            let mut out = Vec::with_capacity(chunk.len());
            let resp = lcd_cosmwasm_smart_query(lcd, voting_verifier, &query).await?;
            let arr = resp.as_array().ok_or_else(|| {
                eyre::eyre!("VotingVerifier messages_status returned non-array: {resp}")
            })?;
            if arr.len() != chunk.len() {
                return Err(eyre::eyre!(
                    "VotingVerifier messages_status returned {} items for {} messages",
                    arr.len(),
                    chunk.len()
                ));
            }
            for (j, item) in arr.iter().enumerate() {
                if item.is_null() {
                    out.push((chunk[j].0, false));
                    continue;
                }
                let status = item.get("status").and_then(|s| s.as_str()).ok_or_else(|| {
                    eyre::eyre!("VotingVerifier status item missing string status: {item}")
                })?;
                out.push((chunk[j].0, status.to_lowercase().contains("succeeded")));
            }
            Ok(out)
        })
        .collect();
    Ok(futures::future::try_join_all(futs)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// Batch Cosmos Gateway routed check with owned data and concurrent chunks.
async fn batch_check_cosmos_routed_owned(
    lcd: &str,
    cosm_gateway: &str,
    source_chain: &str,
    txs: &[(usize, String)], // (idx, message_id)
) -> Result<Vec<(usize, bool)>> {
    let futs: Vec<_> = txs
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let cc_ids: Vec<_> = chunk
                .iter()
                .map(|(_, msg_id)| json!({ "source_chain": source_chain, "message_id": msg_id }))
                .collect();
            let query = json!({ "outgoing_messages": cc_ids });
            let mut out = Vec::with_capacity(chunk.len());
            let resp = match lcd_cosmwasm_smart_query(lcd, cosm_gateway, &query).await {
                Ok(resp) => resp,
                Err(e) if is_message_not_yet_routed_error(&e) => {
                    // The whole batch is "not yet routed"; mark all as false.
                    for (idx, _) in chunk {
                        out.push((*idx, false));
                    }
                    return Ok::<Vec<(usize, bool)>, eyre::Report>(out);
                }
                Err(e) => return Err(e),
            };
            let arr = resp.as_array().ok_or_else(|| {
                eyre::eyre!("Gateway outgoing_messages returned non-array: {resp}")
            })?;
            if arr.len() != chunk.len() {
                return Err(eyre::eyre!(
                    "Gateway outgoing_messages returned {} items for {} messages",
                    arr.len(),
                    chunk.len()
                ));
            }
            for (j, item) in arr.iter().enumerate() {
                out.push((chunk[j].0, !item.is_null()));
            }
            Ok(out)
        })
        .collect();
    Ok(futures::future::try_join_all(futs)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// Batch AxelarnetGateway hub-approved check with owned data and concurrent chunks.
async fn batch_check_hub_approved_owned(
    lcd: &str,
    axelarnet_gateway: &str,
    source_chain: &str,
    txs: &[(usize, String)], // (idx, message_id)
) -> Result<Vec<(usize, bool)>> {
    let futs: Vec<_> = txs
        .chunks(COSMOS_BATCH_SIZE)
        .map(|chunk| async move {
            let cc_ids: Vec<_> = chunk
                .iter()
                .map(|(_, msg_id)| json!({ "source_chain": source_chain, "message_id": msg_id }))
                .collect();
            let query = json!({ "executable_messages": { "cc_ids": cc_ids } });
            let mut out = Vec::with_capacity(chunk.len());
            let resp = match lcd_cosmwasm_smart_query(lcd, axelarnet_gateway, &query).await {
                Ok(resp) => resp,
                Err(e) if is_hub_not_approved_error(&e) => {
                    for (idx, _) in chunk {
                        out.push((*idx, false));
                    }
                    return Ok(out);
                }
                Err(e) => return Err(e),
            };
            let arr = resp.as_array().ok_or_else(|| {
                eyre::eyre!("AxelarnetGateway executable_messages returned non-array: {resp}")
            })?;
            if arr.len() != chunk.len() {
                return Err(eyre::eyre!(
                    "AxelarnetGateway executable_messages returned {} items for {} messages",
                    arr.len(),
                    chunk.len()
                ));
            }
            for (j, item) in arr.iter().enumerate() {
                out.push((chunk[j].0, !item.is_null()));
            }
            Ok(out)
        })
        .collect();
    Ok(futures::future::try_join_all(futs)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy::primitives::Address;

    use super::{PendingTx, Phase, mark_timed_out_tx, parse_payload_hash};
    use crate::commands::load_test::metrics::AmplifierTiming;

    fn pending_at(phase: Phase) -> PendingTx {
        PendingTx {
            idx: 0,
            message_id: "0xdeadbeef-0".into(),
            send_instant: Instant::now(),
            source_address: String::new(),
            contract_addr: Address::ZERO,
            payload_hash: None,
            payload_hash_hex: String::new(),
            command_id: None,
            gmp_destination_chain: String::new(),
            gmp_destination_address: String::new(),
            timing: AmplifierTiming::default(),
            failed: false,
            fail_reason: None,
            phase,
            second_leg_message_id: None,
            second_leg_payload_hash: None,
            second_leg_source_address: None,
            second_leg_destination_address: None,
            recovered_via_api: false,
        }
    }

    #[test]
    fn timed_out_tx_reported_executed_is_recovered() {
        // A tx stuck at the timeout that the GMP API confirms executed is
        // reclassified as a successful, recovered tx — never marked failed.
        let mut tx = pending_at(Phase::Executed);
        mark_timed_out_tx(&mut tx, true, "EVM approval", "EVM execution");

        assert!(!tx.failed);
        assert!(tx.recovered_via_api);
        assert_eq!(tx.phase, Phase::Done);
        assert_eq!(tx.timing.executed_ok, Some(true));
        assert!(tx.timing.executed_secs.is_some());
        assert!(tx.fail_reason.is_none());
    }

    #[test]
    fn timed_out_tx_not_executed_stays_failed() {
        // A genuinely-unexecuted message keeps the original timed-out verdict.
        let mut tx = pending_at(Phase::Approved);
        mark_timed_out_tx(&mut tx, false, "EVM approval", "EVM execution");

        assert!(tx.failed);
        assert!(!tx.recovered_via_api);
        assert_eq!(tx.timing.executed_ok, None);
        assert_eq!(tx.fail_reason.as_deref(), Some("EVM approval: timed out"));
    }

    #[test]
    fn parse_payload_hash_accepts_prefixed_and_unprefixed_hashes() {
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        assert_eq!(parse_payload_hash(hash).unwrap().0, [0xaa; 32]);
        assert_eq!(
            parse_payload_hash(&format!("0x{hash}")).unwrap().0,
            [0xaa; 32]
        );
    }

    #[test]
    fn parse_payload_hash_rejects_bad_length() {
        let err = parse_payload_hash("0x1234").unwrap_err();

        assert!(err.to_string().contains("32 bytes"));
    }

    #[test]
    fn parse_payload_hash_rejects_bad_hex() {
        let err =
            parse_payload_hash("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")
                .unwrap_err();

        assert!(err.to_string().to_lowercase().contains("invalid"));
    }
}
