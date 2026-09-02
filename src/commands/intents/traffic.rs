use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eyre::{Result, WrapErr};
use indicatif::ProgressBar;

use super::execution::{ExecutionFeedback, execute_round_trip};
use super::route::{DiscoveryFeedback, discover_wallet, plan_sweep};
use super::types::{AssetType, OrderType};
use super::{
    IntentRuntime, IntentRuntimeArgs, confirm_execution, graceful_stop_flag, prepare_runtime,
};
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
    confirm_execution(
        runtime.auto_confirm,
        "Start continuous intent traffic until Ctrl-C?",
    )
    .await?;

    let stop = graceful_stop_flag();
    let mut stats = TrafficStats::default();
    while !stop.load(Ordering::Relaxed) {
        stats.cycles += 1;
        match run_cycle(&runtime, args.wallet_bps, &stop, &mut stats).await {
            Ok(true) => {}
            Ok(false) => wait_before_retry(&stop).await,
            Err(error) => {
                stats.failures += 1;
                ui::warn(&format!(
                    "traffic cycle {} will restart after an error: {}",
                    stats.cycles,
                    format_error(&error)
                ));
                wait_before_retry(&stop).await;
            }
        }
    }

    render_stats(&stats);
    Ok(())
}

async fn run_cycle(
    runtime: &IntentRuntime,
    wallet_bps: u16,
    stop: &AtomicBool,
    stats: &mut TrafficStats,
) -> Result<bool> {
    let mut found_routes = false;
    for (mode_index, mode) in TRAFFIC_MODES.into_iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
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
        )
        .await;
        if plans.is_empty() {
            ui::warn(&format!(
                "no quotable {} routes for {}",
                mode.asset_label(),
                mode.order_label()
            ));
            continue;
        }
        found_routes = true;
        let plan_index = next_plan_index(stats.route_cursors[mode_index], plans.len());
        stats.route_cursors[mode_index] = stats.route_cursors[mode_index].wrapping_add(1);
        let plan = &plans[plan_index];
        ui::info(&format!(
            "traffic cycle {}: {} route {}/{} using {} · {} -> {}",
            stats.cycles,
            mode.asset_label(),
            plan_index + 1,
            plans.len(),
            mode.order_label(),
            plan.from.label(),
            plan.to.label()
        ));

        let progress = traffic_progress(2);
        let feedback = ExecutionFeedback::Progress(progress.clone());
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
        if let Err(error) = result {
            progress.finish_and_clear();
            return Err(error).wrap_err_with(|| {
                format!(
                    "round trip {} -> {} did not complete",
                    plan.from.label(),
                    plan.to.label()
                )
            });
        }
        stats.round_trips += 1;
        progress.finish_and_clear();
    }
    Ok(found_routes)
}

const fn next_plan_index(cursor: usize, available: usize) -> usize {
    cursor % available
}

impl TrafficMode {
    const fn asset_label(self) -> &'static str {
        self.asset_type.label()
    }

    const fn order_label(self) -> &'static str {
        match self.order_type {
            OrderType::ExactInput => "exact input",
            OrderType::ExactOutput => "exact output",
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

fn traffic_progress(total: usize) -> ProgressBar {
    let progress = ProgressBar::new(total as u64);
    progress.set_style(
        ui::progress_bar_style(
            "  {spinner:.cyan} {bar:32.cyan/dim} {pos}/{len} {percent:>3}% {msg}",
        )
        .progress_chars("=> ")
        .tick_strings(&["|", "/", "-", "\\", ""]),
    );
    progress.set_message("simulating intent traffic");
    progress.enable_steady_tick(Duration::from_millis(100));
    progress
}

async fn wait_before_retry(stop: &AtomicBool) {
    if !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(RETRY_DELAY).await;
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
}
