mod progress;
mod report;
mod reservoir;
mod types;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use eyre::{Result, eyre};
use futures::future::join_all;
use tokio::sync::Mutex;
use tokio::time::{Interval, MissedTickBehavior};

use self::progress::BenchmarkProgress;
use self::report::{duration_ms, render_report, report_json};
use self::reservoir::SampleReservoir;
use self::types::{BenchmarkReport, BenchmarkTarget, FailureKind, Sample, SampleOutcome};
use super::client::RfqClient;
use super::read::{PreparedQuote, QuoteRequestArgs, api_client, prepare_quote_from_tokens};
use super::route::validate_quote_route;
use super::types::{
    AssetSpec, QuoteOutcome, TokenInfo, TokensResponse, is_native_token, parse_amount,
};
use crate::shutdown::{DrainTarget, Shutdown};

pub use self::types::{
    QuoteBenchmarkArgs, QuoteBenchmarkLimit, QuoteBenchmarkMode, QuoteBenchmarkTarget,
};

pub async fn benchmark_quotes(args: QuoteBenchmarkArgs) -> eyre::Result<()> {
    let client = api_client(&args.api)?;
    let prepared = resolve_benchmark_target(&client, &args.target).await?;
    let target = Arc::new(BenchmarkTarget::from(prepared));
    let shutdown = Shutdown::install(DrainTarget::QuoteRequests);
    run_warmup(&client, &target, &args, Arc::clone(&shutdown)).await;
    let report = run_benchmark(
        client,
        target,
        shutdown,
        BenchmarkRunArgs {
            limit: args.limit,
            concurrency: args.concurrency,
            request_timeout: args.request_timeout,
            max_rps: args.max_rps,
            phase: "benchmark",
            show_progress: !args.json,
        },
    )
    .await;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
    } else {
        render_report(&report, args.concurrency, args.warmup, args.max_rps);
    }
    Ok(())
}

async fn run_warmup(
    client: &RfqClient,
    target: &Arc<BenchmarkTarget>,
    args: &QuoteBenchmarkArgs,
    shutdown: Arc<Shutdown>,
) {
    if args.warmup == 0 {
        return;
    }
    run_benchmark(
        client.clone(),
        Arc::clone(target),
        shutdown,
        BenchmarkRunArgs {
            limit: QuoteBenchmarkLimit::Requests(args.warmup),
            concurrency: args.concurrency,
            request_timeout: args.request_timeout,
            max_rps: args.max_rps,
            phase: "warmup",
            show_progress: !args.json,
        },
    )
    .await;
}

struct BenchmarkRunArgs {
    limit: QuoteBenchmarkLimit,
    concurrency: usize,
    request_timeout: Duration,
    max_rps: Option<u64>,
    phase: &'static str,
    show_progress: bool,
}

async fn run_benchmark(
    client: RfqClient,
    target: Arc<BenchmarkTarget>,
    shutdown: Arc<Shutdown>,
    args: BenchmarkRunArgs,
) -> BenchmarkReport {
    let started = Instant::now();
    let next_request = Arc::new(AtomicU64::new(0));
    let rate_limit = rate_limiter(args.max_rps);
    let progress = Arc::new(BenchmarkProgress::new(
        args.limit,
        args.phase,
        args.show_progress,
    ));
    let samples = Arc::new(SampleReservoir::new());
    let workers = (0..args.concurrency).map(|_| {
        benchmark_worker(BenchmarkWorker {
            client: client.clone(),
            target: Arc::clone(&target),
            limit: args.limit,
            started,
            request_timeout: args.request_timeout,
            next_request: Arc::clone(&next_request),
            rate_limit: rate_limit.clone(),
            shutdown: Arc::clone(&shutdown),
            progress: Arc::clone(&progress),
            samples: Arc::clone(&samples),
        })
    });
    join_all(workers).await;
    progress.finish();
    BenchmarkReport {
        mode: args.limit.mode(),
        interrupted: shutdown.requested(),
        counts: progress.counts(),
        samples: samples.snapshot(),
        elapsed: started.elapsed(),
        output_symbol: target.output_symbol.clone(),
        output_decimals: target.output_decimals,
        from_label: target.from_label.clone(),
        to_label: target.to_label.clone(),
        requested_amount: target.requested_amount,
        requested_symbol: target.requested_symbol.clone(),
        requested_decimals: target.requested_decimals,
    }
}

