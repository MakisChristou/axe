//! Shared verification polling pipeline.
//! in `mod.rs`: focused [`DestinationVerifier`] adapters for chain-specific
//! RPCs, [`PollScheduler`] for common streaming/timeout behavior, and
//! [`ItsHubVerifier`] for the shared Cosmos side of hub-routed ITS.
//!
//! The orchestrators in `mod.rs` build [`PendingTx`] vectors, hand them to one
//! of the `poll_pipeline*` functions, and turn the resulting [`PeakThroughput`]
//! plus the populated `tx.timing` into a `VerificationReport` via
//! [`super::report::compute_verification_report`].

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use alloy::hex;
use alloy::primitives::{Address, FixedBytes, keccak256};
use alloy::providers::Provider;
use eyre::{Result, WrapErr};
use futures::{StreamExt, stream};
use serde::de::DeserializeOwned;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use tokio::sync::mpsc;
use tokio::{task, time};

use super::POLL_INTERVAL;
use super::PendingTx;
use super::checks::{
    batch_check_solana_incoming_messages, check_evm_command_executed,
    check_evm_is_message_approved, check_evm_is_message_executed,
};
use super::legacy;
use super::report::compute_peak_throughput;
use super::state::{Phase, RealTimeStats, SecondLeg, phase_counts};
use crate::commands::load_test::identifiers::PayloadHash;
use crate::commands::load_test::metrics::{AmplifierTiming, PeakThroughput};
use crate::cosmos::{
    CosmwasmQueryError, CosmwasmQueryPending, discover_second_leg, lcd_cosmwasm_smart_query_typed,
};
use crate::evm::AxelarAmplifierGateway;
use crate::gmp_api;
use crate::stellar::StellarClient;
use crate::sui::SuiClient;
use crate::types::Network;
use crate::ui;
use crate::xrpl::XrplClient;

#[path = "pipeline/cosmos.rs"]
mod cosmos;
#[path = "pipeline/destination.rs"]
mod destination;
#[path = "pipeline/scheduler.rs"]
mod scheduler;

use self::cosmos::{
    batch_check_cosmos_routed_owned, batch_check_hub_approved_owned,
    batch_check_voting_verifier_owned,
};
pub(super) use self::cosmos::{check_cosmos_routed, check_hub_approved};
pub(super) use self::destination::{
    DestinationCheckFuture, DestinationVerifier, EvmDestinationVerifier, SolanaDestinationVerifier,
    StellarDestinationVerifier, SuiDestinationVerifier,
};
use self::destination::{DestinationObservation, DestinationStatus};
use self::scheduler::{PollAction, PollScheduler};

/// Parse a hex-encoded 32-byte payload hash, with or without the `0x`
/// prefix. Returns an error rather than silently zero-extending so a
/// truncated hash from upstream code surfaces immediately instead of
/// propagating into a downstream "wrong gateway hash" mismatch.
pub(super) fn parse_payload_hash(hex_str: &str) -> Result<PayloadHash> {
    let bytes = hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?;
    if bytes.len() != 32 {
        return Err(eyre::eyre!(
            "payload_hash must be 32 bytes, got {}",
            bytes.len()
        ));
    }
    Ok(FixedBytes::from_slice(&bytes).into())
}

fn required_payload_hash(tx: &PendingTx) -> Result<FixedBytes<32>> {
    tx.payload_hash
        .map(PayloadHash::into_fixed_bytes)
        .ok_or_else(|| eyre::eyre!("tx {} has no first-leg payload_hash", tx.message_id))
}

fn required_second_leg(tx: &PendingTx) -> Result<&SecondLeg> {
    tx.second_leg()
        .ok_or_else(|| eyre::eyre!("tx {} has no discovered second leg", tx.message_id))
}

fn required_second_leg_payload_hash(tx: &PendingTx) -> Result<FixedBytes<32>> {
    Ok(required_second_leg(tx)?.payload_hash.into_fixed_bytes())
}

enum ContractQueryObservation<T> {
    Ready(T),
    Pending,
}

fn is_pending_contract_error(error: &CosmwasmQueryError, pending: CosmwasmQueryPending) -> bool {
    error.is_pending(pending)
}

/// Preserve the distinction between a contract-level "not ready yet"
/// response and failures in transport or response decoding. Some LCDs return
/// a 500 for a valid query while the message is still being routed; only the
/// typed HTTP response body is eligible for that pending classification.
async fn observe_contract_query<T: DeserializeOwned>(
    lcd: &str,
    contract: &str,
    // CosmWasm smart-query messages have contract-specific wire shapes. The
    // response is converted to `T` immediately below; keeping only this
    // transport boundary dynamic avoids a misleading shared query enum.
    query: &serde_json::Value,
    pending: CosmwasmQueryPending,
) -> Result<ContractQueryObservation<T>> {
    match lcd_cosmwasm_smart_query_typed(lcd, contract, query).await {
        Ok(response) => serde_json::from_value(response)
            .map(ContractQueryObservation::Ready)
            .wrap_err("CosmWasm verification response had an unexpected shape"),
        Err(error) if is_pending_contract_error(&error, pending) => {
            Ok(ContractQueryObservation::Pending)
        }
        Err(error) => Err(error).wrap_err("CosmWasm verification query failed"),
    }
}

/// Stamp the elapsed time into one Cosmos-side timing slot and advance every
/// tx whose check came back positive. Returns whether anything moved.
fn apply_cosmos_results(
    txs: &mut [PendingTx],
    results: Vec<(usize, bool)>,
    timing_slot: impl Fn(&mut AmplifierTiming) -> &mut Option<f64>,
    next_phase: Phase,
) -> bool {
    let mut progressed = false;
    for (index, observed) in results {
        if !observed {
            continue;
        }
        let tx = &mut txs[index];
        *timing_slot(&mut tx.timing) = Some(tx.send_instant.elapsed().as_secs_f64());
        progressed |= tx.transition_to(next_phase);
    }
    progressed
}

fn apply_destination_observations(
    txs: &mut [PendingTx],
    observations: Vec<DestinationObservation>,
) -> bool {
    let mut progressed = false;
    for DestinationObservation { index, status } in observations {
        let tx = &mut txs[index];
        // A verifier that reports twice for the same tx in one cycle must not
        // re-apply timings on top of a settled state.
        if !tx.is_active() {
            continue;
        }
        match status {
            DestinationStatus::Pending => {}
            DestinationStatus::Approved { command_id } if tx.is_phase(Phase::Approved) => {
                tx.timing.approved_secs = Some(tx.send_instant.elapsed().as_secs_f64());
                progressed |= tx.approve_destination(command_id);
            }
            DestinationStatus::Executed { command_id } => {
                let elapsed = tx.send_instant.elapsed().as_secs_f64();
                if tx.timing.approved_secs.is_none() {
                    tx.timing.approved_secs = Some(elapsed);
                }
                tx.timing.executed_secs = Some(elapsed);
                tx.timing.executed_ok = Some(true);
                progressed |= tx.execute_destination(command_id);
            }
            DestinationStatus::Approved { .. } => {}
        }
    }
    progressed
}

