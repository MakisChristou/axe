//! Per-tx state types tracked during batch verification:
//! [`PendingTx`], the [`Phase`] enum that drives state transitions, and
//! [`RealTimeStats`] which keeps the spinner display fed with rolling
//! throughput + latency numbers.

use std::time::Instant;

use alloy::primitives::{Address, FixedBytes};

use super::THROUGHPUT_WINDOW;
use crate::commands::load_test::metrics::AmplifierTiming;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Voted,
    Routed,
    HubApproved,
    DiscoverSecondLeg,
    Approved,
    Executed,
}

impl Phase {
    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Voted,
                Self::Routed | Self::HubApproved | Self::Approved
            ) | (Self::Routed, Self::HubApproved | Self::Approved)
                | (Self::HubApproved, Self::DiscoverSecondLeg | Self::Approved)
                | (Self::DiscoverSecondLeg, Self::Routed | Self::Approved)
                | (Self::Approved, Self::Executed)
        )
    }
}

/// Terminal-aware state for a transaction under verification. A transaction
/// cannot be both failed and successful, and terminal states cannot carry an
/// unrelated active phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum VerificationState {
    Active(Phase),
    Succeeded { recovered_via_api: bool },
    Failed { phase: Phase, reason: String },
}

/// Fully-discovered ITS hub→destination leg. Keeping these fields together
/// prevents later stages from observing only a partial discovery result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SecondLeg {
    pub(super) message_id: String,
    pub(super) payload_hash: String,
    pub(super) source_address: String,
    pub(super) destination_address: String,
}

/// Per-tx state tracked during batch verification.
///
/// Visibility note: the struct is `pub(in crate::commands::load_test)` (one
/// level above `verify/`) because `verify::tx_to_pending_*` constructors
/// hand `PendingTx` instances back to the per-pair load-test runners that
/// own the verifier `mpsc::Sender`. Fields stay `pub(super)` so only
/// `verify/`-internal code reads them.
pub(in crate::commands::load_test) struct PendingTx {
    pub(super) idx: usize,
    pub(super) message_id: String,
    pub(super) send_instant: Instant,
    pub(super) source_address: String,
    pub(super) contract_addr: Address,
    pub(super) payload_hash: Option<FixedBytes<32>>,
    pub(super) payload_hash_hex: String,
    /// Pre-computed command ID for Solana destination checks.
    pub(super) command_id: Option<[u8; 32]>,
    /// GMP-level destination chain from ContractCall event (e.g. "axelar" for ITS).
    pub(super) gmp_destination_chain: String,
    /// GMP-level destination address from ContractCall event (e.g. ITS Hub contract).
    pub(super) gmp_destination_address: String,
    pub(super) timing: AmplifierTiming,
    pub(super) state: VerificationState,
    /// Populated atomically when the ITS hub→destination leg is discovered.
    pub(super) second_leg: Option<SecondLeg>,
}

impl PendingTx {
    pub(super) fn phase(&self) -> Option<Phase> {
        match self.state {
            VerificationState::Active(phase) | VerificationState::Failed { phase, .. } => {
                Some(phase)
            }
            VerificationState::Succeeded { .. } => None,
        }
    }

    pub(super) fn is_phase(&self, phase: Phase) -> bool {
        self.state == VerificationState::Active(phase)
    }

    pub(super) fn is_active(&self) -> bool {
        matches!(self.state, VerificationState::Active(_))
    }

    pub(super) fn transition_to(&mut self, phase: Phase) {
        let current = self
            .phase()
            .expect("cannot transition a successful transaction");
        assert!(
            self.is_active(),
            "cannot transition a failed transaction from {current:?} to {phase:?}"
        );
        assert!(
            current.can_transition_to(phase),
            "invalid verification transition from {current:?} to {phase:?}"
        );
        self.state = VerificationState::Active(phase);
    }

    pub(super) fn succeed(&mut self, recovered_via_api: bool) {
        assert!(
            self.is_active(),
            "only an active transaction can be marked successful"
        );
        self.state = VerificationState::Succeeded { recovered_via_api };
    }

