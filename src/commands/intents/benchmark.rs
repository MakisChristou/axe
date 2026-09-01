mod report;
mod types;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::future::join_all;
use tokio::sync::Mutex;
use tokio::time::{Interval, MissedTickBehavior};

use self::report::{duration_ms, render_report, report_json};
use self::types::{BenchmarkReport, BenchmarkTarget, FailureKind, RunLimit, Sample, SampleOutcome};
use super::client::RfqClient;
use super::read::{api_client, prepare_quote};
use super::route::validate_quote_route;
use super::types::{QuoteOutcome, parse_amount};

pub use self::types::{QuoteBenchmarkArgs, QuoteBenchmarkLimit};

pub async fn benchmark_quotes(args: QuoteBenchmarkArgs) -> eyre::Result<()> {
    let client = api_client(&args.api)?;
    let prepared = prepare_quote(&client, &args.request).await?;
    let target = Arc::new(BenchmarkTarget::from(prepared));
    run_warmup(&client, &target, &args).await;
    let report = run_benchmark(
        client,
        target,
        measured_limit(&args.limit),
        args.concurrency,
        args.request_timeout,
        args.max_rps,
    )
    .await;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
    } else {
        render_report(&report, args.concurrency, args.warmup, args.max_rps);
    }
    Ok(())
}

async fn run_warmup(client: &RfqClient, target: &Arc<BenchmarkTarget>, args: &QuoteBenchmarkArgs) {
    if args.warmup == 0 {
        return;
    }
    run_benchmark(
        client.clone(),
        Arc::clone(target),
        RunLimit::Requests(args.warmup),
        args.concurrency,
        args.request_timeout,
        args.max_rps,
    )
    .await;
}

const fn measured_limit(limit: &QuoteBenchmarkLimit) -> RunLimit {
    match limit {
        QuoteBenchmarkLimit::Requests(requests) => RunLimit::Requests(*requests),
        QuoteBenchmarkLimit::Duration(duration) => RunLimit::Duration(*duration),
    }
}

async fn run_benchmark(
    client: RfqClient,
    target: Arc<BenchmarkTarget>,
    limit: RunLimit,
    concurrency: usize,
    request_timeout: Duration,
    max_rps: Option<u64>,
) -> BenchmarkReport {
    let started = Instant::now();
    let next_request = Arc::new(AtomicU64::new(0));
    let rate_limit = rate_limiter(max_rps);
    let workers = (0..concurrency).map(|_| {
        benchmark_worker(
            client.clone(),
            Arc::clone(&target),
            limit,
            started,
            request_timeout,
            Arc::clone(&next_request),
            rate_limit.clone(),
        )
    });
    let samples = join_all(workers).await.into_iter().flatten().collect();
    BenchmarkReport {
        samples,
        elapsed: started.elapsed(),
        output_symbol: target.output_symbol.clone(),
        output_decimals: target.output_decimals,
    }
}

async fn benchmark_worker(
    client: RfqClient,
    target: Arc<BenchmarkTarget>,
    limit: RunLimit,
    started: Instant,
    request_timeout: Duration,
    next_request: Arc<AtomicU64>,
    rate_limit: Option<Arc<Mutex<Interval>>>,
) -> Vec<Sample> {
    let mut samples = Vec::new();
    loop {
        let request_index = next_request.fetch_add(1, Ordering::Relaxed);
        if !should_start(limit, request_index, started.elapsed()) {
            break;
        }
        if !wait_for_rate_limit(
            rate_limit.as_ref(),
            remaining_duration(limit, started.elapsed()),
        )
        .await
        {
            break;
        }
        samples.push(benchmark_request(&client, &target, request_timeout).await);
    }
    samples
}

fn should_start(limit: RunLimit, request_index: u64, elapsed: Duration) -> bool {
    match limit {
        RunLimit::Requests(requests) => request_index < requests,
        RunLimit::Duration(duration) => elapsed < duration,
    }
}

fn rate_limiter(max_rps: Option<u64>) -> Option<Arc<Mutex<Interval>>> {
    max_rps.map(|requests_per_second| {
        let period =
            Duration::from_secs_f64(1.0 / requests_per_second as f64).max(Duration::from_nanos(1));
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Arc::new(Mutex::new(interval))
    })
}

async fn wait_for_rate_limit(
    rate_limit: Option<&Arc<Mutex<Interval>>>,
    remaining: Option<Duration>,
) -> bool {
    let Some(rate_limit) = rate_limit else {
        return remaining.is_none_or(|remaining| !remaining.is_zero());
    };
    let wait = async {
        rate_limit.lock().await.tick().await;
    };
    match remaining {
        Some(remaining) => tokio::time::timeout(remaining, wait).await.is_ok(),
        None => {
            wait.await;
            true
        }
    }
}

fn remaining_duration(limit: RunLimit, elapsed: Duration) -> Option<Duration> {
    match limit {
        RunLimit::Requests(_) => None,
        RunLimit::Duration(duration) => Some(duration.saturating_sub(elapsed)),
    }
}

async fn benchmark_request(
    client: &RfqClient,
    target: &BenchmarkTarget,
    request_timeout: Duration,
) -> Sample {
    let started = Instant::now();
    let outcome = tokio::time::timeout(request_timeout, client.quote(&target.request)).await;
    let latency_ms = duration_ms(started.elapsed());
    let outcome = match outcome {
        Err(_) => SampleOutcome::TimedOut,
        Ok(Err(_)) => SampleOutcome::Failed(FailureKind::Request),
        Ok(Ok(QuoteOutcome::Unavailable(_))) => SampleOutcome::Unavailable,
        Ok(Ok(QuoteOutcome::Available(timed))) => available_outcome(&timed.quote, target),
    };
    Sample {
        latency_ms,
        outcome,
    }
}

fn available_outcome(quote: &super::types::Quote, target: &BenchmarkTarget) -> SampleOutcome {
    if validate_quote_route(
        quote,
        &target.from,
        &target.to,
        target.order_type,
        target.requested_amount,
    )
    .is_err()
    {
        return SampleOutcome::Failed(FailureKind::InvalidQuote);
    }
    let Ok(output_amount) = parse_amount(&quote.output.amount) else {
        return SampleOutcome::Failed(FailureKind::InvalidOutput);
    };
    let validity_ms = (quote.validity.quote_expires_at - Utc::now())
        .to_std()
        .map(duration_ms)
        .unwrap_or_default();
    SampleOutcome::Available {
        output_amount,
        validity_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_and_duration_limits_stop_scheduling() {
        assert!(should_start(RunLimit::Requests(100), 99, Duration::ZERO));
        assert!(!should_start(RunLimit::Requests(100), 100, Duration::ZERO));
        assert!(should_start(
            RunLimit::Duration(Duration::from_secs(1)),
            1_000,
            Duration::from_millis(999)
        ));
        assert!(!should_start(
            RunLimit::Duration(Duration::from_secs(1)),
            1_000,
            Duration::from_secs(1)
        ));
        assert_eq!(
            remaining_duration(
                RunLimit::Duration(Duration::from_secs(1)),
                Duration::from_millis(750)
            ),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            remaining_duration(RunLimit::Requests(100), Duration::from_secs(10)),
            None
        );
    }
}
