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
use futures::{StreamExt, stream};
use rand::seq::SliceRandom;
use tokio::sync::Mutex;
use tokio::time::{Interval, MissedTickBehavior};

use self::progress::BenchmarkProgress;
use self::report::{duration_ms, render_report, report_json};
use self::reservoir::SampleReservoir;
use self::types::{
    BenchmarkReport, BenchmarkSelection, BenchmarkTarget, FailureKind, Sample, SampleOutcome,
};
use super::client::RfqClient;
use super::presentation::IntentActivity;
use super::read::{QuoteRequestArgs, api_client, prepare_quote_from_tokens};
use super::route::validate_quote_route;
use super::types::{
    AssetSpec, QuoteOutcome, TokenInfo, TokensResponse, is_native_token, parse_amount,
};
use crate::shutdown::{DrainTarget, Shutdown};

pub use self::types::{
    QuoteBenchmarkArgs, QuoteBenchmarkLimit, QuoteBenchmarkMode, QuoteBenchmarkTarget,
};

struct BenchmarkTargets {
    targets: Vec<BenchmarkTarget>,
    selection: BenchmarkSelection,
}

impl BenchmarkTargets {
    fn coverage_label(&self) -> String {
        match &self.selection {
            BenchmarkSelection::Fixed => "fixed route".to_owned(),
            BenchmarkSelection::Randomized {
                bidirectional_routes,
                ..
            } => format!("{bidirectional_routes} routes ↔"),
        }
    }
}

pub async fn benchmark_quotes(args: QuoteBenchmarkArgs) -> eyre::Result<()> {
    let activity = IntentActivity::new("Finding quote benchmark routes…", !args.json);
    let client = api_client(&args.api)?;
    let targets =
        Arc::new(resolve_benchmark_targets(&client, &args.target, args.concurrency).await?);
    drop(activity);
    let shutdown = Shutdown::install(DrainTarget::QuoteRequests);
    run_warmup(&client, &targets, &args, Arc::clone(&shutdown)).await?;
    let report = run_benchmark(
        client,
        targets,
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
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report_json(&report))?);
    } else {
        render_report(&report, args.concurrency, args.warmup, args.max_rps);
    }
    Ok(())
}