struct PhaseIndices {
    voted: Vec<usize>,
    routed: Vec<usize>,
    hub_approved: Vec<usize>,
    discover_second_leg: Vec<usize>,
    approved: Vec<usize>,
    executed: Vec<usize>,
}

impl PhaseIndices {
    fn collect(txs: &[PendingTx], active: &[usize]) -> Self {
        let for_phase = |phase| {
            active
                .iter()
                .copied()
                .filter(|&index| txs[index].is_phase(phase))
                .collect()
        };
        Self {
            voted: for_phase(Phase::Voted),
            routed: for_phase(Phase::Routed),
            hub_approved: for_phase(Phase::HubApproved),
            discover_second_leg: for_phase(Phase::DiscoverSecondLeg),
            approved: for_phase(Phase::Approved),
            executed: for_phase(Phase::Executed),
        }
    }

    fn destination(&self) -> Vec<usize> {
        self.approved
            .iter()
            .chain(self.executed.iter())
            .copied()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Final GMP-API recheck for timed-out txs
// ---------------------------------------------------------------------------

/// Bound each GMP-API recheck attempt so a slow or unreachable API can never
/// hang the end of a verification run.
const GMP_API_RECHECK_TIMEOUT: Duration = Duration::from_secs(15);
const GMP_API_RECHECK_ATTEMPTS: u32 = 3;
const GMP_API_RECHECK_BACKOFF: Duration = Duration::from_secs(10);

/// Best-effort, non-fatal final check: ask the Axelarscan GMP API whether this
/// message actually executed on-chain. A slow final leg or a missed poll can
/// leave an executed transfer looking failed at the inactivity timeout (the
/// MOU-3/MOU-4 failure mode); this is the safety net layered on top of those
/// fixes. Retries so a transient API error or a message still transitioning to
/// executed does not become a permanent failed verdict.
async fn gmp_api_reports_executed(network: Network, message_id: &str) -> bool {
    let base = gmp_api::base_url(network);
    for attempt in 0..GMP_API_RECHECK_ATTEMPTS {
        let lookup = async {
            // Prefer the exact `message_id` Axelarscan keys on. If it isn't
            // indexed in that form for this source chain, fall back to the
            // source tx id (the portion before the event-index suffix).
            if let Some(rec) = gmp_api::search_by_message_id(base, message_id).await? {
                return Ok::<bool, eyre::Report>(rec.is_executed());
            }
            if let Some((source_tx, _)) = message_id.rsplit_once('-')
                && let Some(rec) = gmp_api::search_by_tx(base, source_tx).await?
            {
                return Ok(rec.is_executed());
            }
            Ok(false)
        };
        if let Ok(Ok(true)) = time::timeout(GMP_API_RECHECK_TIMEOUT, lookup).await {
            return true;
        }
        if attempt + 1 < GMP_API_RECHECK_ATTEMPTS {
            time::sleep(GMP_API_RECHECK_BACKOFF).await;
        }
    }
    false
}

/// Apply the final verdict to a tx that was still active at the
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
        if tx.timing.executed_secs.is_none() {
            tx.timing.executed_secs = Some(tx.send_instant.elapsed().as_secs_f64());
        }
        tx.timing.executed_ok = Some(true);
        tx.succeed(true);
        return;
    }
    let Some(phase) = tx.phase() else {
        return;
    };
    let label = match phase {
        Phase::Voted => "VotingVerifier",
        Phase::Routed => "cosmos routing",
        Phase::HubApproved => "hub approval",
        Phase::DiscoverSecondLeg => "second-leg discovery",
        Phase::Approved => approval_label,
        Phase::Executed => execution_label,
    };
    if phase == Phase::Executed {
        tx.timing.executed_ok = Some(false);
    }
    tx.time_out(label);
}

