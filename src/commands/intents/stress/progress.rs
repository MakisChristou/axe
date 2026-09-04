use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use indicatif::ProgressBar;

use super::super::types::format_units;
use super::types::{
    AdmissionState, RateSample, StopReason, StressLimits, StressProgress, StressTelemetry,
};
use crate::ui;

const RATE_WINDOW: Duration = Duration::from_secs(10);

pub(super) fn bar() -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_style(ui::progress_spinner_style(
        "  {spinner:.cyan} {elapsed_precise}  {msg}",
    ));
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

impl StressProgress {
    pub fn new(bar: ProgressBar, limits: StressLimits, telemetry: Arc<StressTelemetry>) -> Self {
        Self {
            bar,
            limits,
            telemetry,
            samples: VecDeque::from([RateSample {
                elapsed: Duration::ZERO,
                broadcast: 0,
                confirmed: 0,
            }]),
        }
    }

    pub fn update(&mut self, state: &AdmissionState, stop: Option<StopReason>) {
        let warnings = self.telemetry.warnings.load(Ordering::Relaxed);
        let (send_rate, confirm_rate) =
            self.rates(state.started.elapsed(), state.broadcast, state.confirmed);
        let phase = stop.map_or_else(
            || "RUNNING".to_owned(),
            |reason| format!("DRAINING: {}", reason.label()),
        );
        self.bar.set_message(format!(
            "{phase} | SEND {send_rate:.2}/s | CONFIRM {confirm_rate:.2}/s (10s)\n  {} broadcast | {} confirmed | {} active | {} skipped | {} failed | {warnings} retries\n  deposited {} / {} {} input",
            state.broadcast, state.confirmed, state.active, state.skipped, state.failed,
            format_units(alloy::primitives::U256::from(state.confirmed).saturating_mul(self.limits.amount), self.limits.decimals),
            format_units(self.limits.max_volume, self.limits.decimals), self.limits.symbol,
        ));
    }

    fn rates(&mut self, elapsed: Duration, broadcast: u64, confirmed: u64) -> (f64, f64) {
        let cutoff = elapsed.saturating_sub(RATE_WINDOW);
        while self
            .samples
            .get(1)
            .is_some_and(|sample| sample.elapsed <= cutoff)
        {
            self.samples.pop_front();
        }
        let rates = self.samples.front().map_or((0.0, 0.0), |sample| {
            let seconds = elapsed.saturating_sub(sample.elapsed).as_secs_f64();
            if seconds == 0.0 {
                (0.0, 0.0)
            } else {
                (
                    broadcast.saturating_sub(sample.broadcast) as f64 / seconds,
                    confirmed.saturating_sub(sample.confirmed) as f64 / seconds,
                )
            }
        });
        if self
            .samples
            .back()
            .is_none_or(|sample| elapsed.saturating_sub(sample.elapsed) >= Duration::from_secs(1))
        {
            self.samples.push_back(RateSample {
                elapsed,
                broadcast,
                confirmed,
            });
        }
        rates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    #[test]
    fn throughput_counts_deposits_separately_and_decays_when_work_stalls() {
        let limits = StressLimits {
            duration: Duration::from_secs(60),
            max_intents: 200,
            max_in_flight: 16,
            amount: U256::from(1),
            max_volume: U256::from(200),
            max_native_spend: U256::from(20),
            min_native_balance: U256::ZERO,
            decimals: 6,
            symbol: "USDC".to_owned(),
        };
        let mut progress = StressProgress::new(
            ProgressBar::hidden(),
            limits,
            Arc::new(StressTelemetry::default()),
        );
        assert_eq!(progress.rates(Duration::ZERO, 0, 0), (0.0, 0.0));
        for seconds in 1..=10 {
            assert_eq!(
                progress.rates(Duration::from_secs(seconds), seconds * 2, seconds),
                (2.0, 1.0)
            );
        }
        for seconds in 11..=20 {
            progress.rates(Duration::from_secs(seconds), 20, 10);
        }
        assert_eq!(progress.rates(Duration::from_secs(21), 20, 10), (0.0, 0.0));
        assert!(progress.samples.len() <= 12);
    }
}
