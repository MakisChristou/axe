use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use alloy::primitives::U256;
use indicatif::ProgressBar;
use serde::Serialize;

use super::super::IntentRuntimeArgs;
use super::super::client::RfqClient;
use super::super::types::{HumanAmount, LegPlan};
use crate::evm::pipeline::PipelinedSender;
use crate::shutdown::Shutdown;

pub struct StressArgs {
    pub runtime: IntentRuntimeArgs,
    pub symbol: String,
    pub amount: HumanAmount,
    pub duration: Option<Duration>,
    pub max_intents: Option<u64>,
    pub max_in_flight: usize,
    pub max_volume: Option<HumanAmount>,
    pub max_native_spend: Option<HumanAmount>,
    pub min_native_balance: HumanAmount,
    pub json: bool,
}

#[derive(Default)]
pub(super) struct StressTelemetry {
    pub warnings: Arc<AtomicU64>,
}

pub(super) struct RateSample {
    pub elapsed: Duration,
    pub broadcast: u64,
    pub confirmed: u64,
}

pub(super) struct StressProgress {
    pub bar: ProgressBar,
    pub limits: StressLimits,
    pub telemetry: Arc<StressTelemetry>,
    pub samples: VecDeque<RateSample>,
}

#[derive(Clone, Debug)]
pub(super) struct StressLimits {
    pub duration: Option<Duration>,
    pub max_intents: Option<u64>,
    pub max_in_flight: usize,
    pub amount: U256,
    pub max_volume: Option<U256>,
    pub max_native_spend: Option<U256>,
    pub min_native_balance: U256,
    pub decimals: u8,
    pub symbol: String,
}

impl StressLimits {
    pub fn approval_amount(&self) -> U256 {
        let count_volume = self
            .max_intents
            .map(|count| U256::from(count).saturating_mul(self.amount));
        self.max_volume
            .into_iter()
            .chain(count_volume)
            .min()
            .unwrap_or(U256::MAX)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StopReason {
    Duration,
    MaxIntents,
    MaxVolume,
    Interrupted,
    SourcesStopped,
}

impl StopReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Duration => "duration reached",
            Self::MaxIntents => "deposit cap reached",
            Self::MaxVolume => "volume cap reached",
            Self::Interrupted => "interrupted",
            Self::SourcesStopped => "all source chains stopped",
        }
    }
}

pub(super) enum DepositOutcome {
    Confirmed(DepositRecord),
    Skipped(String),
    Failed(String),
}

pub(super) struct TaskCompletion {
    pub source: usize,
    pub outcome: DepositOutcome,
}

#[derive(Serialize)]
pub(super) struct DepositRecord {
    pub quote_id: String,
    pub transaction_hash: String,
    pub quote_latency_ms: u64,
    pub deposit_latency_ms: u64,
}

#[derive(Default, Serialize)]
pub(super) struct SourceReport {
    pub chain: String,
    pub broadcast: u64,
    pub confirmed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub gas_spent: String,
    pub last_issue: Option<String>,
}

pub(super) struct SourceState {
    pub routes: Vec<LegPlan>,
    pub sender: Arc<PipelinedSender>,
    pub cursor: usize,
    pub ready_at: Instant,
    pub report: SourceReport,
}

pub(super) struct SchedulerArgs {
    pub client: RfqClient,
    pub sources: Vec<SourceState>,
    pub wallet: alloy::primitives::Address,
    pub limits: StressLimits,
    pub shutdown: Arc<Shutdown>,
    pub progress: ProgressBar,
    pub telemetry: Arc<StressTelemetry>,
}

pub(super) struct DepositTask {
    pub client: RfqClient,
    pub wallet: alloy::primitives::Address,
    pub source: usize,
    pub sender: Arc<PipelinedSender>,
    pub plan: LegPlan,
    pub telemetry: Arc<StressTelemetry>,
}

pub(super) struct StressRun {
    pub stop_reason: StopReason,
    pub state: AdmissionState,
    pub warnings: u64,
    pub records: Vec<DepositRecord>,
    pub sources: Vec<SourceReport>,
}

pub(super) struct AdmissionState {
    pub started: Instant,
    pub attempts: u64,
    pub committed: u64,
    pub broadcast: u64,
    pub confirmed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub active: usize,
    pub peak_active: usize,
}

