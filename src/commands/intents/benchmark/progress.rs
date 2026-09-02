use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use indicatif::ProgressBar;

use super::super::presentation::{intent_activity_bar, intent_progress_bar};
use super::types::{FailureKind, QuoteBenchmarkLimit, Sample, SampleCounts, SampleOutcome};

pub(super) struct BenchmarkProgress {
    bar: ProgressBar,
    limit: QuoteBenchmarkLimit,
    phase: &'static str,
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
    pub fn new(limit: QuoteBenchmarkLimit, phase: &'static str, visible: bool) -> Self {
        let bar = if visible {
            progress_bar(limit)
        } else {
            ProgressBar::hidden()
        };
        let progress = Self {
            bar,
            limit,
            phase,
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
        match self.limit {
            QuoteBenchmarkLimit::Requests(_) => self.bar.set_position(completed),
            QuoteBenchmarkLimit::Duration(duration) => {
                let position = u64::try_from(self.started.elapsed().as_millis())
                    .unwrap_or(u64::MAX)
                    .min(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
                self.bar.set_position(position);
            }
            QuoteBenchmarkLimit::Continuous => self.bar.set_position(completed),
        }
        let rps = completed as f64 / self.started.elapsed().as_secs_f64().max(f64::EPSILON);
        self.bar.set_message(format!(
            "{} · {available} available · {unavailable} unavailable · {failed} failed · {timed_out} timed out · {rps:.1} req/s",
            self.phase
        ));
    }
}

fn progress_bar(limit: QuoteBenchmarkLimit) -> ProgressBar {
    match limit {
        QuoteBenchmarkLimit::Requests(requests) => intent_progress_bar(requests, ""),
        QuoteBenchmarkLimit::Duration(duration) => {
            intent_progress_bar(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX), "")
        }
        QuoteBenchmarkLimit::Continuous => intent_activity_bar(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_counts_all_outcomes_and_failure_kinds() {
        let progress = BenchmarkProgress::new(QuoteBenchmarkLimit::Continuous, "test", false);
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
    }
}
