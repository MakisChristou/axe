mod client;
mod execution;
mod route;
mod types;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, WrapErr, eyre};
use indicatif::ProgressBar;

use self::client::RfqClient;
use self::execution::{ExecutionFeedback, execute_leg, execute_round_trip};
use self::route::{
    DiscoveryFeedback, RouteDiscovery, discover_wallet, plan_roundtrip, plan_send, plan_sweep,
};
use self::types::{LegExecution, LegResult, RoutePlan, RunLimits};
use crate::config::ChainsConfig;
use crate::types::Network;
use crate::ui;

pub use self::types::{AssetSpec, HumanAmount, OrderType};

pub struct IntentRuntimeArgs {
    pub network: Network,
    pub config: PathBuf,
    pub private_key: String,
    pub poll_interval_secs: u64,
    pub fulfillment_timeout_secs: u64,
    pub yes: bool,
}

#[derive(Clone, Debug)]
pub enum RouteChoice {
    Random {
        wallet_bps: u16,
        order_type: OrderType,
    },
    Explicit {
        from: AssetSpec,
        to: AssetSpec,
        amount: Option<HumanAmount>,
        wallet_bps: u16,
        order_type: OrderType,
    },
}

impl RouteChoice {
    pub fn new(
        from: Option<AssetSpec>,
        to: Option<AssetSpec>,
        amount: Option<HumanAmount>,
        wallet_bps: u16,
        order_type: OrderType,
    ) -> Result<Self> {
        match (from, to, amount) {
            (None, None, None) => Ok(Self::Random {
                wallet_bps,
                order_type,
            }),
            (Some(from), Some(to), amount) => Ok(Self::Explicit {
                from,
                to,
                amount,
                wallet_bps,
                order_type,
            }),
            (None, None, Some(_)) => Err(eyre!("--amount requires --from and --to")),
            _ => Err(eyre!("--from and --to must be provided together")),
        }
    }
}

pub struct SendArgs {
    pub runtime: IntentRuntimeArgs,
    pub route: RouteChoice,
    pub recipient: Option<Address>,
}

pub struct RoundtripArgs {
    pub runtime: IntentRuntimeArgs,
    pub route: RouteChoice,
}

pub struct SweepArgs {
    pub runtime: IntentRuntimeArgs,
    pub sweeps: u64,
    pub continuous: bool,
    pub wallet_bps: u16,
    pub order_type: OrderType,
}

struct IntentRuntime {
    signer: PrivateKeySigner,
    config: ChainsConfig,
    client: RfqClient,
    limits: RunLimits,
    auto_confirm: bool,
}

pub async fn send(args: SendArgs) -> Result<()> {
    let runtime = prepare_runtime(args.runtime).await?;
    let discovery = discover_wallet(
        &runtime.client,
        &runtime.config,
        &runtime.signer,
        DiscoveryFeedback::Detailed,
    )
    .await?;
    let recipient = args.recipient.unwrap_or_else(|| runtime.signer.address());
    let plan = plan_send(
        &runtime.client,
        &discovery,
        runtime.signer.address(),
        recipient,
        &args.route,
    )
    .await?;
    ui::kv("mode", "one intent");
    ui::kv("intent deposits", "1");
    confirm_execution(runtime.auto_confirm, "Execute this intent?").await?;

    let result = execute_leg(
        &runtime.client,
        &discovery.chains,
        &runtime.signer,
        LegExecution {
            from: plan.from,
            to: plan.to,
            order_type: plan.order_type,
            amount: plan.requested_amount,
            recipient,
        },
        runtime.limits,
        &ExecutionFeedback::Detailed,
    )
    .await?;
    render_summary(std::slice::from_ref(&result), 1);
    Ok(())
}

pub async fn roundtrip(args: RoundtripArgs) -> Result<()> {
    let runtime = prepare_runtime(args.runtime).await?;
    let discovery = discover_wallet(
        &runtime.client,
        &runtime.config,
        &runtime.signer,
        DiscoveryFeedback::Detailed,
    )
    .await?;
    let plan = plan_roundtrip(
        &runtime.client,
        &discovery,
        runtime.signer.address(),
        &args.route,
    )
    .await?;
    ui::kv("mode", "one round trip");
    ui::kv("intent deposits", "2");
    confirm_execution(runtime.auto_confirm, "Execute this intent round trip?").await?;

    let mut results = Vec::new();
    let executed = execute_round_trip(
        &runtime.client,
        &discovery.chains,
        &runtime.signer,
        &plan,
        runtime.limits,
        &ExecutionFeedback::Detailed,
        &mut results,
    )
    .await;
    render_summary(&results, 2);
    executed
}

