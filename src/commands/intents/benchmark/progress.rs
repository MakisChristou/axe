use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use indicatif::ProgressBar;

use super::types::{QuoteBenchmarkLimit, Sample, SampleOutcome};
use crate::ui;

pub(super) struct BenchmarkProgress {
    bar: ProgressBar,
    limit: QuoteBenchmarkLimit,
    phase: &'static str,
    started: Instant,
    available: AtomicU64,
    unavailable: AtomicU64,
    failed: AtomicU64,
    timed_out: AtomicU64,
}

impl BenchmarkProgress {
    pub fn new(limit: QuoteBenchmarkLimit, phase: &'static str, visible: bool) -> Self {
        let bar = if visible {
            progress_bar(limit)
        } else {
            ProgressBar::hidden()
        };
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        let progress = Self {
            bar,
            limit,
            phase,
            started: Instant::now(),
            available: AtomicU64::new(0),
            unavailable: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
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
        self.refresh();
    }

    pub fn finish(&self) {
        self.refresh();
        self.bar.finish_and_clear();
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
        }
        let rps = completed as f64 / self.started.elapsed().as_secs_f64().max(f64::EPSILON);
        self.bar.set_message(format!(
            "{} · {available} available · {unavailable} unavailable · {failed} failed · {timed_out} timed out · {rps:.1} req/s",
            self.phase
        ));
    }
}

fn progress_bar(limit: QuoteBenchmarkLimit) -> ProgressBar {
    let bar = match limit {
        QuoteBenchmarkLimit::Requests(requests) => ProgressBar::new(requests),
        QuoteBenchmarkLimit::Duration(duration) => {
            ProgressBar::new(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        }
    };
    bar.set_style(ui::progress_bar_style(
        "  {spinner:.cyan} [{bar:32.cyan/dim}] {percent:>3}% · {msg}",
    ));
    bar
}