impl AdmissionState {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            attempts: 0,
            committed: 0,
            broadcast: 0,
            confirmed: 0,
            skipped: 0,
            failed: 0,
            active: 0,
            peak_active: 0,
        }
    }

    pub fn permanent_stop(&self, limits: &StressLimits) -> Option<StopReason> {
        if limits.max_intents.is_some_and(|cap| self.committed >= cap) {
            Some(StopReason::MaxIntents)
        } else if limits.max_volume.is_some_and(|cap| {
            U256::from(self.committed.saturating_add(1)).saturating_mul(limits.amount) > cap
        }) {
            Some(StopReason::MaxVolume)
        } else {
            None
        }
    }

    pub fn can_admit(&self, limits: &StressLimits) -> bool {
        let reserved = self
            .committed
            .saturating_add(self.active as u64)
            .saturating_add(1);
        self.active < limits.max_in_flight
            && limits.max_intents.is_none_or(|cap| reserved <= cap)
            && limits
                .max_volume
                .is_none_or(|cap| U256::from(reserved).saturating_mul(limits.amount) <= cap)
    }

    pub fn admit(&mut self) {
        self.attempts += 1;
        self.active += 1;
        self.peak_active = self.peak_active.max(self.active);
    }

    pub fn complete(&mut self, outcome: &DepositOutcome) {
        self.active = self.active.saturating_sub(1);
        match outcome {
            DepositOutcome::Skipped(_) => self.skipped += 1,
            DepositOutcome::Confirmed(_) => {
                self.confirmed += 1;
                self.committed += 1;
            }
            DepositOutcome::Failed(_) => {
                self.failed += 1;
                self.committed += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn limits() -> StressLimits {
        StressLimits {
            duration: Some(Duration::from_secs(60)),
            max_intents: Some(5),
            max_in_flight: 3,
            amount: U256::from(10),
            max_volume: Some(U256::from(50)),
            max_native_spend: Some(U256::from(100)),
            min_native_balance: U256::ZERO,
            decimals: 6,
            symbol: "USDC".to_owned(),
        }
    }

    #[test]
    fn odd_limits_allow_independent_deposits_and_reserve_pending_volume() {
        let limits = limits();
        let mut state = AdmissionState::new();
        for _ in 0..3 {
            assert!(state.can_admit(&limits));
            state.admit();
        }
        assert!(!state.can_admit(&limits));
        assert_eq!(state.active, 3);
    }

    #[test]
    fn skipped_quotes_release_budgets_without_becoming_losses_or_failures() {
        let limits = limits();
        let mut state = AdmissionState::new();
        state.admit();
        state.complete(&DepositOutcome::Skipped("no quote".to_owned()));
        assert_eq!(state.committed, 0);
        assert_eq!(state.failed, 0);
        assert_eq!(state.skipped, 1);
        assert!(state.can_admit(&limits));
        for _ in 0..5 {
            state.admit();
            state.complete(&DepositOutcome::Failed("broadcast uncertain".to_owned()));
        }
        assert_eq!(state.permanent_stop(&limits), Some(StopReason::MaxIntents));
        assert!(!state.can_admit(&limits));
    }

    #[test]
    fn continuous_runs_still_enforce_deposit_and_volume_caps() {
        let mut limits = limits();
        limits.duration = None;
        limits.max_volume = None;
        let mut state = AdmissionState::new();
        state.committed = 5;
        assert_eq!(state.permanent_stop(&limits), Some(StopReason::MaxIntents));
        assert!(!state.can_admit(&limits));

        limits.max_intents = None;
        limits.max_volume = Some(U256::from(50));
        assert_eq!(state.permanent_stop(&limits), Some(StopReason::MaxVolume));
        assert!(!state.can_admit(&limits));
    }

    #[test]
    fn uncapped_runs_admit_past_default_budgets_but_keep_concurrency_bounded() {
        let mut limits = limits();
        limits.max_intents = None;
        limits.max_volume = None;
        let mut state = AdmissionState::new();
        state.committed = 100_000;
        assert_eq!(state.permanent_stop(&limits), None);
        for _ in 0..limits.max_in_flight {
            assert!(state.can_admit(&limits));
            state.admit();
        }
        assert!(!state.can_admit(&limits));
    }

    #[test]
    fn approvals_follow_the_tightest_explicit_input_cap() {
        let mut limits = limits();
        assert_eq!(limits.approval_amount(), U256::from(50));
        limits.max_intents = Some(2);
        assert_eq!(limits.approval_amount(), U256::from(20));
        limits.max_volume = None;
        assert_eq!(limits.approval_amount(), U256::from(20));
        limits.max_intents = None;
        assert_eq!(limits.approval_amount(), U256::MAX);
        limits.max_volume = Some(U256::from(30));
        assert_eq!(limits.approval_amount(), U256::from(30));
    }
}