/// Finalize the polling loop: for every tx that didn't reach `Done`, do a
/// best-effort GMP-API recheck and either recover it (executed on-chain) or
/// mark it timed-out failed. Sequential by design — this runs once at the end
/// of a run (single-test CI has `num_txs=1`), so concurrency is naturally
/// bounded to one in-flight recheck.
async fn finalize_timed_out_txs(
    txs: &mut [PendingTx],
    network: Network,
    approval_label: &str,
    execution_label: &str,
) {
    for tx in txs.iter_mut() {
        if !tx.is_active() {
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
    pub network: Network,
}

struct PhaseCheckData {
    voted: Vec<(usize, String, String, String)>,
    routed: Vec<(usize, String)>,
    hub_approved: Vec<(usize, String)>,
    destination: Vec<usize>,
    voting_destination: (String, String),
}

struct PhaseCheckResults {
    voted: Vec<(usize, bool)>,
    routed: Vec<(usize, bool)>,
    hub_approved: Vec<(usize, bool)>,
}

struct CosmosPollContext<'a> {
    lcd: &'a str,
    voting_verifier: Option<&'a str>,
    cosm_gateway: Option<&'a str>,
    axelarnet_gateway: Option<&'a str>,
    source_chain: &'a str,
}

fn collect_phase_check_data(
    txs: &[PendingTx],
    active: &[usize],
    destination_chain: &str,
    destination_address: &str,
) -> PhaseCheckData {
    let phases = PhaseIndices::collect(txs, active);
    let voted = phases
        .voted
        .iter()
        .map(|&index| {
            (
                index,
                txs[index].message_id.clone().into_string(),
                txs[index].source_address.clone(),
                txs[index].payload_hash_hex.clone(),
            )
        })
        .collect();
    let routed = phases
        .routed
        .iter()
        .map(|&index| (index, txs[index].message_id.clone().into_string()))
        .collect();
    let hub_approved = phases
        .hub_approved
        .iter()
        .map(|&index| (index, txs[index].message_id.clone().into_string()))
        .collect();
    let voting_destination = phases
        .voted
        .first()
        .map(|&index| {
            (
                txs[index].gmp_destination_chain.clone(),
                txs[index].gmp_destination_address.clone(),
            )
        })
        .filter(|(chain, address)| !chain.is_empty() && !address.is_empty())
        .unwrap_or_else(|| {
            (
                destination_chain.to_string(),
                destination_address.to_string(),
            )
        });
    PhaseCheckData {
        voted,
        routed,
        hub_approved,
        destination: phases.destination(),
        voting_destination,
    }
}

async fn run_cosmos_phase_checks(
    context: &CosmosPollContext<'_>,
    data: &PhaseCheckData,
) -> Result<PhaseCheckResults> {
    let (voted, routed, hub_approved) = tokio::join!(
        async {
            match (data.voted.is_empty(), context.voting_verifier) {
                (true, _) => Ok(Vec::new()),
                (false, Some(verifier)) => {
                    batch_check_voting_verifier_owned(
                        context.lcd,
                        verifier,
                        context.source_chain,
                        &data.voting_destination.0,
                        &data.voting_destination.1,
                        &data.voted,
                    )
                    .await
                }
                (false, None) => Ok(data
                    .voted
                    .iter()
                    .map(|(index, ..)| (*index, true))
                    .collect()),
            }
        },
        async {
            match (data.routed.is_empty(), context.cosm_gateway) {
                (true, _) => Ok(Vec::new()),
                (false, Some(gateway)) => {
                    batch_check_cosmos_routed_owned(
                        context.lcd,
                        gateway,
                        context.source_chain,
                        &data.routed,
                    )
                    .await
                }
                (false, None) => Ok(data
                    .routed
                    .iter()
                    .map(|(index, _)| (*index, true))
                    .collect()),
            }
        },
        async {
            match (data.hub_approved.is_empty(), context.axelarnet_gateway) {
                (true, _) => Ok(Vec::new()),
                (false, Some(gateway)) => {
                    batch_check_hub_approved_owned(
                        context.lcd,
                        gateway,
                        context.source_chain,
                        &data.hub_approved,
                    )
                    .await
                }
                (false, None) => Ok(data
                    .hub_approved
                    .iter()
                    .map(|(index, _)| (*index, true))
                    .collect()),
            }
        },
    );
    Ok(PhaseCheckResults {
        voted: voted?,
        routed: routed?,
        hub_approved: hub_approved?,
    })
}

fn apply_phase_check_results(
    txs: &mut [PendingTx],
    results: PhaseCheckResults,
    has_cosm_gateway: bool,
    has_axelarnet_gateway: bool,
) -> bool {
    apply_cosmos_results(
        txs,
        results.voted,
        |timing| &mut timing.voted_secs,
        if has_cosm_gateway {
            Phase::Routed
        } else {
            Phase::Approved
        },
    ) | apply_cosmos_results(
        txs,
        results.routed,
        |timing| &mut timing.routed_secs,
        if has_axelarnet_gateway {
            Phase::HubApproved
        } else {
            Phase::Approved
        },
    ) | apply_cosmos_results(
        txs,
        results.hub_approved,
        |timing| &mut timing.hub_approved_secs,
        Phase::Approved,
    )
}

fn update_pipeline_spinner(
    spinner: &indicatif::ProgressBar,
    stats: &mut RealTimeStats,
    txs: &[PendingTx],
    total: usize,
    has_voting_verifier: bool,
    has_cosm_gateway: bool,
    has_axelarnet_gateway: bool,
) {
    let (voted, routed, hub_approved, approved, executed) = phase_counts(txs);
    let counts = [voted, routed, hub_approved, approved, executed];
    stats.update(counts, txs);
    if voted + routed + approved + executed == 0 {
        return;
    }
    let message = if has_axelarnet_gateway {
        stats.spinner_msg_its(counts, total, None)
    } else {
        stats.spinner_msg_gmp(counts, total, None, has_voting_verifier, has_cosm_gateway)
    };
    spinner.set_message(message);
}

fn finish_pipeline_display(
    spinner: &indicatif::ProgressBar,
    txs: &[PendingTx],
    destination_chain: &str,
    display_chain: Option<&str>,
    has_axelarnet_gateway: bool,
) {
    let total = txs.len();
    let (voted, routed, hub_approved, approved, executed) = phase_counts(txs);
    let hub = if has_axelarnet_gateway {
        format!("  hub: {hub_approved}/{total}")
    } else {
        String::new()
    };
    spinner.finish_and_clear();
    ui::success_annotated(
        &format!(
            "voted: {voted}/{total}  routed: {routed}/{total}{hub}  approved: {approved}/{total}  executed: {executed}/{total}"
        ),
        display_chain.unwrap_or(destination_chain),
    );
}

struct PipelineCheckContext<'a> {
    cosmos: CosmosPollContext<'a>,
    destination_chain: &'a str,
    destination_address: &'a str,
}

async fn check_pipeline_phases<V: DestinationVerifier>(
    txs: &mut [PendingTx],
    active: &[usize],
    verifier: &V,
    context: &PipelineCheckContext<'_>,
) -> Result<bool> {
    let phase_data = collect_phase_check_data(
        txs,
        active,
        context.destination_chain,
        context.destination_address,
    );
    let phase_results = run_cosmos_phase_checks(&context.cosmos, &phase_data).await?;
    let mut progressed = apply_phase_check_results(
        txs,
        phase_results,
        context.cosmos.cosm_gateway.is_some(),
        context.cosmos.axelarnet_gateway.is_some(),
    );
    if !phase_data.destination.is_empty() {
        let observations = verifier
            .check(context.cosmos.source_chain, txs, &phase_data.destination)
            .await?;
        progressed |= apply_destination_observations(txs, observations);
    }
    Ok(progressed)
}

