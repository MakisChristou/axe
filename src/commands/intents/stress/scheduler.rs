use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::task::JoinSet;

use super::super::execution::prepare_deposit;
use super::super::types::LegExecution;
use super::types::{
    AdmissionState, DepositOutcome, DepositRecord, DepositTask, SchedulerArgs, SourceState,
    StopReason, StressProgress, StressRun, TaskCompletion,
};
use crate::evm::pipeline::PipelineError;
use crate::ui;

pub(super) async fn run(mut args: SchedulerArgs) -> StressRun {
    let mut state = AdmissionState::new();
    let mut tasks = JoinSet::new();
    let mut records = Vec::new();
    let mut cursor = 0;
    let mut stop = None;
    let deadline = args
        .limits
        .duration
        .map(|duration| tokio::time::Instant::now() + duration);
    let mut refresh = tokio::time::interval(Duration::from_secs(1));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut progress = StressProgress::new(
        args.progress.clone(),
        args.limits.clone(),
        Arc::clone(&args.telemetry),
    );

    loop {
        if stop.is_none() {
            stop = if args.shutdown.requested() {
                Some(StopReason::Interrupted)
            } else if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                Some(StopReason::Duration)
            } else {
                state.permanent_stop(&args.limits)
            };
        }
        if stop.is_none() {
            admit_deposits(&mut args, &mut tasks, &mut state, &mut cursor);
            if tasks.is_empty() && args.sources.iter().all(|source| source.sender.stopped()) {
                stop = Some(StopReason::SourcesStopped);
            }
        }
        state.broadcast = args
            .sources
            .iter()
            .map(|source| source.sender.broadcasts.load(Ordering::Relaxed))
            .sum();
        progress.update(&state, stop);
        if stop.is_some() && tasks.is_empty() {
            break;
        }
        tokio::select! {
            joined = tasks.join_next(), if !tasks.is_empty() => {
                match joined {
                    Some(Ok(completion)) => complete_deposit(&mut args.sources, &mut state, &mut records, completion),
                    Some(Err(error)) => {
                        ui::error(&format!("deposit task could not be tracked: {error}"));
                        state.active = state.active.saturating_sub(1);
                        state.failed += 1;
                        state.committed += 1;
                        stop = Some(StopReason::SourcesStopped);
                    }
                    None => {}
                }
            }
            () = args.shutdown.cancelled(), if stop.is_none() => stop = Some(StopReason::Interrupted),
            () = wait_for_deadline(deadline), if stop.is_none() => stop = Some(StopReason::Duration),
            _ = refresh.tick() => {}
        }
    }
    for source in &mut args.sources {
        source.report.broadcast = source.sender.broadcasts.load(Ordering::Relaxed);
        source.report.gas_spent =
            super::super::types::format_units(source.sender.gas_spent().await, 18);
    }
    StressRun {
        stop_reason: stop.unwrap_or(StopReason::SourcesStopped),
        state,
        records,
        warnings: args.telemetry.warnings.load(Ordering::Relaxed),
        sources: args
            .sources
            .into_iter()
            .map(|source| source.report)
            .collect(),
    }
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn admit_deposits(
    args: &mut SchedulerArgs,
    tasks: &mut JoinSet<TaskCompletion>,
    state: &mut AdmissionState,
    cursor: &mut usize,
) {
    while state.can_admit(&args.limits) {
        let Some(index) = next_source(&args.sources, *cursor, Instant::now()) else {
            break;
        };
        *cursor = (index + 1) % args.sources.len();
        let source = &mut args.sources[index];
        let plan = source.routes[source.cursor % source.routes.len()].clone();
        source.cursor = source.cursor.wrapping_add(1);
        state.admit();
        let task = DepositTask {
            client: args.client.clone(),
            wallet: args.wallet,
            source: index,
            sender: Arc::clone(&source.sender),
            plan,
            telemetry: Arc::clone(&args.telemetry),
        };
        tasks.spawn(async move {
            ui::count_warnings(Arc::clone(&task.telemetry.warnings), run_deposit(task)).await
        });
    }
}

fn next_source(sources: &[SourceState], cursor: usize, now: Instant) -> Option<usize> {
    (0..sources.len())
        .map(|offset| (cursor + offset) % sources.len())
        .find(|index| {
            let source = &sources[*index];
            !source.sender.stopped() && source.ready_at <= now
        })
}

async fn run_deposit(task: DepositTask) -> TaskCompletion {
    let leg = LegExecution {
        from: task.plan.from,
        to: task.plan.to,
        order_type: task.plan.order_type,
        amount: task.plan.requested_amount,
        recipient: task.wallet,
        max_input_amount: Some(task.plan.input_amount),
        settlement_contract: task.plan.settlement_contract,
    };
    let prepared = match prepare_deposit(&task.client, task.wallet, leg).await {
        Ok(prepared) => prepared,
        Err(error) => {
            return TaskCompletion {
                source: task.source,
                outcome: DepositOutcome::Skipped(ui::scrub_urls(&format!("{error:#}"))),
            };
        }
    };
    let started = Instant::now();
    let outcome = match task.sender.send(prepared.transaction).await {
        Ok(receipt) if receipt.status() => DepositOutcome::Confirmed(DepositRecord {
            quote_id: prepared.quote_id,
            transaction_hash: receipt.transaction_hash.to_string(),
            quote_latency_ms: prepared.quote_latency_ms,
            deposit_latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }),
        Ok(receipt) => {
            DepositOutcome::Failed(format!("deposit reverted: {}", receipt.transaction_hash))
        }
        Err(PipelineError::NotSent(error)) => DepositOutcome::Skipped(ui::scrub_urls(&error)),
        Err(PipelineError::Uncertain(error)) => DepositOutcome::Failed(ui::scrub_urls(&error)),
    };
    TaskCompletion {
        source: task.source,
        outcome,
    }
}

fn complete_deposit(
    sources: &mut [SourceState],
    state: &mut AdmissionState,
    records: &mut Vec<DepositRecord>,
    completion: TaskCompletion,
) {
    let source = &mut sources[completion.source];
    state.complete(&completion.outcome);
    match completion.outcome {
        DepositOutcome::Confirmed(record) => {
            source.report.confirmed += 1;
            records.push(record);
        }
        DepositOutcome::Skipped(reason) => {
            source.report.skipped += 1;
            source.report.last_issue = Some(reason);
            source.ready_at = Instant::now() + Duration::from_secs(1);
        }
        DepositOutcome::Failed(reason) => {
            source.report.failed += 1;
            source.report.last_issue = Some(reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shutdown::{DrainTarget, Shutdown};

    #[tokio::test(start_paused = true)]
    async fn timed_runs_wait_for_the_configured_deadline() {
        let started = tokio::time::Instant::now();
        let duration = Duration::from_secs(7200);
        wait_for_deadline(Some(started + duration)).await;
        assert_eq!(started.elapsed(), duration);
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_runs_have_no_deadline_and_can_be_interrupted() {
        assert!(
            tokio::time::timeout(Duration::from_secs(86400), wait_for_deadline(None))
                .await
                .is_err()
        );
        let shutdown = Shutdown::test_instance(DrainTarget::IntentStress);
        shutdown.request_for_test();
        tokio::select! {
            () = wait_for_deadline(None) => panic!("continuous run must not expire"),
            () = shutdown.cancelled() => {}
        }
    }
}
