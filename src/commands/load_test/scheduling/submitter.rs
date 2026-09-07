//! Chain-neutral scheduled submission.
//!
//! A submitter owns the chain client and confirmation policy for one route.
//! The shared driver owns concurrency, progress, task lifetime, and metric
//! collection. Wallet preparation and transaction construction stay with the
//! chain-specific module.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::time;

use eyre::Result;
use tokio::task::JoinSet;

use super::metrics::TxMetrics;
use crate::shutdown::Shutdown;
use crate::ui;

/// Submit and confirm one chain-specific, fully prepared transaction.
pub(super) trait TransactionSubmitter: Send + Sync + 'static {
    type Job: Send + 'static;

    fn submit(&self, job: Self::Job) -> impl Future<Output = TxMetrics> + Send;
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
    run_burst_with_shutdown(submitter, jobs, max_concurrent, Shutdown::current()).await
}

async fn run_burst_with_shutdown<S>(
    submitter: S,
    jobs: Vec<S::Job>,
    max_concurrent: usize,
    shutdown: Option<Arc<Shutdown>>,
) -> Result<BurstResult>
where
    S: TransactionSubmitter,
{
    let total = jobs.len();
    let submitter = Arc::new(submitter);
    let confirmed = Arc::new(AtomicU64::new(0));
    let spinner = ui::wait_spinner(&format!("sending (0/{total} confirmed)..."));
    let test_start = Instant::now();
    let mut jobs = jobs.into_iter().enumerate();
    let mut tasks = JoinSet::new();
    let mut total_submitted = 0u64;
    let mut metrics = Vec::with_capacity(total);
    let mut join_error = None;

    while tasks.len() < max_concurrent.max(1) && !shutdown_requested(shutdown.as_deref()) {
        let Some((index, job)) = jobs.next() else {
            break;
        };
        spawn_submission(&mut tasks, Arc::clone(&submitter), index, job);
        total_submitted += 1;
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(metric) => {
                if metric.1.is_success() {
                    let done = confirmed.fetch_add(1, Ordering::Relaxed) + 1;
                    spinner.set_message(format!("sending ({done}/{total} confirmed)..."));
                }
                metrics.push(metric);
            }
            Err(error) => {
                join_error.get_or_insert(error);
            }
        };
        if join_error.is_none()
            && !shutdown_requested(shutdown.as_deref())
            && let Some((index, job)) = jobs.next()
        {
            spawn_submission(&mut tasks, Arc::clone(&submitter), index, job);
            total_submitted += 1;
        }
    }
    if let Some(error) = join_error {
        spinner.abandon_with_message("send phase failed: task did not complete");
        return Err(error.into());
    }
    metrics.sort_unstable_by_key(|(index, _)| *index);
    let metrics = metrics
        .into_iter()
        .map(|(_, metric)| metric)
        .collect::<Vec<_>>();
    let test_duration_secs = test_start.elapsed().as_secs_f64();
    let confirmed_count = confirmed.load(Ordering::Relaxed);
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed_count}/{total_submitted} submitted transactions confirmed"
    ));
    warn_skipped(total, total_submitted);

    Ok(BurstResult {
        metrics,
        total_submitted,
        test_duration_secs,
    })
}

fn spawn_submission<S>(
    tasks: &mut JoinSet<(usize, TxMetrics)>,
    submitter: Arc<S>,
    index: usize,
    job: S::Job,
) where
    S: TransactionSubmitter,
{
    tasks.spawn(async move { (index, submitter.submit(job).await) });
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
    let spinner = ui::wait_spinner(&format!("sending (0/{total} confirmed)..."));
    let shutdown = Shutdown::current();
    let mut interval = pacing.map(|duration| {
        let mut interval = time::interval(duration);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        interval
    });
    let test_start = Instant::now();
    let mut metrics = Vec::with_capacity(total);
    let mut confirmed = 0u64;

    for job in jobs {
        if shutdown_requested(shutdown.as_deref()) {
            break;
        }
        if let Some(interval) = &mut interval {
            let paced = match shutdown.as_deref() {
                Some(shutdown) => tokio::select! {
                    _ = interval.tick() => true,
                    () = shutdown.cancelled() => false,
                },
                None => {
                    interval.tick().await;
                    true
                }
            };
            if !paced {
                break;
            }
        }
        let result = submitter.submit(job).await;
        if result.is_success() {
            confirmed += 1;
        }
        metrics.push(result);
        spinner.set_message(format!("sending ({confirmed}/{total} confirmed)..."));
    }

    let test_duration_secs = test_start.elapsed().as_secs_f64();
    let total_submitted = metrics.len() as u64;
    spinner.finish_and_clear();
    ui::success(&format!(
        "sent {confirmed}/{total_submitted} submitted transactions confirmed"
    ));
    warn_skipped(total, total_submitted);
    Ok(BurstResult {
        metrics,
        total_submitted,
        test_duration_secs,
    })
}

fn shutdown_requested(shutdown: Option<&Shutdown>) -> bool {
    shutdown.is_some_and(Shutdown::requested)
}

fn warn_skipped(total: usize, total_submitted: u64) {
    let skipped = total.saturating_sub(total_submitted as usize);
    if skipped > 0 {
        ui::warn(&format!(
            "skipped {skipped} queued transactions during graceful shutdown"
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::{TransactionSubmitter, run_burst, run_burst_with_shutdown, run_serial};
    use crate::commands::load_test::metrics::TxMetrics;
    use crate::shutdown::{DrainTarget, Shutdown};
    use tokio::sync::Semaphore;
    use tokio::task::yield_now;
    use tokio::time::timeout;

    struct FakeSubmitter;

    struct BlockingSubmitter {
        started: Arc<AtomicU64>,
        release: Arc<Semaphore>,
    }

    impl TransactionSubmitter for FakeSubmitter {
        type Job = u64;

        async fn submit(&self, job: Self::Job) -> TxMetrics {
            let mut metrics = TxMetrics::succeeded(job.to_string(), 0);
            metrics.confirm_time_ms = Some(0);
            metrics.latency_ms = Some(0);
            metrics
        }
    }

    impl TransactionSubmitter for BlockingSubmitter {
        type Job = u64;

        async fn submit(&self, job: Self::Job) -> TxMetrics {
            self.started.fetch_add(1, Ordering::Relaxed);
            let permit = self.release.acquire().await;
            if let Ok(permit) = permit {
                permit.forget();
            }
            TxMetrics::succeeded(job.to_string(), 0)
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

    #[tokio::test]
    async fn burst_cancellation_drains_started_jobs_and_skips_the_queue() {
        let started = Arc::new(AtomicU64::new(0));
        let release = Arc::new(Semaphore::new(0));
        let shutdown = Shutdown::test_instance(DrainTarget::LoadTestSubmissions);
        let run = tokio::spawn(run_burst_with_shutdown(
            BlockingSubmitter {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            },
            vec![1, 2, 3, 4, 5],
            2,
            Some(Arc::clone(&shutdown)),
        ));

        timeout(Duration::from_secs(1), async {
            while started.load(Ordering::Relaxed) < 2 {
                yield_now().await;
            }
        })
        .await
        .expect("two submissions should start");
        shutdown.request_for_test();
        release.add_permits(2);

        let result = run
            .await
            .expect("scheduler task should complete")
            .expect("cancellable burst should complete");
        assert_eq!(result.total_submitted, 2);
        assert_eq!(result.metrics.len(), 2);
    }
}