pub(super) async fn poll_pipeline<V: DestinationVerifier>(
    txs: &mut Vec<PendingTx>,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    verifier: &V,
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
    let mut scheduler = PollScheduler::new();
    let mut rt_stats = RealTimeStats::new();
    let mut received_first_tx = false;

    // For EVM destinations, derive the contract_addr from destination_address
    // so streaming PendingTx entries (which may have Address::ZERO) get the right value.
    let default_contract_addr = if verifier.needs_contract_address() {
        Some(destination_address.parse()?)
    } else {
        None
    };

    loop {
        let action = scheduler.next_action(txs, rx.as_deref_mut(), send_done, |new_tx| {
            if new_tx.contract_addr == Address::ZERO
                && let Some(contract_addr) = default_contract_addr
            {
                new_tx.contract_addr = contract_addr;
            }
        });
        let (active, total, sending_complete) = match action {
            PollAction::Finished => break,
            PollAction::Waiting => {
                time::sleep(POLL_INTERVAL).await;
                continue;
            }
            PollAction::Ready {
                active,
                total,
                sending_complete,
            } => (active, total, sending_complete),
        };

        if !received_first_tx {
            received_first_tx = true;
            spinner.set_message(format!("verifying pipeline: 0/{total} confirmed..."));
        }

        let check_context = PipelineCheckContext {
            cosmos: CosmosPollContext {
                lcd,
                voting_verifier,
                cosm_gateway,
                axelarnet_gateway,
                source_chain,
            },
            destination_chain,
            destination_address,
        };
        if check_pipeline_phases(txs, &active, verifier, &check_context).await? {
            scheduler.mark_progress();
        }

        update_pipeline_spinner(
            &spinner,
            &mut rt_stats,
            txs,
            total,
            voting_verifier.is_some(),
            cosm_gateway.is_some(),
            axelarnet_gateway.is_some(),
        );

        // If no tx has made progress for INACTIVITY_TIMEOUT, stop waiting.
        // During streaming (send still in progress), use 2× timeout to allow for
        // slow send phases, but still break to avoid hanging indefinitely.
        if scheduler.timed_out(sending_complete) {
            break;
        }
        time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed — but first do an authoritative
    // GMP-API recheck so a slow final leg that executed on-chain is recovered
    // as successful rather than reported as a false timeout.
    finalize_timed_out_txs(
        txs,
        network,
        verifier.approval_label(),
        verifier.execution_label(),
    )
    .await;

    finish_pipeline_display(
        &spinner,
        txs,
        destination_chain,
        display_chain,
        axelarnet_gateway.is_some(),
    );

    Ok(compute_peak_throughput(txs))
}

// ---------------------------------------------------------------------------
// ITS hub-only pipeline (Voted → HubApproved)
// ---------------------------------------------------------------------------

/// Solana second-leg verifier used by the ITS hub pipeline.
pub(super) struct SolanaItsDestinationVerifier {
    rpc_client: Arc<RpcClient>,
    network: Network,
}

impl SolanaItsDestinationVerifier {
    pub(super) fn new(rpc_url: String, network: Network) -> Self {
        Self {
            rpc_client: Arc::new(RpcClient::new_with_commitment(
                rpc_url,
                CommitmentConfig::finalized(),
            )),
            network,
        }
    }
}

impl DestinationVerifier for SolanaItsDestinationVerifier {
    fn approval_label(&self) -> &str {
        "Solana approval"
    }

    fn execution_label(&self) -> &str {
        "Solana execution"
    }

    fn check<'a>(
        &'a self,
        _source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            let data = indices
                .iter()
                .map(|&index| {
                    let second_leg = required_second_leg(&txs[index])?;
                    let input = [b"axelar-".as_slice(), second_leg.message_id.as_bytes()].concat();
                    Ok((index, keccak256(&input).into()))
                })
                .collect::<Result<Vec<_>>>()?;
            let client = Arc::clone(&self.rpc_client);
            let network = self.network;
            let results = task::spawn_blocking(move || {
                batch_check_solana_incoming_messages(&client, network, &data)
            })
            .await
            .wrap_err("Solana ITS destination check task failed")??;
            Ok(results
                .into_iter()
                .map(|(index, status)| DestinationObservation {
                    index,
                    status: match status {
                        None => DestinationStatus::Pending,
                        Some(0) => DestinationStatus::Approved { command_id: None },
                        Some(_) => DestinationStatus::Executed { command_id: None },
                    },
                })
                .collect())
        })
    }
}

/// Stellar second-leg verifier used by the ITS hub pipeline.
pub(super) struct StellarItsDestinationVerifier {
    client: StellarClient,
    gateway_contract: String,
    signer_pk: [u8; 32],
}

impl StellarItsDestinationVerifier {
    pub(super) fn new(
        rpc_url: &str,
        network_type: &str,
        gateway_contract: String,
        signer_pk: [u8; 32],
    ) -> Result<Self> {
        Ok(Self {
            client: StellarClient::new(rpc_url, network_type)?,
            gateway_contract,
            signer_pk,
        })
    }
}

impl DestinationVerifier for StellarItsDestinationVerifier {
    fn approval_label(&self) -> &str {
        "Stellar approval"
    }

    fn execution_label(&self) -> &str {
        "Stellar execution"
    }

    fn check<'a>(
        &'a self,
        _source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            let mut observations = Vec::with_capacity(indices.len());
            for &index in indices {
                let tx = &txs[index];
                let second_leg = required_second_leg(tx)?;
                let status = match tx.phase() {
                    Some(Phase::Approved) => {
                        let approved = self
                            .client
                            .gateway_is_message_approved(crate::stellar::MessageApprovalQuery {
                                signer_account_pk: &self.signer_pk,
                                gateway_contract: &self.gateway_contract,
                                source_chain: "axelar",
                                message_id: &second_leg.message_id,
                                source_address: &second_leg.source_address,
                                contract_address: &second_leg.destination_address,
                                payload_hash: required_second_leg_payload_hash(tx)?.0,
                            })
                            .await?
                            .ok_or_else(|| {
                                eyre::eyre!(
                                    "Stellar gateway returned non-bool approval result for tx {}",
                                    tx.message_id
                                )
                            })?;
                        if approved {
                            DestinationStatus::Approved { command_id: None }
                        } else {
                            DestinationStatus::Pending
                        }
                    }
                    Some(Phase::Executed) => {
                        let executed = self
                            .client
                            .gateway_is_message_executed(
                                &self.signer_pk,
                                &self.gateway_contract,
                                "axelar",
                                &second_leg.message_id,
                            )
                            .await?
                            .ok_or_else(|| {
                                eyre::eyre!(
                                    "Stellar gateway returned non-bool execution result for tx {}",
                                    tx.message_id
                                )
                            })?;
                        if executed {
                            DestinationStatus::Executed { command_id: None }
                        } else {
                            DestinationStatus::Pending
                        }
                    }
                    _ => DestinationStatus::Pending,
                };
                observations.push(DestinationObservation { index, status });
            }
            Ok(observations)
        })
    }
}

/// XRPL second-leg verifier used by the ITS hub pipeline.
pub(super) struct XrplItsDestinationVerifier {
    client: XrplClient,
    recipient_address: String,
}

impl XrplItsDestinationVerifier {
    pub(super) fn new(rpc_url: &str, recipient_address: String) -> Self {
        Self {
            client: XrplClient::new(rpc_url),
            recipient_address,
        }
    }
}

impl DestinationVerifier for XrplItsDestinationVerifier {
    fn approval_label(&self) -> &str {
        "XRPL approval"
    }

    fn execution_label(&self) -> &str {
        "XRPL execution"
    }

