//! Per-tx state types tracked during batch verification:
//! [`PendingTx`], the [`Phase`] enum that drives state transitions, and
//! [`RealTimeStats`] which keeps the spinner display fed with rolling
//! throughput + latency numbers.

use std::time::Instant;

use alloy::primitives::Address;

use super::THROUGHPUT_WINDOW;
use crate::commands::load_test::identifiers::{MessageId, PayloadHash};
use crate::commands::load_test::metrics::AmplifierTiming;
use crate::ui;

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
    Succeeded {
        recovered_via_api: bool,
    },
    Failed {
        phase: Phase,
        failure: VerificationFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum VerificationFailure {
    #[cfg(test)]
    Error(String),
    TimedOut(String),
}

impl VerificationFailure {
    fn reason(&self) -> &str {
        match self {
            #[cfg(test)]
            Self::Error(reason) => reason,
            Self::TimedOut(reason) => reason,
        }
    }
}

/// Fully-discovered ITS hub→destination leg. Keeping these fields together
/// prevents later stages from observing only a partial discovery result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SecondLeg {
    pub(super) message_id: MessageId,
    pub(super) payload_hash: PayloadHash,
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
    pub(super) message_id: MessageId,
    pub(super) send_instant: Instant,
    pub(super) source_address: String,
    pub(super) contract_addr: Address,
    pub(super) payload_hash: Option<PayloadHash>,
    pub(super) payload_hash_hex: String,
    /// Pre-computed command ID for Solana destination checks.
    command_id: Option<[u8; 32]>,
    /// GMP-level destination chain from ContractCall event (e.g. "axelar" for ITS).
    pub(super) gmp_destination_chain: String,
    /// GMP-level destination address from ContractCall event (e.g. ITS Hub contract).
    pub(super) gmp_destination_address: String,
    pub(super) timing: AmplifierTiming,
    state: VerificationState,
    /// Populated atomically when the ITS hub→destination leg is discovered.
    second_leg: Option<SecondLeg>,
}

/// Owned initialization data for [`PendingTx`]. Runtime-only timing and
/// terminal state are deliberately absent so every transaction starts with
/// identical defaults.
pub(super) struct PendingTxInput {
    pub idx: usize,
    pub message_id: MessageId,
    pub send_instant: Instant,
    pub source_address: String,
    pub contract_addr: Address,
    pub payload_hash: Option<PayloadHash>,
    pub payload_hash_hex: String,
    pub command_id: Option<[u8; 32]>,
    pub gmp_destination_chain: String,
    pub gmp_destination_address: String,
    pub initial_phase: Phase,
}

impl PendingTx {
    pub(super) fn new(input: PendingTxInput) -> Self {
        let PendingTxInput {
            idx,
            message_id,
            send_instant,
            source_address,
            contract_addr,
            payload_hash,
            payload_hash_hex,
            command_id,
            gmp_destination_chain,
            gmp_destination_address,
            initial_phase,
        } = input;
        Self {
            idx,
            message_id,
            send_instant,
            source_address,
            contract_addr,
            payload_hash,
            payload_hash_hex,
            command_id,
            gmp_destination_chain,
            gmp_destination_address,
            timing: AmplifierTiming::default(),
            state: VerificationState::Active(initial_phase),
            second_leg: None,
        }
    }

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

    /// Advance to `phase`, ignoring the request if it is not a legal step from
    /// the current state.
    ///
    /// Returns whether the transition was applied. Rejections are reported but
    /// never panic: verification runs inside a spawned task, so an unforeseen
    /// state sequence must not abort a load test that has already spent real
    /// funds. A tx left behind by a rejected transition keeps polling and is
    /// resolved by the inactivity-timeout GMP-API recheck.
    pub(super) fn transition_to(&mut self, phase: Phase) -> bool {
        let VerificationState::Active(current) = self.state else {
            self.reject_transition(&format!("transition to {phase:?}"));
            return false;
        };
        if !current.can_transition_to(phase) {
            self.reject_transition(&format!("transition from {current:?} to {phase:?}"));
            return false;
        }
        self.state = VerificationState::Active(phase);
        true
    }

    /// Install the fully parsed second leg and advance in one operation.
    ///
    /// A failed transition leaves both the phase and discovery data unchanged,
    /// so callers cannot expose a half-applied hub discovery.
    pub(super) fn discover_second_leg(&mut self, second_leg: SecondLeg, phase: Phase) -> bool {
        let VerificationState::Active(current) = self.state else {
            self.reject_transition("install second leg");
            return false;
        };
        if current != Phase::DiscoverSecondLeg || !current.can_transition_to(phase) {
            self.reject_transition(&format!(
                "install second leg and transition from {current:?} to {phase:?}"
            ));
            return false;
        }
        self.second_leg = Some(second_leg);
        self.state = VerificationState::Active(phase);
        true
    }

    pub(super) fn second_leg(&self) -> Option<&SecondLeg> {
        self.second_leg.as_ref()
    }

    pub(super) fn command_id(&self) -> Option<[u8; 32]> {
        self.command_id
    }