    pub(super) fn fail(&mut self, reason: String) {
        assert!(
            self.is_active(),
            "only an active transaction can be marked failed"
        );
        let phase = self.phase().expect("active transaction must have a phase");
        self.state = VerificationState::Failed { phase, reason };
    }

    pub(super) fn is_failed(&self) -> bool {
        matches!(self.state, VerificationState::Failed { .. })
    }

    pub(super) fn failure_reason(&self) -> Option<&str> {
        match &self.state {
            VerificationState::Failed { reason, .. } => Some(reason),
            VerificationState::Active(_) | VerificationState::Succeeded { .. } => None,
        }
    }

    pub(super) fn recovered_via_api(&self) -> bool {
        matches!(
            self.state,
            VerificationState::Succeeded {
                recovered_via_api: true
            }
        )
    }
}

/// Real-time stats (throughput + latency) for spinner display.
pub(super) struct RealTimeStats {
    snapshot_time: Instant,
    snapshot_counts: [usize; 5], // voted, routed, hub_approved, approved, executed
    throughputs: [Option<f64>; 5],
    latencies: Vec<f64>, // sorted executed_secs for completed txs
}

impl RealTimeStats {
    pub(super) fn new() -> Self {
        Self {
            snapshot_time: Instant::now(),
            snapshot_counts: [0; 5],
            throughputs: [None; 5],
            latencies: Vec::new(),
        }
    }

    /// Update throughputs every THROUGHPUT_WINDOW and collect new latencies.
    pub(super) fn update(&mut self, counts: [usize; 5], txs: &[PendingTx]) {
        let elapsed = self.snapshot_time.elapsed();
        if elapsed >= THROUGHPUT_WINDOW {
            let secs = elapsed.as_secs_f64();
            for (i, &count) in counts.iter().enumerate() {
                let delta = count.saturating_sub(self.snapshot_counts[i]);
                self.throughputs[i] = if delta > 0 {
                    Some(delta as f64 / secs)
                } else {
                    self.throughputs[i] // keep last known value
                };
            }
            self.snapshot_counts = counts;
            self.snapshot_time = Instant::now();
        }

        // Rebuild latencies from all completed txs (simple and correct).
        let new_len = txs
            .iter()
            .filter(|t| t.timing.executed_secs.is_some())
            .count();
        if new_len != self.latencies.len() {
            self.latencies.clear();
            for tx in txs {
                if let Some(secs) = tx.timing.executed_secs {
                    self.latencies.push(secs);
                }
            }
            self.latencies
                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }
    }

    /// Format a single phase: "450/600(4.2/s)" or "450/600" if no throughput yet.
    fn fmt_phase(count: usize, total: usize, tps: Option<f64>) -> String {
        match tps {
            Some(t) => format!("{count}/{total}({t:.1}/s)"),
            None => format!("{count}/{total}"),
        }
    }

    /// Format latency summary: "e2e: avg 94.5s p50 92.1s p75 96.3s p99 102.1s"
    fn fmt_latency(&self) -> String {
        let n = self.latencies.len();
        if n == 0 {
            return String::new();
        }
        let sum: f64 = self.latencies.iter().sum();
        let avg = sum / n as f64;
        let pct = |p: f64| -> f64 {
            let idx = ((n as f64 * p) as usize).min(n - 1);
            self.latencies[idx]
        };
        let min = self.latencies[0];
        let max = self.latencies[n - 1];
        format!(
            " | e2e: avg {avg:.1}s p50 {:.1}s p75 {:.1}s p99 {:.1}s min {min:.1}s max {max:.1}s",
            pct(0.50),
            pct(0.75),
            pct(0.99),
        )
    }