    fn check<'a>(
        &'a self,
        _source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            let mut observations = Vec::with_capacity(indices.len());
            for &index in indices {
                let tx = &txs[index];
                let second_leg = required_second_leg(tx)?;
                let delivered = self
                    .client
                    .find_inbound_with_message_id(
                        &self.recipient_address,
                        &second_leg.message_id,
                        None,
                    )
                    .await?
                    .is_some();
                observations.push(DestinationObservation {
                    index,
                    status: if delivered {
                        DestinationStatus::Executed { command_id: None }
                    } else {
                        DestinationStatus::Pending
                    },
                });
            }
            Ok(observations)
        })
    }
}

/// Sui second-leg verifier used by the ITS hub pipeline.
pub(super) struct SuiItsDestinationVerifier {
    client: SuiClient,
    gateway_pkg: String,
}

impl SuiItsDestinationVerifier {
    pub(super) fn new(rpc_url: &str, gateway_pkg: String) -> Self {
        Self {
            client: SuiClient::new(rpc_url),
            gateway_pkg,
        }
    }
}

impl DestinationVerifier for SuiItsDestinationVerifier {
    fn approval_label(&self) -> &str {
        "Sui approval"
    }

    fn execution_label(&self) -> &str {
        "Sui execution"
    }

    fn check<'a>(
        &'a self,
        _source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            let approved_event_type = format!("{}::events::MessageApproved", self.gateway_pkg);
            let executed_event_type = format!("{}::events::MessageExecuted", self.gateway_pkg);
            let mut observations = Vec::with_capacity(indices.len());
            for &index in indices {
                let tx = &txs[index];
                let second_leg = required_second_leg(tx)?;
                let approved = self
                    .client
                    .has_message_approved(&approved_event_type, "axelar", &second_leg.message_id)
                    .await?;
                let executed = self
                    .client
                    .has_message_executed(&executed_event_type, "axelar", &second_leg.message_id)
                    .await?;
                observations.push(DestinationObservation {
                    index,
                    status: if executed {
                        DestinationStatus::Executed { command_id: None }
                    } else if tx.is_phase(Phase::Approved) && approved {
                        DestinationStatus::Approved { command_id: None }
                    } else {
                        DestinationStatus::Pending
                    },
                });
            }
            Ok(observations)
        })
    }
}

#[derive(Clone, Copy)]
enum SecondLegTarget {
    Routed,
    DestinationApproval,
}

/// Chain-independent ITS hub verifier. It owns the common Cosmos-side state
/// machine while a [`DestinationVerifier`] adapter owns the final-chain
/// approval/execution checks.
struct ItsHubVerifier<'a> {
    lcd: &'a str,
    voting_verifier: Option<&'a str>,
    source_chain: &'a str,
    axelarnet_gateway: &'a str,
    rpc: &'a str,
    destination_gateway: &'a str,
    second_leg_target: SecondLegTarget,
    backfill_voted_timing: bool,
}

type VotingCheckData = (usize, String, String, String);
type MessageCheckData = (usize, String);

struct ItsHubPhaseData {
    voted: Vec<VotingCheckData>,
    hub_approved: Vec<MessageCheckData>,
    routed: Vec<MessageCheckData>,
    voting_destination_chain: String,
    voting_destination_address: String,
}

struct ItsHubPhaseResults {
    voted: Vec<(usize, bool)>,
    hub_approved: Vec<(usize, bool)>,
    routed: Vec<(usize, bool)>,
}