async fn run_warmup(
    client: &RfqClient,
    targets: &Arc<BenchmarkTargets>,
    args: &QuoteBenchmarkArgs,
    shutdown: Arc<Shutdown>,
) -> Result<()> {
    if args.warmup == 0 {
        return Ok(());
    }
    run_benchmark(
        client.clone(),
        Arc::clone(targets),
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
    .await?;
    Ok(())
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
    targets: Arc<BenchmarkTargets>,
    shutdown: Arc<Shutdown>,
    args: BenchmarkRunArgs,
) -> Result<BenchmarkReport> {
    let started = Instant::now();
    let next_request = Arc::new(AtomicU64::new(0));
    let rate_limit = rate_limiter(args.max_rps);
    let progress = Arc::new(BenchmarkProgress::new(
        args.limit,
        args.phase,
        targets.coverage_label(),
        args.show_progress,
    ));
    let samples = Arc::new(SampleReservoir::new());
    let workers = (0..args.concurrency).map(|_| {
        benchmark_worker(BenchmarkWorker {
            client: client.clone(),
            targets: Arc::clone(&targets),
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
    let primary = targets
        .targets
        .first()
        .ok_or_else(|| eyre!("benchmark resolved no quote targets"))?;
    Ok(BenchmarkReport {
        mode: args.limit.mode(),
        interrupted: shutdown.requested(),
        selection: targets.selection.clone(),
        counts: progress.counts(),
        samples: samples.snapshot(),
        elapsed: started.elapsed(),
        output_symbol: primary.output_symbol.clone(),
        output_decimals: primary.output_decimals,
        from_label: primary.from_label.clone(),
        to_label: primary.to_label.clone(),
        requested_amount: primary.requested_amount,
        requested_symbol: primary.requested_symbol.clone(),
        requested_decimals: primary.requested_decimals,
    })
}

struct BenchmarkWorker {
    client: RfqClient,
    targets: Arc<BenchmarkTargets>,
    limit: QuoteBenchmarkLimit,
    started: Instant,
    request_timeout: Duration,
    next_request: Arc<AtomicU64>,
    rate_limit: Option<Arc<Mutex<Interval>>>,
    shutdown: Arc<Shutdown>,
    progress: Arc<BenchmarkProgress>,
    samples: Arc<SampleReservoir>,
}

async fn resolve_benchmark_targets(
    client: &RfqClient,
    target: &QuoteBenchmarkTarget,
    concurrency: usize,
) -> Result<BenchmarkTargets> {
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
    if target.from.is_some() || target.to.is_some() {
        return resolve_fixed_target(client, &tokens, target, amount, pairs).await;
    }

    let amount_label = amount.to_string();
    let mut valid_targets = Vec::new();
    let mut last_failure = None;
    let candidates = stream::iter(pairs)
        .map(|(from, to)| resolve_candidate(client, &tokens, target, &amount, from, to))
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await;
    for candidate in candidates {
        match candidate {
            Ok(resolved) => valid_targets.push(resolved),
            Err(failure) => last_failure = Some(failure),
        }
    }
    let targets = shuffled_bidirectional_targets(valid_targets);
    if targets.is_empty() {
        return Err(no_valid_route_error("bidirectional", last_failure));
    }
    let bidirectional_routes = targets.len() / 2;
    Ok(BenchmarkTargets {
        targets,
        selection: BenchmarkSelection::Randomized {
            bidirectional_routes,
            amount: amount_label,
            asset_type: target.asset_type,
        },
    })
}

async fn resolve_fixed_target(
    client: &RfqClient,
    tokens: &TokensResponse,
    target: &QuoteBenchmarkTarget,
    amount: super::types::HumanAmount,
    pairs: Vec<(&TokenInfo, &TokenInfo)>,
) -> Result<BenchmarkTargets> {
    let mut last_failure = None;
    for (from, to) in pairs {
        match resolve_candidate(client, tokens, target, &amount, from, to).await {
            Ok(resolved) => {
                return Ok(BenchmarkTargets {
                    targets: vec![resolved],
                    selection: BenchmarkSelection::Fixed,
                });
            }
            Err(failure) => last_failure = Some(failure),
        }
    }
    Err(no_valid_route_error("matching", last_failure))
}

async fn resolve_candidate(
    client: &RfqClient,
    tokens: &TokensResponse,
    target: &QuoteBenchmarkTarget,
    amount: &super::types::HumanAmount,
    from: &TokenInfo,
    to: &TokenInfo,
) -> std::result::Result<BenchmarkTarget, String> {
    let request = QuoteRequestArgs {
        from: token_spec(from).map_err(|error| error.to_string())?,
        to: token_spec(to).map_err(|error| error.to_string())?,
        amount: amount.clone(),
        sender: target.sender,
        recipient: target.recipient,
        order_type: target.order_type,
    };
    let prepared =
        prepare_quote_from_tokens(tokens, &request).map_err(|error| error.to_string())?;
    match client.quote(&prepared.request).await {
        Ok(QuoteOutcome::Available(quote)) => validate_quote_route(
            &quote.quote,
            request.from.id(),
            request.to.id(),
            target.order_type,
            prepared.requested_amount,
        )
        .map(|()| BenchmarkTarget::from(prepared))
        .map_err(|_| "solver returned a mismatched quote".to_owned()),
        Ok(QuoteOutcome::Unavailable(reason)) => Err(reason),
        Err(error) => Err(error.to_string()),
    }
}

fn no_valid_route_error(kind: &str, last_failure: Option<String>) -> eyre::Report {
    eyre!(
        "No {kind} route returned a valid quote{}",
        last_failure.map_or_else(String::new, |failure| format!(": {failure}"))
    )
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

fn shuffled_bidirectional_targets(mut targets: Vec<BenchmarkTarget>) -> Vec<BenchmarkTarget> {
    let mut routes = Vec::new();
    while let Some(forward) = targets.pop() {
        let Some(reverse_index) = targets
            .iter()
            .position(|candidate| routes_are_reversed(&forward, candidate))
        else {
            continue;
        };
        let reverse = targets.swap_remove(reverse_index);
        let mut route = [forward, reverse];
        if rand::random() {
            route.swap(0, 1);
        }
        routes.push(route);
    }
    routes.shuffle(&mut rand::thread_rng());
    routes.into_iter().flatten().collect()
}

fn routes_are_reversed(left: &BenchmarkTarget, right: &BenchmarkTarget) -> bool {
    left.from == right.to && left.to == right.from
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
        let Some(target) = scheduled_target(&worker.targets.targets, request_index) else {
            break;
        };
        let sample = benchmark_request(&worker.client, target, worker.request_timeout).await;
        worker.progress.record(&sample);
        worker.samples.record(sample);
    }
}

fn scheduled_target(targets: &[BenchmarkTarget], request_index: u64) -> Option<&BenchmarkTarget> {
    let target_count = u64::try_from(targets.len()).ok()?;
    if target_count == 0 {
        return None;
    }
    let index = usize::try_from(request_index % target_count).ok()?;
    targets.get(index)
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

    fn benchmark_target(from_chain: &str, to_chain: &str) -> BenchmarkTarget {
        let from_token = format!("token-{from_chain}");
        let to_token = format!("token-{to_chain}");
        BenchmarkTarget {
            request: super::super::types::QuoteRequest {
                from_chain: from_chain.to_owned(),
                from_token: from_token.clone(),
                to_chain: to_chain.to_owned(),
                to_token: to_token.clone(),
                amount: "1".to_owned(),
                order_type: super::super::types::OrderType::ExactInput,
                sender: alloy::primitives::Address::ZERO.to_string(),
                recipient: alloy::primitives::Address::ZERO.to_string(),
            },
            from: super::super::types::AssetId {
                chain_id: from_chain.to_owned(),
                token_address: from_token,
            },
            to: super::super::types::AssetId {
                chain_id: to_chain.to_owned(),
                token_address: to_token,
            },
            requested_amount: alloy::primitives::U256::from(1),
            order_type: super::super::types::OrderType::ExactInput,
            output_symbol: "USDC".to_owned(),
            output_decimals: 6,
            from_label: from_chain.to_owned(),
            to_label: to_chain.to_owned(),
            requested_symbol: "USDC".to_owned(),
            requested_decimals: 6,
        }
    }

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

    #[test]
    fn automatic_benchmark_keeps_balanced_bidirectional_routes() {
        let targets = shuffled_bidirectional_targets(vec![
            benchmark_target("chain-a", "chain-b"),
            benchmark_target("chain-b", "chain-a"),
            benchmark_target("chain-a", "chain-c"),
        ]);

        assert_eq!(targets.len(), 2);
        assert!(routes_are_reversed(&targets[0], &targets[1]));
        assert_eq!(
            scheduled_target(&targets, 2).map(|target| &target.from),
            Some(&targets[0].from)
        );
    }
}
