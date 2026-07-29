//! Chain-neutral source transaction submission.
//!
//! A submitter owns the chain client and confirmation policy for one route.
//! The shared driver owns concurrency, progress, task lifetime, and metric
//! collection. Wallet preparation and transaction construction stay with the
//! chain-specific module.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eyre::Result;
use tokio::sync::Semaphore;

use super::metrics::TxMetrics;
use crate::ui;

/// Submit and confirm one chain-specific, fully prepared transaction.
pub(super) trait TransactionSubmitter: Send + Sync + 'static {
    type Job: Send + 'static;

    fn submit(&self, job: Self::Job) -> impl std::future::Future<Output = TxMetrics> + Send;
}

/// Chain-neutral output of a burst submission run.
pub(super) struct BurstResult {
    pub metrics: Vec<TxMetrics>,
    pub total_submitted: u64,
    pub test_duration_secs: f64,
}

/// Submit prepared jobs concurrently and collect their normalized metrics.
pub(super) async fn run_burst<S>(
    submitter: S,
    jobs: Vec<S::Job>,
    max_concurrent: usize,
) -> Result<BurstResult>
where
    S: TransactionSubmitter,
{
    let total = jobs.len();
    let total_submitted = total as u64;
    let submitter = Arc::new(submitter);
    let semaphore = Arc::new(Semaphore::new(max_concurrent.max(1)));
    let confirmed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{total} confirmed)..."));
    let test_start = Instant::now();

    let tasks = jobs
        .into_iter()
        .map(|job| {
            let submitter = Arc::clone(&submitter);
            let semaphore = Arc::clone(&semaphore);
            let confirmed = Arc::clone(&confirmed);
            let spinner = spinner.clone();
            tokio::spawn(async move {
                let Ok(_permit) = semaphore.acquire_owned().await else {
                    return TxMetrics::failed("", 0, "burst semaphore closed unexpectedly");
                };
                let metrics = submitter.submit(job).await;
                if metrics.is_success() {
                    let done = confirmed.fetch_add(1, Ordering::Relaxed) + 1;
                    spinner.set_message(format!("sending ({done}/{total} confirmed)..."));
                }
                metrics
            })
        })
        .collect();

    let metrics = super::task_group::join_all(tasks).await?;
    let test_duration_secs = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} confirmed"
    ));

    Ok(BurstResult {
        metrics,
        total_submitted,
        test_duration_secs,
    })
}

/// Submit jobs one at a time, optionally rate-pacing their start times.
///
/// This is the chain-neutral driver for account models such as Stellar where
/// one wallet's sequence number makes concurrent submission invalid.
pub(super) async fn run_serial<S>(
    submitter: S,
    jobs: Vec<S::Job>,
    pacing: Option<Duration>,
) -> Result<BurstResult>
where
    S: TransactionSubmitter,
{
    let total = jobs.len();
    let total_submitted = total as u64;
    let spinner = ui::wait_spinner(&format!("sending (0/{total} confirmed)..."));
    let mut interval = pacing.map(|duration| {
        let mut interval = tokio::time::interval(duration);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    });
    let test_start = Instant::now();
    let mut metrics = Vec::with_capacity(total);
    let mut confirmed = 0u64;

    for job in jobs {
        if let Some(interval) = &mut interval {
            interval.tick().await;
        }
        let result = submitter.submit(job).await;
        if result.is_success() {
            confirmed += 1;
        }
        metrics.push(result);
        spinner.set_message(format!("sending ({confirmed}/{total} confirmed)..."));
    }

    let test_duration_secs = test_start.elapsed().as_secs_f64();
    spinner.finish_and_clear();
    ui::success(&format!("sent {confirmed}/{total_submitted} confirmed"));
    Ok(BurstResult {
        metrics,
        total_submitted,
        test_duration_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::{TransactionSubmitter, run_burst, run_serial};
    use crate::commands::load_test::metrics::TxMetrics;

    struct FakeSubmitter;

    impl TransactionSubmitter for FakeSubmitter {
        type Job = u64;

        async fn submit(&self, job: Self::Job) -> TxMetrics {
            let mut metrics = TxMetrics::succeeded(job.to_string(), 0);
            metrics.confirm_time_ms = Some(0);
            metrics.latency_ms = Some(0);
            metrics
        }
    }

    #[tokio::test]
    async fn burst_collects_each_submitted_job() {
        let result = run_burst(FakeSubmitter, vec![1, 2, 3], 2)
            .await
            .expect("fake burst should succeed");

        assert_eq!(result.total_submitted, 3);
        assert_eq!(
            result
                .metrics
                .iter()
                .map(|metrics| metrics.signature.as_str())
                .collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
    }

    #[tokio::test]
    async fn serial_preserves_job_order() {
        let result = run_serial(FakeSubmitter, vec![3, 1, 2], None)
            .await
            .expect("fake serial run should succeed");

        assert_eq!(result.total_submitted, 3);
        assert_eq!(
            result
                .metrics
                .iter()
                .map(|metrics| metrics.signature.as_str())
                .collect::<Vec<_>>(),
            ["3", "1", "2"]
        );
    }
}