impl ItsHubVerifier<'_> {
    fn normalize_start(&self, tx: &mut PendingTx) {
        if self.voting_verifier.is_none() && tx.is_phase(Phase::Voted) {
            tx.transition_to(Phase::HubApproved);
        }
    }

    fn collect_phase_data(
        &self,
        txs: &[PendingTx],
        phases: &PhaseIndices,
    ) -> Result<ItsHubPhaseData> {
        let voted = phases
            .voted
            .iter()
            .map(|&index| {
                (
                    index,
                    txs[index].message_id.clone().into_string(),
                    txs[index].source_address.clone(),
                    txs[index].payload_hash_hex.clone(),
                )
            })
            .collect::<Vec<_>>();
        let hub_approved = phases
            .hub_approved
            .iter()
            .map(|&index| (index, txs[index].message_id.clone().into_string()))
            .collect::<Vec<_>>();
        let routed = phases
            .routed
            .iter()
            .map(|&index| {
                Ok((
                    index,
                    required_second_leg(&txs[index])?
                        .message_id
                        .clone()
                        .into_string(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        let (voting_destination_chain, voting_destination_address) = voted
            .first()
            .map(|(index, ..)| {
                (
                    txs[*index].gmp_destination_chain.clone(),
                    txs[*index].gmp_destination_address.clone(),
                )
            })
            .unwrap_or_default();

        Ok(ItsHubPhaseData {
            voted,
            hub_approved,
            routed,
            voting_destination_chain,
            voting_destination_address,
        })
    }

    async fn run_phase_checks(&self, data: &ItsHubPhaseData) -> Result<ItsHubPhaseResults> {
        let (voted_results, hub_results, routed_results) = tokio::join!(
            async {
                if data.voted.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(voting_verifier) = self.voting_verifier {
                    batch_check_voting_verifier_owned(
                        self.lcd,
                        voting_verifier,
                        self.source_chain,
                        &data.voting_destination_chain,
                        &data.voting_destination_address,
                        &data.voted,
                    )
                    .await
                } else {
                    Ok(data
                        .voted
                        .iter()
                        .map(|(index, ..)| (*index, true))
                        .collect())
                }
            },
            async {
                if data.hub_approved.is_empty() {
                    return Ok(Vec::new());
                }
                batch_check_hub_approved_owned(
                    self.lcd,
                    self.axelarnet_gateway,
                    self.source_chain,
                    &data.hub_approved,
                )
                .await
            },
            async {
                if data.routed.is_empty() {
                    return Ok(Vec::new());
                }
                batch_check_cosmos_routed_owned(
                    self.lcd,
                    self.destination_gateway,
                    "axelar",
                    &data.routed,
                )
                .await
            },
        );

        Ok(ItsHubPhaseResults {
            voted: voted_results?,
            hub_approved: hub_results?,
            routed: routed_results?,
        })
    }

    fn apply_phase_results(&self, txs: &mut [PendingTx], results: ItsHubPhaseResults) -> bool {
        let mut progressed = false;
        for (index, approved) in results.voted {
            if approved {
                txs[index].timing.voted_secs =
                    Some(txs[index].send_instant.elapsed().as_secs_f64());
                progressed |= txs[index].transition_to(Phase::HubApproved);
            }
        }
        for (index, approved) in results.hub_approved {
            if approved {
                let elapsed = txs[index].send_instant.elapsed().as_secs_f64();
                if self.backfill_voted_timing && txs[index].timing.voted_secs.is_none() {
                    txs[index].timing.voted_secs = Some(elapsed);
                }
                txs[index].timing.hub_approved_secs = Some(elapsed);
                progressed |= txs[index].transition_to(Phase::DiscoverSecondLeg);
            }
        }
        for (index, routed) in results.routed {
            if routed {
                txs[index].timing.routed_secs =
                    Some(txs[index].send_instant.elapsed().as_secs_f64());
                progressed |= txs[index].transition_to(Phase::Approved);
            }
        }
        progressed
    }

    async fn discover_second_legs(&self, txs: &mut [PendingTx], indices: &[usize]) -> Result<bool> {
        let discovery_requests = indices
            .iter()
            .map(|&index| (index, txs[index].message_id.clone().into_string()))
            .collect::<Vec<_>>();
        let discovery_futures =
            discovery_requests
                .into_iter()
                .map(|(index, message_id)| async move {
                    discover_second_leg(self.rpc, &message_id)
                        .await
                        .map(|info| (index, info))
                });
        let discovery_results: Vec<Result<_>> = stream::iter(discovery_futures)
            .buffer_unordered(20)
            .collect()
            .await;
        let mut progressed = false;
        for result in discovery_results {
            let (index, info) = result?;
            if let Some(info) = info {
                let second_leg = SecondLeg {
                    message_id: info.message_id.into(),
                    payload_hash: parse_payload_hash(&info.payload_hash).wrap_err_with(|| {
                        format!(
                            "tx {} has an invalid second-leg payload_hash",
                            txs[index].message_id
                        )
                    })?,
                    source_address: info.source_address,
                    destination_address: info.destination_address,
                };
                progressed |= txs[index].discover_second_leg(
                    second_leg,
                    match self.second_leg_target {
                        SecondLegTarget::Routed => Phase::Routed,
                        SecondLegTarget::DestinationApproval => Phase::Approved,
                    },
                );
            }
        }

        Ok(progressed)
    }

    async fn check(&self, txs: &mut [PendingTx], phases: &PhaseIndices) -> Result<bool> {
        let data = self.collect_phase_data(txs, phases)?;
        let results = self.run_phase_checks(&data).await?;
        let mut progressed = self.apply_phase_results(txs, results);
        progressed |= self
            .discover_second_legs(txs, &phases.discover_second_leg)
            .await?;
        Ok(progressed)
    }
}

fn update_its_spinner(
    spinner: &indicatif::ProgressBar,
    stats: &mut RealTimeStats,
    txs: &[PendingTx],
    total: usize,
) {
    let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
    let routed = txs
        .iter()
        .filter(|tx| tx.timing.routed_secs.is_some())
        .count();
    let counts = [voted, routed, hub_approved, approved, executed];
    stats.update(counts, txs);
    if counts.iter().sum::<usize>() > 0 {
        spinner.set_message(stats.spinner_msg_its(counts, total, None));
    }
}

fn finish_its_display(spinner: &indicatif::ProgressBar, txs: &[PendingTx]) {
    let total = txs.len();
    let (voted, _, hub_approved, approved, executed) = phase_counts(txs);
    let routed = txs
        .iter()
        .filter(|tx| tx.timing.routed_secs.is_some())
        .count();
    spinner.finish_and_clear();
    ui::success(&format!(
        "ITS pipeline: voted: {voted}/{total}  hub: {hub_approved}/{total}  routed: {routed}/{total}  approved: {approved}/{total}  executed: {executed}/{total}"
    ));
}

pub(super) struct PollItsHubArgs<V> {
    pub lcd: String,
    pub voting_verifier: Option<String>,
    pub source_chain: String,
    pub axelarnet_gateway: String,
    pub rpc: String,
    pub cosm_gateway_dest: String,
    pub dest: V,
    /// Network for the final Axelarscan GMP-API recheck on timed-out txs.
    pub network: Network,
}

/// Full ITS polling pipeline: Voted → HubApproved → DiscoverSecondLeg → Routed → Approved → Executed.
pub(super) async fn poll_pipeline_its_hub<V: DestinationVerifier>(
    txs: &mut Vec<PendingTx>,
    mut rx: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
    send_done: Option<&AtomicBool>,
    external_spinner: Option<indicatif::ProgressBar>,
    args: PollItsHubArgs<V>,
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
    let hub_verifier = ItsHubVerifier {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        destination_gateway: cosm_gateway_dest,
        second_leg_target: SecondLegTarget::Routed,
        backfill_voted_timing: false,
    };
    let mut scheduler = PollScheduler::new();
    let mut rt_stats = RealTimeStats::new();

    for tx in txs.iter_mut() {
        hub_verifier.normalize_start(tx);
    }

    loop {
        let action = scheduler.next_action(txs, rx.as_deref_mut(), send_done, |new_tx| {
            hub_verifier.normalize_start(new_tx);
        });
        let (active, total, sending_complete) = match action {
            PollAction::Finished => break,
            PollAction::Waiting => {
                time::sleep(POLL_INTERVAL).await;
                continue;
            }
            PollAction::Ready {
                active,
                total,
                sending_complete,
            } => (active, total, sending_complete),
        };

        let phases = PhaseIndices::collect(txs, &active);
        if hub_verifier.check(txs, &phases).await? {
            scheduler.mark_progress();
        }

        // --- Destination checks (capability adapter + shared transition path) ---
        let destination_indices = phases.destination();
        if !destination_indices.is_empty() {
            let observations = dest.check("axelar", txs, &destination_indices).await?;
            if apply_destination_observations(txs, observations) {
                scheduler.mark_progress();
            }
        }

        update_its_spinner(&spinner, &mut rt_stats, txs, total);

        if scheduler.timed_out(sending_complete) {
            break;
        }
        time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed — but first do an authoritative
    // GMP-API recheck so a slow final leg that executed on-chain is recovered
    // as successful rather than reported as a false timeout.
    finalize_timed_out_txs(txs, network, dest.approval_label(), dest.execution_label()).await;
    finish_its_display(&spinner, txs);

    Ok(compute_peak_throughput(txs))
}

/// EVM destination kind for the ITS-via-hub pipeline. Amplifier destinations
/// have a Cosmos Gateway (the `routed` phase) and verify the second leg via
/// `isMessageApproved`. Legacy (consensus) destinations have neither — the
/// second leg lands as a `ContractCallApproved` event on the legacy gateway
/// (read for its `commandId`, then confirmed via `isCommandExecuted`).
#[derive(Clone, Copy)]
pub(super) enum ItsEvmDest {
    Amplifier,
    Legacy { from_block: u64 },
}

struct EvmItsDestinationVerifier<'a, P: Provider> {
    gw_contract: &'a AxelarAmplifierGateway::AxelarAmplifierGatewayInstance<&'a P>,
    mode: ItsEvmDest,
}

async fn check_evm_its_amplifier<P: Provider>(
    verifier: &EvmItsDestinationVerifier<'_, P>,
    txs: &[PendingTx],
    indices: &[usize],
) -> Result<Vec<DestinationObservation>> {
    let mut futures = Vec::with_capacity(indices.len());
    for &index in indices {
        let tx = &txs[index];
        let phase = tx
            .phase()
            .ok_or_else(|| eyre::eyre!("destination check received an inactive transaction"))?;
        let second_leg = required_second_leg(tx)?.clone();
        let payload_hash = required_second_leg_payload_hash(tx)?;
        futures.push(async move {
            let destination_address: Address =
                second_leg.destination_address.parse().wrap_err_with(|| {
                    format!(
                        "invalid second-leg EVM destination address {}",
                        second_leg.destination_address
                    )
                })?;
            let approved = check_evm_is_message_approved(
                verifier.gw_contract,
                "axelar",
                &second_leg.message_id,
                &second_leg.source_address,
                destination_address,
                payload_hash,
            )
            .await?;
            let executed = check_evm_is_message_executed(
                verifier.gw_contract,
                "axelar",
                &second_leg.message_id,
            )
            .await?;
            let status = match phase {
                Phase::Approved | Phase::Executed if executed => {
                    DestinationStatus::Executed { command_id: None }
                }
                Phase::Approved if approved => DestinationStatus::Approved { command_id: None },
                _ => DestinationStatus::Pending,
            };
            Ok(DestinationObservation { index, status })
        });
    }
    let results: Vec<Result<_>> = stream::iter(futures).buffer_unordered(20).collect().await;
    // Same defense as the GMP EVM branch: a persistent second-leg dest-RPC
    // error (e.g. Hedera 429 rate-limiting, transient retries already
    // exhausted) must NOT abort the run - drop the observation so the tx stays
    // in-flight and `finalize_timed_out_txs`'s GMP-API recheck can recover it if
    // it actually executed on-chain.
    let mut observations = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok(observation) => observations.push(observation),
            Err(error) => ui::warn(&format!(
                "second-leg destination check RPC error (keeping in-flight for GMP-API recheck): \
                 {error}"
            )),
        }
    }
    Ok(observations)
}

async fn check_evm_its_legacy<P: Provider>(
    verifier: &EvmItsDestinationVerifier<'_, P>,
    from_block: u64,
    txs: &[PendingTx],
    indices: &[usize],
) -> Result<Vec<DestinationObservation>> {
    let mut observations = Vec::with_capacity(indices.len());
    for &index in indices {
        let tx = &txs[index];
        // Per-tx tolerance: a destination-RPC failure on one poll cycle must
        // not abort the whole run — report Pending and let the next cycle (or
        // the GMP-API recheck after the inactivity timeout) resolve it.
        let status = match its_legacy_tx_status(verifier, from_block, tx).await {
            Ok(status) => status,
            Err(error) => {
                ui::warn(&format!(
                    "legacy ITS destination check failed for {} (keeping in-flight): {error}",
                    tx.message_id
                ));
                DestinationStatus::Pending
            }
        };
        observations.push(DestinationObservation { index, status });
    }
    Ok(observations)
}

/// Status of one legacy-destination ITS tx for this poll cycle.
async fn its_legacy_tx_status<P: Provider>(
    verifier: &EvmItsDestinationVerifier<'_, P>,
    from_block: u64,
    tx: &PendingTx,
) -> Result<DestinationStatus> {
    let second_leg = required_second_leg(tx)?;
    let destination_address: Address =
        second_leg.destination_address.parse().wrap_err_with(|| {
            format!(
                "invalid second-leg EVM destination address {}",
                second_leg.destination_address
            )
        })?;
    Ok(match tx.phase() {
        Some(Phase::Approved) => {
            let found = legacy::find_contract_call_approved_by_payload(
                verifier.gw_contract.provider(),
                *verifier.gw_contract.address(),
                destination_address,
                required_second_leg_payload_hash(tx)?,
                from_block,
            )
            .await?;
            match found {
                Some(command_id)
                    if check_evm_command_executed(verifier.gw_contract, command_id.into())
                        .await? =>
                {
                    DestinationStatus::Executed {
                        command_id: Some(command_id),
                    }
                }
                Some(command_id) => DestinationStatus::Approved {
                    command_id: Some(command_id),
                },
                None => DestinationStatus::Pending,
            }
        }
        Some(Phase::Executed) => {
            let command_id = tx.command_id().ok_or_else(|| {
                eyre::eyre!(
                    "legacy ITS tx {} in Executed phase without a commandId",
                    tx.message_id
                )
            })?;
            if check_evm_command_executed(verifier.gw_contract, command_id.into()).await? {
                DestinationStatus::Executed {
                    command_id: Some(command_id),
                }
            } else {
                DestinationStatus::Pending
            }
        }
        _ => DestinationStatus::Pending,
    })
}

impl<P: Provider> DestinationVerifier for EvmItsDestinationVerifier<'_, P> {
    fn approval_label(&self) -> &str {
        match self.mode {
            ItsEvmDest::Amplifier => "EVM approval",
            ItsEvmDest::Legacy { .. } => "EVM(legacy) approval",
        }
    }

    fn execution_label(&self) -> &str {
        match self.mode {
            ItsEvmDest::Amplifier => "EVM execution",
            ItsEvmDest::Legacy { .. } => "EVM(legacy) execution",
        }
    }

    fn check<'a>(
        &'a self,
        _source_chain: &'a str,
        txs: &'a [PendingTx],
        indices: &'a [usize],
    ) -> DestinationCheckFuture<'a> {
        Box::pin(async move {
            match self.mode {
                ItsEvmDest::Amplifier => check_evm_its_amplifier(self, txs, indices).await,
                ItsEvmDest::Legacy { from_block } => {
                    check_evm_its_legacy(self, from_block, txs, indices).await
                }
            }
        })
    }
}