struct BenchmarkWorker {
    client: RfqClient,
    target: Arc<BenchmarkTarget>,
    limit: QuoteBenchmarkLimit,
    started: Instant,
    request_timeout: Duration,
    next_request: Arc<AtomicU64>,
    rate_limit: Option<Arc<Mutex<Interval>>>,
    shutdown: Arc<Shutdown>,
    progress: Arc<BenchmarkProgress>,
    samples: Arc<SampleReservoir>,
}

async fn resolve_benchmark_target(
    client: &RfqClient,
    target: &QuoteBenchmarkTarget,
) -> Result<PreparedQuote> {
    let tokens = client.tokens().await?;
    let pairs = candidate_pairs(&tokens, target);
    if pairs.is_empty() {
        return Err(eyre!(
            "No cross-chain {} pairs match the benchmark overrides",
            target.asset_type.label()
        ));
    }
    let amount = target
        .amount
        .clone()
        .map_or_else(|| "1".parse(), Ok)
        .map_err(eyre::Report::msg)?;
    let mut last_failure = None;
    for (from, to) in pairs {
        let request = QuoteRequestArgs {
            from: token_spec(from)?,
            to: token_spec(to)?,
            amount: amount.clone(),
            sender: target.sender,
            recipient: target.recipient,
            order_type: target.order_type,
        };
        let prepared = prepare_quote_from_tokens(&tokens, &request)?;
        match client.quote(&prepared.request).await {
            Ok(QuoteOutcome::Available(quote)) => {
                if validate_quote_route(
                    &quote.quote,
                    request.from.id(),
                    request.to.id(),
                    target.order_type,
                    prepared.requested_amount,
                )
                .is_ok()
                {
                    return Ok(prepared);
                }
                last_failure = Some("solver returned a mismatched quote".to_owned());
            }
            Ok(QuoteOutcome::Unavailable(reason)) => last_failure = Some(reason),
            Err(error) => last_failure = Some(error.to_string()),
        }
    }
    Err(eyre!(
        "No matching route returned a valid quote{}",
        last_failure.map_or_else(String::new, |failure| format!(": {failure}"))
    ))
}

fn candidate_pairs<'a>(
    tokens: &'a TokensResponse,
    target: &QuoteBenchmarkTarget,
) -> Vec<(&'a TokenInfo, &'a TokenInfo)> {
    let mut pairs = tokens
        .tokens
        .iter()
        .filter(|token| target.asset_type == token_asset_type(token))
        .filter(|token| {
            target
                .from
                .as_ref()
                .is_none_or(|asset| token_matches(token, asset))
        })
        .flat_map(|from| {
            tokens
                .tokens
                .iter()
                .filter(|to| target.asset_type == token_asset_type(to))
                .filter(|to| from.chain_id != to.chain_id)
                .filter(|to| {
                    target
                        .to
                        .as_ref()
                        .is_none_or(|asset| token_matches(to, asset))
                })
                .map(move |to| (from, to))
        })
        .collect::<Vec<_>>();
    pairs.sort_by(|left, right| {
        (
            left.0.symbol != left.1.symbol,
            &left.0.symbol,
            &left.0.chain_id,
            &left.1.chain_id,
        )
            .cmp(&(
                right.0.symbol != right.1.symbol,
                &right.0.symbol,
                &right.0.chain_id,
                &right.1.chain_id,
            ))
    });
    pairs
}

fn token_asset_type(token: &TokenInfo) -> super::types::AssetType {
    if is_native_token(&token.address) {
        super::types::AssetType::Native
    } else {
        super::types::AssetType::Token
    }
}

fn token_matches(token: &TokenInfo, asset: &AssetSpec) -> bool {
    token.chain_id == asset.id().chain_id
        && token
            .address
            .eq_ignore_ascii_case(&asset.id().token_address)
}

fn token_spec(token: &TokenInfo) -> Result<AssetSpec> {
    format!("{}/{}", token.chain_id, token.address)
        .parse()
        .map_err(eyre::Report::msg)
}

