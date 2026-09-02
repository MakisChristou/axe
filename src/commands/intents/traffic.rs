use std::time::Duration;

use eyre::{Result, WrapErr};
use indicatif::ProgressBar;

use super::execution::{ExecutionFeedback, execute_round_trip};
use super::route::{DiscoveryFeedback, PlanningFeedback, discover_wallet, plan_sweep};
use super::types::{AssetType, OrderType};
use super::{IntentRuntime, IntentRuntimeArgs, prepare_runtime};
use crate::shutdown::{DrainTarget, Shutdown};
use crate::ui;

const RETRY_DELAY: Duration = Duration::from_secs(5);

pub struct TrafficArgs {
    pub runtime: IntentRuntimeArgs,
    pub wallet_bps: u16,
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
    cycles: u64,
    round_trips: u64,
    intents: u64,
    failures: u64,
    route_cursors: [usize; TRAFFIC_MODES.len()],
}

pub async fn run(args: TrafficArgs) -> Result<()> {
    let runtime = prepare_runtime(args.runtime).await?;
    render_strategy(args.wallet_bps);
    let shutdown = Shutdown::install(DrainTarget::RoundTrip);
    let mut stats = TrafficStats::default();
    let progress = traffic_progress();
    set_traffic_status(&progress, &stats, "starting");
    while !shutdown.requested() {
        stats.cycles += 1;
        match run_cycle(&runtime, args.wallet_bps, &shutdown, &mut stats, &progress).await {
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
    shutdown: &Shutdown,
    stats: &mut TrafficStats,
    progress: &ProgressBar,
) -> Result<bool> {
    let mut found_routes = false;
    for (mode_index, mode) in TRAFFIC_MODES.into_iter().enumerate() {
        if shutdown.requested() {
            break;
        }
        set_traffic_status(
            progress,
            stats,
            &format!("{} · discovering routes", mode.short_label()),
        );
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
            set_traffic_status(
                progress,
                stats,
                &format!("{} · no quotable routes", mode.short_label()),
            );
            continue;
        }
        found_routes = true;
        let plan_index = next_plan_index(stats.route_cursors[mode_index], plans.len());
        stats.route_cursors[mode_index] = stats.route_cursors[mode_index].wrapping_add(1);
        let plan = &plans[plan_index];
        let feedback = ExecutionFeedback::Traffic {
            progress: progress.clone(),
            context: traffic_context(stats, mode, plan_index, plans.len()),
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
        stats.intents += results.len() as u64;
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
        stats.round_trips += 1;
        set_traffic_status(progress, stats, "round trip complete");
    }
    Ok(found_routes)
}

const fn next_plan_index(cursor: usize, available: usize) -> usize {
    cursor % available
}

impl TrafficMode {
    const fn short_label(self) -> &'static str {
        match (self.asset_type, self.order_type) {
            (AssetType::Token, OrderType::ExactInput) => "token/exact-in",
            (AssetType::Token, OrderType::ExactOutput) => "token/exact-out",
            (AssetType::Native, OrderType::ExactInput) => "native/exact-in",
            (AssetType::Native, OrderType::ExactOutput) => "native/exact-out",
        }
    }
}

fn render_strategy(wallet_bps: u16) {
    ui::section("intent traffic");
    ui::kv("strategy", "serial balance-returning round trips");
    ui::kv(
        "coverage",
        "all tokens and native assets · both order types",
    );
    ui::kv(
        "maximum route input",
        &format!("{:.2}% of spendable balance", f64::from(wallet_bps) / 100.0),
    );
    ui::kv("lifetime", "continuous until Ctrl-C");
}

fn render_stats(stats: &TrafficStats) {
    ui::section("intent traffic stopped");
    ui::kv("cycles started", &stats.cycles.to_string());
    ui::kv("completed round trips", &stats.round_trips.to_string());
    ui::kv("completed intents", &stats.intents.to_string());
    ui::kv("route failures", &stats.failures.to_string());
}

fn traffic_progress() -> ProgressBar {
    super::presentation::intent_traffic_bar()
}

fn traffic_context(
    stats: &TrafficStats,
    mode: TrafficMode,
    plan_index: usize,
    plan_count: usize,
) -> String {
    format!(
        "trips {} · errors {} · cycle {} · {} {}/{}",
        stats.round_trips,
        stats.failures,
        stats.cycles,
        mode.short_label(),
        plan_index + 1,
        plan_count
    )
}

fn set_traffic_status(progress: &ProgressBar, stats: &TrafficStats, status: &str) {
    progress.set_position(stats.intents);
    progress.set_message(format!(
        "intents {} · trips {} · errors {} · cycle {} · {status}",
        stats.intents, stats.round_trips, stats.failures, stats.cycles
    ));
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
        assert_eq!(TRAFFIC_MODES.len(), 4);
        assert!(TRAFFIC_MODES.contains(&TrafficMode {
            asset_type: AssetType::Token,
            order_type: OrderType::ExactInput,
        }));
        assert!(TRAFFIC_MODES.contains(&TrafficMode {
            asset_type: AssetType::Token,
            order_type: OrderType::ExactOutput,
        }));
        assert!(TRAFFIC_MODES.contains(&TrafficMode {
            asset_type: AssetType::Native,
            order_type: OrderType::ExactInput,
        }));
        assert!(TRAFFIC_MODES.contains(&TrafficMode {
            asset_type: AssetType::Native,
            order_type: OrderType::ExactOutput,
        }));
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
            cycles: 3,
            round_trips: 4,
            failures: 1,
            ..TrafficStats::default()
        };
        let context = traffic_context(&stats, TRAFFIC_MODES[0], 1, 3);

        assert_eq!(context, "trips 4 · errors 1 · cycle 3 · token/exact-in 2/3");
    }
}