pub(super) struct PollItsHubEvmArgs {
    pub lcd: String,
    pub voting_verifier: Option<String>,
    pub source_chain: String,
    pub axelarnet_gateway: String,
    pub rpc: String,
    pub cosm_gateway_dest: String,
    pub dest: ItsEvmDest,
    /// Network for the final Axelarscan GMP-API recheck on timed-out txs.
    pub network: Network,
}

/// Full ITS polling pipeline with EVM destination (batch + streaming):
/// Voted → HubApproved → DiscoverSecondLeg → Routed → Approved(EVM) → Executed(EVM).
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
        dest,
        network,
    } = args;
    let dest_legacy = matches!(dest, ItsEvmDest::Legacy { .. });
    let destination_verifier = EvmItsDestinationVerifier {
        gw_contract,
        mode: dest,
    };
    let lcd = lcd.as_str();
    let voting_verifier = voting_verifier.as_deref();
    let source_chain = source_chain.as_str();
    let axelarnet_gateway = axelarnet_gateway.as_str();
    let rpc = rpc.as_str();
    let cosm_gateway_dest = cosm_gateway_dest.as_str();
    let spinner = external_spinner
        .unwrap_or_else(|| ui::wait_spinner("verifying ITS pipeline (starting)..."));
    let hub_verifier = ItsHubVerifier {
        lcd,
        voting_verifier,
        source_chain,
        axelarnet_gateway,
        rpc,
        destination_gateway: cosm_gateway_dest,
        second_leg_target: if dest_legacy {
            SecondLegTarget::DestinationApproval
        } else {
            SecondLegTarget::Routed
        },
        backfill_voted_timing: true,
    };
    let mut scheduler = PollScheduler::new();
    let mut rt_stats = RealTimeStats::new();

    for tx in txs.iter_mut() {
        hub_verifier.normalize_start(tx);
    }

    loop {
        let action = scheduler.next_action(txs, rx.as_deref_mut(), send_done, |new_tx| {
            hub_verifier.normalize_start(new_tx);
        });
        let (active, total, sending_complete) = match action {
            PollAction::Finished => break,
            PollAction::Waiting => {
                time::sleep(POLL_INTERVAL).await;
                continue;
            }
            PollAction::Ready {
                active,
                total,
                sending_complete,
            } => (active, total, sending_complete),
        };

        let phases = PhaseIndices::collect(txs, &active);
        if hub_verifier.check(txs, &phases).await? {
            scheduler.mark_progress();
        }

        // --- EVM destination checks (capability adapter + shared transition path) ---
        let evm_check_indices = phases.destination();
        if !evm_check_indices.is_empty() {
            let observations = destination_verifier
                .check("axelar", txs, &evm_check_indices)
                .await?;
            if apply_destination_observations(txs, observations) {
                scheduler.mark_progress();
            }
        }

        update_its_spinner(&spinner, &mut rt_stats, txs, total);

        if scheduler.timed_out(sending_complete) {
            break;
        }
        time::sleep(POLL_INTERVAL).await;
    }

    // Mark remaining non-done txs as failed — but first do an authoritative
    // GMP-API recheck so a slow final leg that executed on-chain is recovered
    // as successful rather than reported as a false timeout.
    finalize_timed_out_txs(
        txs,
        network,
        destination_verifier.approval_label(),
        destination_verifier.execution_label(),
    )
    .await;
    finish_its_display(&spinner, txs);

    Ok(compute_peak_throughput(txs))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy::primitives::{Address, FixedBytes};

    use super::super::state::PendingTxInput;
    use super::{
        DestinationObservation, DestinationStatus, PendingTx, Phase,
        apply_destination_observations, mark_timed_out_tx, parse_payload_hash,
    };

    fn pending_at(phase: Phase) -> PendingTx {
        PendingTx::new(PendingTxInput {
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
            initial_phase: phase,
        })
    }

    #[test]
    fn timed_out_tx_reported_executed_is_recovered() {
        // A tx stuck at the timeout that the GMP API confirms executed is
        // reclassified as a successful, recovered tx — never marked failed.
        let mut tx = pending_at(Phase::Executed);
        mark_timed_out_tx(&mut tx, true, "EVM approval", "EVM execution");

        assert!(!tx.is_failed());
        assert!(tx.recovered_via_api());
        assert_eq!(tx.timing.executed_ok, Some(true));
        assert!(tx.timing.executed_secs.is_some());
        assert!(tx.failure_reason().is_none());
    }

    #[test]
    fn timed_out_tx_not_executed_stays_failed() {
        // A genuinely-unexecuted message keeps the original timed-out verdict.
        let mut tx = pending_at(Phase::Approved);
        mark_timed_out_tx(&mut tx, false, "EVM approval", "EVM execution");

        assert!(tx.is_failed());
        assert!(!tx.recovered_via_api());
        assert_eq!(tx.timing.executed_ok, None);
        assert_eq!(tx.failure_reason(), Some("EVM approval: timed out"));
    }

    #[test]
    fn parse_payload_hash_accepts_prefixed_and_unprefixed_hashes() {
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        assert_eq!(
            parse_payload_hash(hash).unwrap().into_fixed_bytes(),
            FixedBytes::from([0xaa; 32])
        );
        assert_eq!(
            parse_payload_hash(&format!("0x{hash}"))
                .unwrap()
                .into_fixed_bytes(),
            FixedBytes::from([0xaa; 32])
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

    #[test]
    fn approved_observation_advances_once_and_preserves_command_id() {
        let mut txs = vec![pending_at(Phase::Approved)];
        let command_id = [7; 32];

        assert!(apply_destination_observations(
            &mut txs,
            vec![DestinationObservation {
                index: 0,
                status: DestinationStatus::Approved {
                    command_id: Some(command_id),
                },
            }],
        ));
        assert_eq!(txs[0].phase(), Some(Phase::Executed));
        assert_eq!(txs[0].command_id(), Some(command_id));
        assert!(txs[0].timing.approved_secs.is_some());
        assert!(txs[0].timing.executed_secs.is_none());
    }

    #[test]
    fn executed_observation_completes_from_approved_or_executed() {
        for phase in [Phase::Approved, Phase::Executed] {
            let mut txs = vec![pending_at(phase)];

            assert!(apply_destination_observations(
                &mut txs,
                vec![DestinationObservation {
                    index: 0,
                    status: DestinationStatus::Executed { command_id: None },
                }],
            ));
            assert!(!txs[0].is_active());
            assert!(!txs[0].is_failed());
            assert_eq!(txs[0].timing.executed_ok, Some(true));
            assert!(txs[0].timing.approved_secs.is_some());
            assert!(txs[0].timing.executed_secs.is_some());
        }
    }

    #[test]
    fn pending_observation_does_not_advance() {
        let mut txs = vec![pending_at(Phase::Approved)];

        assert!(!apply_destination_observations(
            &mut txs,
            vec![DestinationObservation {
                index: 0,
                status: DestinationStatus::Pending,
            }],
        ));
        assert_eq!(txs[0].phase(), Some(Phase::Approved));
        assert!(txs[0].timing.approved_secs.is_none());
    }
}