    /// Record destination approval and atomically advance to execution polling.
    pub(super) fn approve_destination(&mut self, command_id: Option<[u8; 32]>) -> bool {
        if !self.is_phase(Phase::Approved) {
            self.reject_transition("record destination approval");
            return false;
        }
        self.command_id = command_id.or(self.command_id);
        self.state = VerificationState::Active(Phase::Executed);
        true
    }

    /// Record destination execution and settle the transaction.
    pub(super) fn execute_destination(&mut self, command_id: Option<[u8; 32]>) -> bool {
        if !matches!(
            self.state,
            VerificationState::Active(Phase::Approved | Phase::Executed)
        ) {
            self.reject_transition("record destination execution");
            return false;
        }
        self.command_id = command_id.or(self.command_id);
        self.state = VerificationState::Succeeded {
            recovered_via_api: false,
        };
        true
    }

    /// Mark the tx successful. Ignored if it already reached a terminal state.
    pub(super) fn succeed(&mut self, recovered_via_api: bool) -> bool {
        if !self.is_active() {
            self.reject_transition("mark successful");
            return false;
        }
        self.state = VerificationState::Succeeded { recovered_via_api };
        true
    }

    /// Mark the tx failed. Ignored if it already reached a terminal state, so
    /// the first recorded reason wins.
    #[cfg(test)]
    pub(super) fn fail(&mut self, reason: String) -> bool {
        self.fail_with(VerificationFailure::Error(reason))
    }

    pub(super) fn time_out(&mut self, label: &str) -> bool {
        self.fail_with(VerificationFailure::TimedOut(format!("{label}: timed out")))
    }

    fn fail_with(&mut self, failure: VerificationFailure) -> bool {
        let VerificationState::Active(phase) = self.state else {
            self.reject_transition("mark failed");
            return false;
        };
        self.state = VerificationState::Failed { phase, failure };
        true
    }

    fn reject_transition(&self, attempted: &str) {
        ui::warn(&format!(
            "verification: ignoring {attempted} for {} — already {}",
            self.message_id,
            match &self.state {
                VerificationState::Active(phase) => format!("in phase {phase:?}"),
                VerificationState::Succeeded { .. } => "successful".to_string(),
                VerificationState::Failed { failure, .. } => {
                    format!("failed ({})", failure.reason())
                }
            }
        ));
    }

    pub(super) fn is_failed(&self) -> bool {
        matches!(self.state, VerificationState::Failed { .. })
    }

    pub(super) fn failure_reason(&self) -> Option<&str> {
        match &self.state {
            VerificationState::Failed { failure, .. } => Some(failure.reason()),
            VerificationState::Active(_) | VerificationState::Succeeded { .. } => None,
        }
    }

    pub(super) fn is_timed_out(&self) -> bool {
        matches!(
            self.state,
            VerificationState::Failed {
                failure: VerificationFailure::TimedOut(_),
                ..
            }
        )
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

    use alloy::primitives::{Address, FixedBytes};

    use super::{PendingTx, PendingTxInput, Phase, SecondLeg};
    use crate::commands::load_test::identifiers::PayloadHash;

    fn pending(phase: Phase) -> PendingTx {
        PendingTx::new(PendingTxInput {
            idx: 0,
            message_id: "message-1".into(),
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
            assert!(tx.transition_to(to));
            assert!(tx.is_phase(to));
        }
    }

    #[test]
    fn rejects_out_of_order_transition_without_panicking() {
        let mut tx = pending(Phase::Voted);

        assert!(!tx.transition_to(Phase::Executed));
        assert!(tx.is_phase(Phase::Voted));
    }

    #[test]
    fn keeps_the_first_terminal_verdict() {
        let mut tx = pending(Phase::Approved);
        assert!(tx.time_out("approval"));

        assert!(!tx.succeed(true));
        assert!(!tx.transition_to(Phase::Executed));
        assert!(!tx.fail("something else".to_string()));
        assert_eq!(tx.failure_reason(), Some("approval: timed out"));
        assert!(tx.is_timed_out());
    }

    #[test]
    fn second_leg_discovery_is_atomic_and_phase_checked() {
        let second_leg = SecondLeg {
            message_id: "second-leg".into(),
            payload_hash: PayloadHash::from(FixedBytes::from([3; 32])),
            source_address: "hub".to_string(),
            destination_address: "destination".to_string(),
        };
        let mut wrong_phase = pending(Phase::HubApproved);

        assert!(!wrong_phase.discover_second_leg(second_leg.clone(), Phase::Routed));
        assert!(wrong_phase.second_leg().is_none());
        assert!(wrong_phase.is_phase(Phase::HubApproved));

        let mut discovering = pending(Phase::DiscoverSecondLeg);
        assert!(discovering.discover_second_leg(second_leg.clone(), Phase::Routed));
        assert_eq!(discovering.second_leg(), Some(&second_leg));
        assert!(discovering.is_phase(Phase::Routed));
    }

    #[test]
    fn destination_observations_preserve_command_id_across_state_changes() {
        let command_id = [9; 32];
        let mut tx = pending(Phase::Approved);

        assert!(tx.approve_destination(Some(command_id)));
        assert_eq!(tx.command_id(), Some(command_id));
        assert!(tx.is_phase(Phase::Executed));

        assert!(tx.execute_destination(None));
        assert_eq!(tx.command_id(), Some(command_id));
        assert!(!tx.is_active());
    }
}
