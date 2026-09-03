use std::time::Duration;

use eyre::{Result, WrapErr};
use indicatif::ProgressBar;

use super::execution::{ExecutionFeedback, execute_round_trip};
use super::presentation::set_intent_traffic_message;
use super::route::{DiscoveryFeedback, PlanningFeedback, discover_wallet, plan_sweep};
use super::types::{AssetType, LegResult, OrderType};
use super::{IntentRuntime, IntentRuntimeArgs, prepare_runtime};
use crate::shutdown::{DrainTarget, Shutdown};
use crate::ui;

const RETRY_DELAY: Duration = Duration::from_secs(5);

pub struct TrafficArgs {
    pub runtime: IntentRuntimeArgs,
    pub wallet_bps: u16,
    pub asset_type: Option<AssetType>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrafficMode {
    asset_type: AssetType,
    order_type: OrderType,
}

const TRAFFIC_MODES: [TrafficMode; 4] = [
    TrafficMode {
        asset_type: AssetType::Token,
        order_type: OrderType::ExactInput,
    },
    TrafficMode {
        asset_type: AssetType::Token,
        order_type: OrderType::ExactOutput,
    },
    TrafficMode {
        asset_type: AssetType::Native,
        order_type: OrderType::ExactInput,
    },
    TrafficMode {
        asset_type: AssetType::Native,
        order_type: OrderType::ExactOutput,
    },
];

#[derive(Default)]
struct TrafficStats {
    intents: u64,
    failures: u64,
    intent_latency_ms: u64,
    route_cursors: [usize; TRAFFIC_MODES.len()],
}

pub async fn run(args: TrafficArgs) -> Result<()> {
    let runtime = prepare_runtime(args.runtime).await?;
    render_strategy(args.wallet_bps, args.asset_type);
    let shutdown = Shutdown::install(DrainTarget::RoundTrip);
    let mut stats = TrafficStats::default();
    let progress = traffic_progress();
    set_traffic_status(&progress, &stats, "starting");
    while !shutdown.requested() {
        match run_cycle(
            &runtime,
            args.wallet_bps,
            args.asset_type,
            &shutdown,
            &mut stats,
            &progress,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => {
                set_traffic_status(&progress, &stats, "no quotable routes · retrying in 5s");
                wait_before_retry(&shutdown).await;
            }
            Err(error) => {
                stats.failures += 1;
                set_traffic_status(
                    &progress,
                    &stats,
                    &format!("retrying in 5s · {}", format_error(&error)),
                );
                wait_before_retry(&shutdown).await;
            }
        }
    }

    progress.finish_and_clear();
    render_stats(&stats);
    Ok(())
}

async fn run_cycle(
    runtime: &IntentRuntime,
    wallet_bps: u16,
    asset_type: Option<AssetType>,
    shutdown: &Shutdown,
    stats: &mut TrafficStats,
    progress: &ProgressBar,
) -> Result<bool> {
    let mut found_routes = false;
    for (mode_index, mode) in traffic_modes(asset_type) {
        if shutdown.requested() {
            break;
        }
        set_traffic_status(progress, stats, "discovering routes");
        let discovery = discover_wallet(
            &runtime.client,
            &runtime.config,
            runtime.signer.address(),
            DiscoveryFeedback::Quiet,
        )
        .await?;
        let plans = plan_sweep(
            &runtime.client,
            &discovery,
            runtime.signer.address(),
            mode.asset_type,
            wallet_bps,
            mode.order_type,
            PlanningFeedback::Hidden,
        )
        .await;
        if shutdown.requested() {
            return Ok(true);
        }
        if plans.is_empty() {
            set_traffic_status(progress, stats, "no quotable routes");
            continue;
        }
        found_routes = true;
        let plan_index = next_plan_index(stats.route_cursors[mode_index], plans.len());
        stats.route_cursors[mode_index] = stats.route_cursors[mode_index].wrapping_add(1);
        let plan = &plans[plan_index];
        let feedback = ExecutionFeedback::Traffic {
            progress: progress.clone(),
            context: traffic_context(stats, progress.elapsed()),
        };
        let mut results = Vec::with_capacity(2);
        let result = execute_round_trip(
            &runtime.client,
            &discovery.chains,
            &runtime.signer,
            plan,
            runtime.limits,
            &feedback,
            &mut results,
        )
        .await;
        record_intents(stats, &results);
        progress.set_position(stats.intents);
        if let Err(error) = result {
            return Err(error).wrap_err_with(|| {
                format!(
                    "round trip {} -> {} did not complete",
                    plan.from.label(),
                    plan.to.label()
                )
            });
        }
        set_traffic_status(progress, stats, "round trip complete");
    }
    Ok(found_routes)
}

fn traffic_modes(asset_type: Option<AssetType>) -> impl Iterator<Item = (usize, TrafficMode)> {
    TRAFFIC_MODES
        .into_iter()
        .enumerate()
        .filter(move |(_, mode)| asset_type.is_none_or(|selected| mode.asset_type == selected))
}

const fn next_plan_index(cursor: usize, available: usize) -> usize {
    cursor % available
}

fn render_strategy(wallet_bps: u16, asset_type: Option<AssetType>) {
    ui::section("intent traffic");
    ui::kv("strategy", "serial balance-returning round trips");
    let coverage = asset_type.map_or_else(
        || "all tokens and native assets · both order types".to_owned(),
        |asset_type| format!("{} assets only · both order types", asset_type.label()),
    );
    ui::kv("coverage", &coverage);
    ui::kv(
        "maximum route input",
        &format!("{:.2}% of spendable balance", f64::from(wallet_bps) / 100.0),
    );
    ui::kv("lifetime", "continuous until Ctrl-C");
}

fn render_stats(stats: &TrafficStats) {
    ui::section("intent traffic stopped");
    ui::kv("completed intents", &stats.intents.to_string());
    ui::kv("route failures", &stats.failures.to_string());
    if let Some(average) = average_intent_time(stats) {
        ui::kv("average intent time", &ui::format_duration(average));
    }
}

fn traffic_progress() -> ProgressBar {
    super::presentation::intent_traffic_bar()
}

fn traffic_context(stats: &TrafficStats, elapsed: Duration) -> String {
    let average = average_intent_time(stats)
        .map(|duration| format!(" · avg {}", compact_duration(duration)))
        .unwrap_or_default();
    format!(
        "{:.1} i/m · {} err{average}",
        intents_per_minute(stats, elapsed),
        stats.failures
    )
}

fn set_traffic_status(progress: &ProgressBar, stats: &TrafficStats, status: &str) {
    progress.set_position(stats.intents);
    set_intent_traffic_message(
        progress,
        &format!(
            "{} intents · {} · {status}",
            stats.intents,
            traffic_context(stats, progress.elapsed())
        ),
    );
}

fn record_intents(stats: &mut TrafficStats, results: &[LegResult]) {
    stats.intents = stats
        .intents
        .saturating_add(u64::try_from(results.len()).unwrap_or(u64::MAX));
    for result in results {
        stats.intent_latency_ms = stats
            .intent_latency_ms
            .saturating_add(result.end_to_end_latency_ms);
    }
}

fn average_intent_time(stats: &TrafficStats) -> Option<Duration> {
    (stats.intents > 0).then(|| Duration::from_millis(stats.intent_latency_ms / stats.intents))
}

fn intents_per_minute(stats: &TrafficStats, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        return 0.0;
    }
    stats.intents as f64 * 60.0 / elapsed.as_secs_f64()
}