async fn benchmark_worker(worker: BenchmarkWorker) {
    loop {
        if worker.shutdown.requested() {
            break;
        }
        let request_index = worker.next_request.fetch_add(1, Ordering::Relaxed);
        if !should_start(worker.limit, request_index, worker.started.elapsed()) {
            break;
        }
        if !wait_for_rate_limit(
            worker.rate_limit.as_ref(),
            remaining_duration(worker.limit, worker.started.elapsed()),
            &worker.shutdown,
        )
        .await
        {
            break;
        }
        let sample =
            benchmark_request(&worker.client, &worker.target, worker.request_timeout).await;
        worker.progress.record(&sample);
        worker.samples.record(sample);
    }
}

fn should_start(limit: QuoteBenchmarkLimit, request_index: u64, elapsed: Duration) -> bool {
    match limit {
        QuoteBenchmarkLimit::Requests(requests) => request_index < requests,
        QuoteBenchmarkLimit::Duration(duration) => elapsed < duration,
        QuoteBenchmarkLimit::Continuous => true,
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
    shutdown: &Shutdown,
) -> bool {
    if shutdown.requested() {
        return false;
    }
    let Some(rate_limit) = rate_limit else {
        return remaining.is_none_or(|remaining| !remaining.is_zero());
    };
    let wait = async {
        rate_limit.lock().await.tick().await;
    };
    let rate_ready = async {
        match remaining {
            Some(remaining) => tokio::time::timeout(remaining, wait).await.is_ok(),
            None => {
                wait.await;
                true
            }
        }
    };
    tokio::select! {
        ready = rate_ready => ready,
        () = shutdown.cancelled() => false,
    }
}

fn remaining_duration(limit: QuoteBenchmarkLimit, elapsed: Duration) -> Option<Duration> {
    match limit {
        QuoteBenchmarkLimit::Requests(_) => None,
        QuoteBenchmarkLimit::Duration(duration) => Some(duration.saturating_sub(elapsed)),
        QuoteBenchmarkLimit::Continuous => None,
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

    fn token(chain_id: &str, address: &str, symbol: &str) -> TokenInfo {
        TokenInfo {
            chain_id: chain_id.to_owned(),
            address: address.to_owned(),
            symbol: symbol.to_owned(),
            decimals: 6,
        }
    }

    fn automatic_target(asset_type: super::super::types::AssetType) -> QuoteBenchmarkTarget {
        QuoteBenchmarkTarget {
            from: None,
            to: None,
            amount: None,
            sender: alloy::primitives::Address::ZERO,
            recipient: alloy::primitives::Address::ZERO,
            order_type: super::super::types::OrderType::ExactInput,
            asset_type,
        }
    }

    #[test]
    fn fixed_and_duration_limits_stop_scheduling() {
        assert!(should_start(
            QuoteBenchmarkLimit::Requests(100),
            99,
            Duration::ZERO
        ));
        assert!(!should_start(
            QuoteBenchmarkLimit::Requests(100),
            100,
            Duration::ZERO
        ));
        assert!(should_start(
            QuoteBenchmarkLimit::Duration(Duration::from_secs(1)),
            1_000,
            Duration::from_millis(999)
        ));
        assert!(!should_start(
            QuoteBenchmarkLimit::Duration(Duration::from_secs(1)),
            1_000,
            Duration::from_secs(1)
        ));
        assert_eq!(
            remaining_duration(
                QuoteBenchmarkLimit::Duration(Duration::from_secs(1)),
                Duration::from_millis(750)
            ),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            remaining_duration(QuoteBenchmarkLimit::Requests(100), Duration::from_secs(10)),
            None
        );
    }

    #[test]
    fn automatic_targets_prefer_same_symbol_cross_chain_pairs() {
        let tokens = TokensResponse {
            tokens: vec![
                token(
                    "eip155:2",
                    "0x0000000000000000000000000000000000000002",
                    "USDC",
                ),
                token(
                    "eip155:1",
                    "0x0000000000000000000000000000000000000001",
                    "USDC",
                ),
                token(
                    "eip155:1",
                    "0x0000000000000000000000000000000000000000",
                    "ETH",
                ),
            ],
        };

        let token_pairs = candidate_pairs(
            &tokens,
            &automatic_target(super::super::types::AssetType::Token),
        );
        assert_eq!(token_pairs[0].0.symbol, "USDC");
        assert_eq!(token_pairs[0].1.symbol, "USDC");
        assert_eq!(token_pairs.len(), 2);

        let native_pairs = candidate_pairs(
            &tokens,
            &automatic_target(super::super::types::AssetType::Native),
        );
        assert!(native_pairs.is_empty());
    }
}
