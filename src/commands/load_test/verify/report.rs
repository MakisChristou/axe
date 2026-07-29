//! Aggregations applied at the end of a verification run: peak throughput
//! per pipeline step, the rolled-up `VerificationReport`, and the "stuck at
//! X" classification for txs that timed out.

use std::time::Duration;

use super::state::{PendingTx, Phase};
use crate::commands::load_test::metrics::{
    AmplifierTiming, FailureCategory, FailureCategoryKind, PeakThroughput, TxMetrics,
    VerificationReport,
};

/// Compute peak throughput per pipeline step using 5-second sliding windows
/// over the absolute completion timestamps.
/// Compute sustained throughput per pipeline step: count / (last - first) on
/// absolute completion timestamps. The lowest value is the pipeline bottleneck.
pub(super) fn compute_peak_throughput(txs: &[PendingTx]) -> PeakThroughput {
    let Some(epoch) = txs.iter().map(|t| t.send_instant).min() else {
        return PeakThroughput::default();
    };

    let mut voted_times: Vec<f64> = Vec::new();
    let mut routed_times: Vec<f64> = Vec::new();
    let mut hub_approved_times: Vec<f64> = Vec::new();
    let mut approved_times: Vec<f64> = Vec::new();
    let mut executed_times: Vec<f64> = Vec::new();

    for tx in txs {
        let base = tx.send_instant.duration_since(epoch).as_secs_f64();
        if let Some(s) = tx.timing.voted_secs {
            voted_times.push(base + s);
        }
        if let Some(s) = tx.timing.routed_secs {
            routed_times.push(base + s);
        }
        if let Some(s) = tx.timing.hub_approved_secs {
            hub_approved_times.push(base + s);
        }
        if let Some(s) = tx.timing.approved_secs {
            approved_times.push(base + s);
        }
        if let Some(s) = tx.timing.executed_secs {
            executed_times.push(base + s);
        }
    }

    fn sustained_rate(times: &[f64]) -> Option<f64> {
        if times.len() < 2 {
            return None;
        }
        // Phase-drain rate = count / active phase window. The window is
        // first-completion → last-completion, NOT first-submission → last-
        // completion: dividing by the full wall-clock offset (`max`) drags
        // low-volume sustained runs to ~0 because the offset is dominated
        // by per-tx pipeline latency rather than by the inter-completion
        // spread.
        //
        // Burst-mode runs that cross a phase in a single verifier poll
        // would otherwise collapse `(max - min)` to ~0 and produce
        // millions of tx/s, so floor the window at one poll cadence.
        const MIN_WINDOW_SECS: f64 = 5.0;
        let min = times.iter().cloned().reduce(f64::min)?;
        let max = times.iter().cloned().reduce(f64::max)?;
        let window = (max - min).max(MIN_WINDOW_SECS);
        Some(times.len() as f64 / window)
    }

    PeakThroughput {
        voted_tps: sustained_rate(&voted_times),
        routed_tps: sustained_rate(&routed_times),
        hub_approved_tps: sustained_rate(&hub_approved_times),
        approved_tps: sustained_rate(&approved_times),
        executed_tps: sustained_rate(&executed_times),
    }
}

/// Compute the `VerificationReport` from pending tx results, writing timings
/// back into the original metrics array.
pub(super) fn compute_verification_report(
    txs: &[PendingTx],
    metrics: &mut [TxMetrics],
    peak_throughput: PeakThroughput,
) -> VerificationReport {
    let mut successful = 0u64;
    let mut failed = 0u64;
    let mut failure_reasons: std::collections::HashMap<(String, FailureCategoryKind), u64> =
        std::collections::HashMap::new();
    let mut stuck_count = 0u64;
    let mut stuck_phases: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut recovered_via_api = 0u64;

    for tx in txs {
        if tx.idx < metrics.len() {
            metrics[tx.idx].amplifier_timing = Some(tx.timing.clone());
        }
        if tx.recovered_via_api() {
            recovered_via_api += 1;
        }
        if tx.is_failed() {
            failed += 1;
            if let Some(reason) = tx.failure_reason() {
                let kind = if tx.is_timed_out() {
                    FailureCategoryKind::TimedOut
                } else {
                    FailureCategoryKind::Error
                };
                *failure_reasons
                    .entry((reason.to_string(), kind))
                    .or_insert(0) += 1;

                // Categorize stuck txs by the phase they got stuck at
                if tx.is_timed_out() {
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
        .map(|((reason, kind), count)| FailureCategory {
            reason,
            count,
            kind,
        })
        .collect();

    let stuck_at: Vec<FailureCategory> = stuck_phases
        .into_iter()
        .map(|(reason, count)| FailureCategory {
            reason,
            count,
            kind: FailureCategoryKind::TimedOut,
        })
        .collect();

    let all_timings: Vec<&AmplifierTiming> = txs.iter().map(|t| &t.timing).collect();
    let avg_voted = avg_option(all_timings.iter().filter_map(|t| t.voted_secs));
    let avg_routed = avg_option(all_timings.iter().filter_map(|t| t.routed_secs));
    let avg_hub_approved = avg_option(all_timings.iter().filter_map(|t| t.hub_approved_secs));
    let avg_approved = avg_option(all_timings.iter().filter_map(|t| t.approved_secs));
    let avg_executed = avg_option(all_timings.iter().filter_map(|t| t.executed_secs));
    let min_executed = min_option(all_timings.iter().filter_map(|t| t.executed_secs));
    let max_executed = max_option(all_timings.iter().filter_map(|t| t.executed_secs));

    // Time from earliest send to last successful execution (for throughput).
    // This excludes timeout wait for stuck txs.
    let earliest_send = txs.iter().map(|tx| tx.send_instant).min();
    let last_execution = txs
        .iter()
        .filter(|tx| tx.timing.executed_ok == Some(true))
        .filter_map(|tx| {
            let secs = tx.timing.executed_secs?;
            Some(tx.send_instant + Duration::from_secs_f64(secs))
        })
        .max();
    let time_to_last_success = match (earliest_send, last_execution) {
        (Some(start), Some(end)) if end > start => Some(end.duration_since(start).as_secs_f64()),
        _ => None,
    };

    VerificationReport {
        total_verified,
        successful,
        pending: 0,
        failed,
        success_rate,
        failure_reasons: failure_categories,
        avg_voted_secs: avg_voted,
        avg_routed_secs: avg_routed,
        avg_hub_approved_secs: avg_hub_approved,
        avg_approved_secs: avg_approved,
        avg_executed_secs: avg_executed,
        min_executed_secs: min_executed,
        max_executed_secs: max_executed,
        time_to_last_success_secs: time_to_last_success,
        peak_throughput,
        stuck: stuck_count,
        stuck_at,
        recovered_via_api,
    }
}

/// Determine which phase a timed-out tx got stuck at (the last phase it didn't complete).
fn stuck_phase(tx: &PendingTx) -> String {
    match tx.phase() {
        Some(Phase::Voted) => "voted".into(),
        Some(Phase::Routed) => "routed".into(),
        Some(Phase::HubApproved) => "hub approved".into(),
        Some(Phase::DiscoverSecondLeg) => "second-leg discovery".into(),
        Some(Phase::Approved) => "approved".into(),
        Some(Phase::Executed) => "executed".into(),
        None => "unknown".into(),
    }
}

fn avg_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    let vals: Vec<f64> = iter.collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }
}

fn min_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.reduce(f64::min)
}

fn max_option(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.reduce(f64::max)
}