pub async fn sweep(args: SweepArgs) -> Result<()> {
    let runtime = prepare_runtime(args.runtime).await?;
    let stop = graceful_stop_flag();
    let mut results = Vec::new();
    let mut planned_intents = 0usize;
    let mut sweep = 0u64;
    let mut confirmed = runtime.auto_confirm;

    loop {
        sweep += 1;
        let discovery = discover_wallet(
            &runtime.client,
            &runtime.config,
            &runtime.signer,
            DiscoveryFeedback::Quiet,
        )
        .await?;
        let plans = plan_sweep(
            &runtime.client,
            &discovery,
            runtime.signer.address(),
            args.wallet_bps,
            args.order_type,
        )
        .await;
        if plans.is_empty() {
            render_summary(&results, planned_intents);
            return Err(eyre!(
                "no bidirectionally quotable routes are funded by the axe wallet"
            ));
        }
        let pass_intents = plans.len() * 2;
        planned_intents += pass_intents;
        ui::info(&format!(
            "sweep {sweep}: {} round trips, {pass_intents} intents",
            plans.len()
        ));
        if !confirmed {
            confirm_execution(false, "Execute this intent route sweep?").await?;
            confirmed = true;
        }

        let executed = execute_sweep_pass(&runtime, &discovery, &plans, &mut results, &stop).await;
        match executed {
            Ok(true) => {}
            Ok(false) => {
                render_summary(&results, planned_intents);
                return Ok(());
            }
            Err(error) => {
                render_summary(&results, planned_intents);
                return Err(error);
            }
        }

        if !args.continuous && sweep >= args.sweeps {
            break;
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }

    render_summary(&results, planned_intents);
    Ok(())
}

async fn execute_sweep_pass(
    runtime: &IntentRuntime,
    discovery: &RouteDiscovery,
    plans: &[RoutePlan],
    results: &mut Vec<LegResult>,
    stop: &AtomicBool,
) -> Result<bool> {
    let progress = sweep_progress(plans.len() * 2);
    let feedback = ExecutionFeedback::Progress(progress.clone());
    for plan in plans {
        if stop.load(Ordering::Relaxed) {
            progress.finish_and_clear();
            return Ok(false);
        }
        let executed = execute_round_trip(
            &runtime.client,
            &discovery.chains,
            &runtime.signer,
            plan,
            runtime.limits,
            &feedback,
            results,
        )
        .await;
        if let Err(error) = executed {
            progress.finish_and_clear();
            return Err(error).wrap_err_with(|| {
                format!(
                    "round trip {} -> {} did not complete",
                    plan.from.label(),
                    plan.to.label()
                )
            });
        }
    }
    progress.finish_and_clear();
    Ok(true)
}

fn sweep_progress(total: usize) -> ProgressBar {
    let progress = ProgressBar::new(total as u64);
    progress.set_style(
        ui::progress_bar_style(
            "  {spinner:.cyan} {bar:32.cyan/dim} {pos}/{len} {percent:>3}% {msg}",
        )
        .progress_chars("=> ")
        .tick_strings(&["|", "/", "-", "\\", ""]),
    );
    progress.set_message("starting intent sweep");
    progress.enable_steady_tick(Duration::from_millis(100));
    progress
}

async fn prepare_runtime(args: IntentRuntimeArgs) -> Result<IntentRuntime> {
    let signer: PrivateKeySigner = args
        .private_key
        .parse()
        .wrap_err("EVM_PRIVATE_KEY is not a valid hex private key")?;
    let config = ChainsConfig::load(&args.config).await?;
    let client = RfqClient::for_network(args.network)?;
    let limits = RunLimits {
        poll_interval: Duration::from_secs(args.poll_interval_secs),
        fulfillment_timeout: Duration::from_secs(args.fulfillment_timeout_secs),
    };
    Ok(IntentRuntime {
        signer,
        config,
        client,
        limits,
        auto_confirm: args.yes,
    })
}

async fn confirm_execution(auto_confirm: bool, prompt: &str) -> Result<()> {
    if auto_confirm || ui::confirm(prompt).await {
        return Ok(());
    }
    Err(eyre!(
        "execution not confirmed; pass --yes for non-interactive runs"
    ))
}

fn graceful_stop_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.store(true, Ordering::Relaxed);
            ui::warn("Ctrl-C received; finishing the current round trip before stopping");
        }
    });
    stop
}

fn render_summary(results: &[LegResult], planned: usize) {
    ui::section("intent summary");
    ui::kv(
        "completed intents",
        &format!(
            "{}/{} ({:.1}%)",
            results.len(),
            planned,
            completion_percentage(results.len(), planned)
        ),
    );
    if results.is_empty() {
        return;
    }
    let quote_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.quote_latency_ms)
        .collect();
    let fulfillment_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.fulfillment_latency_ms)
        .collect();
    let deposit_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.deposit_confirmation_latency_ms)
        .collect();
    let end_to_end_latencies: Vec<u64> = results
        .iter()
        .map(|result| result.end_to_end_latency_ms)
        .collect();
    ui::kv(
        "quote latency",
        &format_latency_percentiles(&quote_latencies),
    );
    ui::kv(
        "deposit confirmation",
        &format_latency_percentiles(&deposit_latencies),
    );
    ui::kv(
        "fulfillment latency",
        &format_latency_percentiles(&fulfillment_latencies),
    );
    ui::kv(
        "end-to-end latency",
        &format_latency_percentiles(&end_to_end_latencies),
    );
}

fn format_latency_percentiles(values: &[u64]) -> String {
    format!(
        "p50 {} │ p95 {}",
        ui::format_millis(percentile(values, 50)),
        ui::format_millis(percentile(values, 95))
    )
}

fn completion_percentage(completed: usize, planned: usize) -> f64 {
    if planned == 0 {
        return 0.0;
    }
    completed as f64 / planned as f64 * 100.0
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * percent)
        .div_ceil(100)
        .clamp(1, sorted.len());
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank_without_floats() {
        let values = [10, 20, 30, 40, 50];
        assert_eq!(percentile(&values, 50), 30);
        assert_eq!(percentile(&values, 95), 50);
    }

    #[test]
    fn completion_percentage_handles_empty_and_partial_runs() {
        assert_eq!(completion_percentage(0, 0), 0.0);
        assert_eq!(completion_percentage(3, 4), 75.0);
    }

    #[test]
    fn latency_percentiles_are_labeled_and_human_readable() {
        assert_eq!(
            format_latency_percentiles(&[181, 2_924, 8_823]),
            "p50 2.92 s │ p95 8.82 s"
        );
    }
}