fn compact_duration(duration: Duration) -> String {
    ui::format_duration(duration).replace(' ', "")
}

async fn wait_before_retry(shutdown: &Shutdown) {
    tokio::select! {
        () = tokio::time::sleep(RETRY_DELAY) => {}
        () = shutdown.cancelled() => {}
    }
}

fn format_error(error: &eyre::Report) -> String {
    ui::scrub_urls(&format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_rotates_through_every_asset_and_order_type() {
        let modes = traffic_modes(None)
            .map(|(_, mode)| mode)
            .collect::<Vec<_>>();
        assert_eq!(modes.len(), 4);
        assert!(modes.contains(&TrafficMode {
            asset_type: AssetType::Token,
            order_type: OrderType::ExactInput,
        }));
        assert!(modes.contains(&TrafficMode {
            asset_type: AssetType::Token,
            order_type: OrderType::ExactOutput,
        }));
        assert!(modes.contains(&TrafficMode {
            asset_type: AssetType::Native,
            order_type: OrderType::ExactInput,
        }));
        assert!(modes.contains(&TrafficMode {
            asset_type: AssetType::Native,
            order_type: OrderType::ExactOutput,
        }));
    }

    #[test]
    fn traffic_filters_modes_by_asset_type() {
        for asset_type in [AssetType::Token, AssetType::Native] {
            let modes = traffic_modes(Some(asset_type))
                .map(|(_, mode)| mode)
                .collect::<Vec<_>>();
            assert_eq!(modes.len(), 2);
            assert!(modes.iter().all(|mode| mode.asset_type == asset_type));
            assert!(
                modes
                    .iter()
                    .any(|mode| mode.order_type == OrderType::ExactInput)
            );
            assert!(
                modes
                    .iter()
                    .any(|mode| mode.order_type == OrderType::ExactOutput)
            );
        }
    }

    #[test]
    fn traffic_rotates_through_available_routes() {
        let indexes: Vec<usize> = (0..5).map(|cursor| next_plan_index(cursor, 3)).collect();
        assert_eq!(indexes, [0, 1, 2, 0, 1]);
    }

    #[test]
    fn traffic_errors_include_the_complete_cause_chain() {
        let error = Err::<(), _>(eyre::eyre!("deposit rejected"))
            .wrap_err("round trip failed")
            .unwrap_err();
        assert_eq!(format_error(&error), "round trip failed: deposit rejected");
    }

    #[test]
    fn traffic_context_contains_the_live_summary() {
        let stats = TrafficStats {
            intents: 4,
            failures: 1,
            intent_latency_ms: 300_000,
            ..TrafficStats::default()
        };
        let context = traffic_context(&stats, Duration::from_secs(120));

        assert_eq!(context, "2.0 i/m · 1 err · avg 1m15s");
    }
}
