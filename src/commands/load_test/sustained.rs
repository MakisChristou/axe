use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::future::join_all;
use tokio::sync::Mutex;

use super::metrics::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};
use crate::ui;

/// Result of the sustained send loop (before verification).
pub(super) struct SustainedResult {
    pub metrics: Vec<TxMetrics>,
    pub test_duration_secs: f64,
    pub total_submitted: u64,
}

/// A boxed future that produces a single `TxMetrics`.
type TxFuture = Pin<Box<dyn Future<Output = TxMetrics> + Send>>;

/// A factory that, given `(key_index, optional_nonce)`, returns a future
/// that sends one transaction and produces its metrics.
pub(super) type MakeTask = Box<dyn FnMut(usize, Option<u64>) -> TxFuture + Send>;

/// Run the sustained send loop: fire `tps` transactions per second for
/// `duration_secs`, rotating through a key pool of size `tps * key_cycle`.
///
/// The `make_task` closure is called once per transaction with
/// `(key_index, optional_nonce)` and must return a future that sends
/// the transaction and returns its `TxMetrics`.
///
/// Optional parameters:
/// - `nonces`: pre-fetched nonces for EVM keys (incremented locally per tick).
/// - `send_done` + verify channel: signalled when the send phase finishes.
/// - `spinner`: progress bar for live display.
pub(super) async fn run_sustained_loop(
    tps: usize,
    duration_secs: u64,
    key_cycle: usize,
    mut nonces: Option<Vec<u64>>,
    mut make_task: MakeTask,
    send_done: Option<Arc<AtomicBool>>,
    spinner: indicatif::ProgressBar,
) -> SustainedResult {
    let total_expected = tps as u64 * duration_secs;

    let metrics_list: Arc<Mutex<Vec<TxMetrics>>> = Arc::new(Mutex::new(Vec::new()));
    let src_confirmed = Arc::new(AtomicU64::new(0));
    let src_failed = Arc::new(AtomicU64::new(0));
    let fired_ctr = Arc::new(AtomicU64::new(0));

    let test_start = Instant::now();

    let mut all_tasks: Vec<tokio::task::JoinHandle<()>> =
        Vec::with_capacity(total_expected as usize);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut tick: u64 = 0;
    loop {
        interval.tick().await;
        if tick >= duration_secs {
            break;
        }

        let batch_start = (tick as usize % key_cycle) * tps;
        for i in 0..tps {
            let key_idx = batch_start + i;

            let nonce = nonces.as_mut().map(|n| {
                let val = n[key_idx];
                n[key_idx] += 1;
                val
            });

            let fut = make_task(key_idx, nonce);

            let metrics_clone = Arc::clone(&metrics_list);
            let confirmed_ctr = Arc::clone(&src_confirmed);
            let failed_ctr = Arc::clone(&src_failed);
            let fired = Arc::clone(&fired_ctr);

            fired.fetch_add(1, Ordering::Relaxed);

            let handle = tokio::spawn(async move {
                let result = fut.await;
                if result.success {
                    confirmed_ctr.fetch_add(1, Ordering::Relaxed);
                } else {
                    failed_ctr.fetch_add(1, Ordering::Relaxed);
                }
                metrics_clone.lock().await.push(result);
            });
            all_tasks.push(handle);
        }

        let elapsed_s = test_start.elapsed().as_secs();
        let f = fired_ctr.load(Ordering::Relaxed);
        let c = src_confirmed.load(Ordering::Relaxed);
        let fail = src_failed.load(Ordering::Relaxed);
        spinner.set_message(format!(
            "[{elapsed_s}/{duration_secs}s]  fired: {f}/{total_expected}  src-confirmed: {c}  failed: {fail}  (target: {tps} tx/s)"
        ));
        tick += 1;
    }

    let total_submitted = all_tasks.len() as u64;

    // Background ticker showing receipt progress while waiting for in-flight tasks.
    {
        let sp = spinner.clone();
        let confirmed_c = Arc::clone(&src_confirmed);
        let failed_c = Arc::clone(&src_failed);
        let total = total_submitted;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let c = confirmed_c.load(Ordering::Relaxed);
                let f = failed_c.load(Ordering::Relaxed);
                let in_flight = total.saturating_sub(c + f);
                sp.set_message(format!(
                    "waiting for receipts: {c} confirmed  {f} failed  {in_flight} in-flight"
                ));
                if in_flight == 0 {
                    break;
                }
            }
        });
    }
    join_all(all_tasks).await;

    // Signal verification pipeline that sending is complete.
    if let Some(ref done) = send_done {
        done.store(true, Ordering::Relaxed);
    }

    let test_duration = test_start.elapsed().as_secs_f64();
    let confirmed_count = src_confirmed.load(Ordering::Relaxed);
    // Finish the send spinner with a completion message instead of clearing +
    // printing a separate line. This keeps MultiProgress layout clean when
    // a verification spinner is running concurrently below.
    spinner.finish_with_message(format!(
        "send phase complete: {confirmed_count}/{total_submitted} src-confirmed in {test_duration:.1}s"
    ));

    let metrics = metrics_list.lock().await.clone();

    let total_failed = metrics.iter().filter(|m| !m.success).count() as u64;
    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.success) {
            let reason = m
                .error
                .as_deref()
                .unwrap_or("unknown")
                .chars()
                .take(120)
                .collect::<String>();
            *error_counts.entry(reason).or_default() += 1;
        }
        for (reason, count) in &error_counts {
            ui::warn(&format!("{count} txs failed: {reason}"));
        }
    }

    SustainedResult {
        metrics,
        test_duration_secs: test_duration,
        total_submitted,
    }
}

/// Build a `LoadTestReport` from the sustained loop result.
pub(super) fn build_sustained_report(
    result: SustainedResult,
    source_chain: &str,
    destination_chain: &str,
    destination_address: &str,
    total_expected: u64,
    num_keys: usize,
) -> LoadTestReport {
    LoadTestReport::from_transactions(
        ReportInput {
            source_chain: source_chain.to_string(),
            destination_chain: destination_chain.to_string(),
            destination_address: destination_address.to_string(),
            num_txs: total_expected,
            num_keys,
            total_submitted: result.total_submitted,
            test_duration_secs: result.test_duration_secs,
            compute_unit_summary: ComputeUnitSummary::Include,
        },
        result.metrics,
    )
}
