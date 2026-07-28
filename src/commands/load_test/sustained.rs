use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eyre::{Result, WrapErr};
use tokio::task::JoinSet;

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
) -> Result<SustainedResult> {
    if tps == 0 || duration_secs == 0 || key_cycle == 0 {
        eyre::bail!("sustained schedule values must all be greater than zero");
    }
    let total_expected = u64::try_from(tps)
        .wrap_err("sustained TPS exceeds the supported transaction count")?
        .checked_mul(duration_secs)
        .ok_or_else(|| eyre::eyre!("sustained transaction count overflow"))?;
    tps.checked_mul(key_cycle)
        .ok_or_else(|| eyre::eyre!("sustained key-pool size overflow"))?;

    let src_confirmed = Arc::new(AtomicU64::new(0));
    let src_failed = Arc::new(AtomicU64::new(0));
    let fired_ctr = Arc::new(AtomicU64::new(0));

    let test_start = Instant::now();

    let mut all_tasks = JoinSet::new();
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

            let confirmed_ctr = Arc::clone(&src_confirmed);
            let failed_ctr = Arc::clone(&src_failed);
            let fired = Arc::clone(&fired_ctr);

            fired.fetch_add(1, Ordering::Relaxed);

            all_tasks.spawn(async move {
                let result = fut.await;
                if result.is_success() {
                    confirmed_ctr.fetch_add(1, Ordering::Relaxed);
                } else {
                    failed_ctr.fetch_add(1, Ordering::Relaxed);
                }
                result
            });
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

    let mut metrics = Vec::with_capacity(total_submitted as usize);
    let mut join_error = None;
    let mut receipt_interval = tokio::time::interval(Duration::from_secs(1));
    while !all_tasks.is_empty() {
        tokio::select! {
            joined = all_tasks.join_next() => {
                match joined.expect("JoinSet reported non-empty but returned no task") {
                    Ok(metric) => metrics.push(metric),
                    Err(error) => {
                        if join_error.is_none() {
                            join_error = Some(error);
                            all_tasks.abort_all();
                        }
                    }
                }
            }
            _ = receipt_interval.tick() => {
                let confirmed = src_confirmed.load(Ordering::Relaxed);
                let failed = src_failed.load(Ordering::Relaxed);
                let in_flight = total_submitted.saturating_sub(confirmed + failed);
                spinner.set_message(format!(
                    "waiting for receipts: {confirmed} confirmed  {failed} failed  {in_flight} in-flight"
                ));
            }
        }
    }

    // Signal verification pipeline that sending is complete.
    if let Some(ref done) = send_done {
        done.store(true, Ordering::Relaxed);
    }

    let test_duration = test_start.elapsed().as_secs_f64();
    if let Some(error) = join_error {
        spinner.abandon_with_message("send phase failed: task did not complete");
        return Err(error).wrap_err("sustained send task failed");
    }
    let confirmed_count = src_confirmed.load(Ordering::Relaxed);
    // Finish the send spinner with a completion message instead of clearing +
    // printing a separate line. This keeps MultiProgress layout clean when
    // a verification spinner is running concurrently below.
    spinner.finish_with_message(format!(
        "send phase complete: {confirmed_count}/{total_submitted} src-confirmed in {test_duration:.1}s"
    ));

    let total_failed = metrics.iter().filter(|m| !m.is_success()).count() as u64;
    if total_failed > 0 {
        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for m in metrics.iter().filter(|m| !m.is_success()) {
            let reason = m
                .error()
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

    Ok(SustainedResult {
        metrics,
        test_duration_secs: test_duration,
        total_submitted,
    })
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{MakeTask, run_sustained_loop};

    #[tokio::test]
    async fn task_panics_propagate_and_signal_send_completion() {
        let send_done = Arc::new(AtomicBool::new(false));
        let make_task: MakeTask = Box::new(|_, _| {
            Box::pin(async {
                panic!("simulated sender panic");
            })
        });

        let error = run_sustained_loop(
            1,
            1,
            1,
            None,
            make_task,
            Some(Arc::clone(&send_done)),
            indicatif::ProgressBar::hidden(),
        )
        .await
        .err()
        .expect("panicking sender task should fail the sustained loop");

        assert!(send_done.load(Ordering::Relaxed));
        assert!(error.to_string().contains("sustained send task failed"));
    }
}