    /// Build the full spinner message for GMP (no hub phase).
    pub(super) fn spinner_msg_gmp(
        &self,
        counts: [usize; 5],
        total: usize,
        err: Option<&str>,
        has_voting_verifier: bool,
        has_routed_phase: bool,
    ) -> String {
        let [voted, routed, _, approved, executed] = counts;
        let [tv, tr, _, ta, te] = self.throughputs;
        let mut parts = Vec::new();
        if has_voting_verifier {
            parts.push(format!("voted: {}", Self::fmt_phase(voted, total, tv)));
        }
        if has_routed_phase {
            parts.push(format!("routed: {}", Self::fmt_phase(routed, total, tr)));
        }
        parts.push(format!(
            "approved: {}",
            Self::fmt_phase(approved, total, ta)
        ));
        parts.push(format!(
            "executed: {}",
            Self::fmt_phase(executed, total, te)
        ));
        let mut msg = parts.join("  ");
        msg.push_str(&self.fmt_latency());
        if let Some(e) = err {
            msg.push_str(&format!("  (err: {e})"));
        }
        msg
    }

    /// Build the full spinner message for ITS (with hub phase).
    pub(super) fn spinner_msg_its(
        &self,
        counts: [usize; 5],
        total: usize,
        err: Option<&str>,
    ) -> String {
        let [voted, routed, hub, approved, executed] = counts;
        let [tv, tr, th, ta, te] = self.throughputs;
        let mut msg = format!(
            "voted: {}  hub: {}  routed: {}  approved: {}  executed: {}",
            Self::fmt_phase(voted, total, tv),
            Self::fmt_phase(hub, total, th),
            Self::fmt_phase(routed, total, tr),
            Self::fmt_phase(approved, total, ta),
            Self::fmt_phase(executed, total, te),
        );
        msg.push_str(&self.fmt_latency());
        if let Some(e) = err {
            msg.push_str(&format!("  (err: {e})"));
        }
        msg
    }
}

/// Count how many txs have each phase's timing populated (voted, routed,
/// hub_approved, approved, executed). Used by both the spinner refresh and
/// the report's "stuck at X" diagnostics.
pub(super) fn phase_counts(txs: &[PendingTx]) -> (usize, usize, usize, usize, usize) {
    let mut voted = 0;
    let mut routed = 0;
    let mut hub_approved = 0;
    let mut approved = 0;
    let mut executed = 0;
    for tx in txs {
        if tx.timing.voted_secs.is_some() {
            voted += 1;
        }
        if tx.timing.routed_secs.is_some() {
            routed += 1;
        }
        if tx.timing.hub_approved_secs.is_some() {
            hub_approved += 1;
        }
        if tx.timing.approved_secs.is_some() {
            approved += 1;
        }
        if tx.timing.executed_secs.is_some() {
            executed += 1;
        }
    }
    (voted, routed, hub_approved, approved, executed)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alloy::primitives::Address;

    use super::{PendingTx, Phase, VerificationState};
    use crate::commands::load_test::metrics::AmplifierTiming;

    fn pending(phase: Phase) -> PendingTx {
        PendingTx {
            idx: 0,
            message_id: "message-1".to_string(),
            send_instant: Instant::now(),
            source_address: String::new(),
            contract_addr: Address::ZERO,
            payload_hash: None,
            payload_hash_hex: String::new(),
            command_id: None,
            gmp_destination_chain: String::new(),
            gmp_destination_address: String::new(),
            timing: AmplifierTiming::default(),
            state: VerificationState::Active(phase),
            second_leg: None,
        }
    }

    #[test]
    fn accepts_each_supported_pipeline_branch() {
        for (from, to) in [
            (Phase::Voted, Phase::Routed),
            (Phase::Voted, Phase::HubApproved),
            (Phase::Voted, Phase::Approved),
            (Phase::Routed, Phase::HubApproved),
            (Phase::Routed, Phase::Approved),
            (Phase::HubApproved, Phase::DiscoverSecondLeg),
            (Phase::HubApproved, Phase::Approved),
            (Phase::DiscoverSecondLeg, Phase::Routed),
            (Phase::DiscoverSecondLeg, Phase::Approved),
            (Phase::Approved, Phase::Executed),
        ] {
            let mut tx = pending(from);
            tx.transition_to(to);
            assert!(tx.is_phase(to));
        }
    }

    #[test]
    #[should_panic(expected = "invalid verification transition")]
    fn rejects_out_of_order_transition() {
        pending(Phase::Voted).transition_to(Phase::Executed);
    }
}
