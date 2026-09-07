use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use indicatif::ProgressBar;

use super::super::presentation::intent_activity_bar;
use super::types::{FailureKind, QuoteBenchmarkLimit, Sample, SampleCounts, SampleOutcome};
use crate::ui;

pub(super) struct BenchmarkProgress {
    bar: ProgressBar,
    limit: QuoteBenchmarkLimit,
    phase: &'static str,
    coverage: String,
    started: Instant,
    attempted: AtomicU64,
    available: AtomicU64,
    unavailable: AtomicU64,
    failed: AtomicU64,
    timed_out: AtomicU64,
    request_failures: AtomicU64,
    invalid_quotes: AtomicU64,
    invalid_outputs: AtomicU64,
}

impl BenchmarkProgress {
    pub fn new(
        limit: QuoteBenchmarkLimit,
        phase: &'static str,
        coverage: String,
        visible: bool,
    ) -> Self {
        let bar = if visible {
            intent_activity_bar("")
        } else {
            ProgressBar::hidden()
        };
        let progress = Self {
            bar,
            limit,
            phase,
            coverage,
            started: Instant::now(),
            attempted: AtomicU64::new(0),
            available: AtomicU64::new(0),
            unavailable: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            request_failures: AtomicU64::new(0),
            invalid_quotes: AtomicU64::new(0),
            invalid_outputs: AtomicU64::new(0),
        };
        progress.refresh();
        progress
    }

    pub fn record(&self, sample: &Sample) {
        match sample.outcome {
            SampleOutcome::Available { .. } => &self.available,
            SampleOutcome::Unavailable => &self.unavailable,
            SampleOutcome::Failed(_) => &self.failed,
            SampleOutcome::TimedOut => &self.timed_out,
        }
        .fetch_add(1, Ordering::Relaxed);
        let failure_counter = match sample.outcome {
            SampleOutcome::Failed(FailureKind::Request) => Some(&self.request_failures),
            SampleOutcome::Failed(FailureKind::InvalidQuote) => Some(&self.invalid_quotes),
            SampleOutcome::Failed(FailureKind::InvalidOutput) => Some(&self.invalid_outputs),
            _ => None,
        };
        if let Some(counter) = failure_counter {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        self.attempted.fetch_add(1, Ordering::Relaxed);
        self.refresh();
    }

    pub fn finish(&self) {
        self.refresh();
        self.bar.finish_and_clear();
    }

    pub fn counts(&self) -> SampleCounts {
        SampleCounts {
            attempted: self.attempted.load(Ordering::Relaxed),
            available: self.available.load(Ordering::Relaxed),
            unavailable: self.unavailable.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
            request_failures: self.request_failures.load(Ordering::Relaxed),
            invalid_quotes: self.invalid_quotes.load(Ordering::Relaxed),
            invalid_outputs: self.invalid_outputs.load(Ordering::Relaxed),
        }
    }

    fn refresh(&self) {
        let available = self.available.load(Ordering::Relaxed);
        let unavailable = self.unavailable.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        let timed_out = self.timed_out.load(Ordering::Relaxed);
        let completed = available + unavailable + failed + timed_out;
        let limit = match self.limit {
            QuoteBenchmarkLimit::Requests(requests) => format!("{completed}/{requests} requests"),
            QuoteBenchmarkLimit::Duration(duration) => {
                format!("{} limit", ui::format_duration(duration))
            }
            QuoteBenchmarkLimit::Continuous => "continuous".to_owned(),
        };
        let rps = completed as f64 / self.started.elapsed().as_secs_f64().max(f64::EPSILON);
        self.bar.set_message(format!(
            "{} | QUOTE {rps:.2}/s (average)\n  {available} available | {unavailable} unavailable | {failed} failed | {timed_out} timed out\n  {} | {limit}",
            self.phase.to_ascii_uppercase(), self.coverage
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_counts_all_outcomes_and_failure_kinds() {
        let progress = BenchmarkProgress::new(
            QuoteBenchmarkLimit::Continuous,
            "test",
            "3 routes ↔".to_owned(),
            false,
        );
        let outcomes = [
            SampleOutcome::Available {
                output_amount: alloy::primitives::U256::from(1),
                validity_ms: 1,
            },
            SampleOutcome::Unavailable,
            SampleOutcome::Failed(FailureKind::InvalidQuote),
            SampleOutcome::TimedOut,
        ];
        for (index, outcome) in outcomes.into_iter().enumerate() {
            progress.record(&Sample {
                latency_ms: index as u64,
                outcome,
            });
        }

        let counts = progress.counts();
        assert_eq!(counts.attempted, 4);
        assert_eq!(counts.available, 1);
        assert_eq!(counts.unavailable, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.timed_out, 1);
        assert_eq!(counts.invalid_quotes, 1);
        let message = progress.bar.message();
        assert_eq!(message.lines().count(), 3);
        assert!(message.contains("QUOTE"));
        assert!(message.contains("1 available | 1 unavailable | 1 failed | 1 timed out"));
        assert!(!message.contains("fulfilled"));
    }

    #[test]
    fn progress_distinguishes_request_and_duration_limits() {
        for (limit, expected) in [
            (QuoteBenchmarkLimit::Requests(20), "0/20 requests"),
            (
                QuoteBenchmarkLimit::Duration(std::time::Duration::from_secs(60)),
                "1m 00s limit",
            ),
            (QuoteBenchmarkLimit::Continuous, "continuous"),
        ] {
            let progress = BenchmarkProgress::new(limit, "benchmark", "fixed route".into(), false);
            assert!(progress.bar.message().contains(expected));
        }
    }
}
