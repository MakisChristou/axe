mod progress;
mod report;
mod scheduler;
mod types;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::{Result, eyre};
use indicatif::ProgressBar;

use self::types::{SchedulerArgs, SourceReport, SourceState, StressLimits, StressTelemetry};
use super::execution::{ExecutionFeedback, prepare_stress_approval};
use super::execution_lock::ExecutionLock;
use super::route::{DiscoveryFeedback, discover_wallet, plan_stress_routes};
use super::types::{ChainRuntime, LegPlan, format_units};
use super::{IntentRuntime, prepare_runtime};
use crate::evm::EvmEndpoints;
use crate::evm::pipeline::{PipelineLimits, PipelinedSender};
use crate::shutdown::{DrainTarget, Shutdown};
use crate::types::Network;
use crate::ui;

pub use self::types::StressArgs;

pub async fn run(args: StressArgs) -> Result<()> {
    let telemetry = Arc::new(StressTelemetry::default());
    let startup = if args.json {
        ProgressBar::hidden()
    } else {
        ui::wait_spinner("Loading intent configuration…")
    };
    let result = ui::count_warnings(
        Arc::clone(&telemetry.warnings),
        run_stress(args, telemetry, &startup),
    )
    .await;
    startup.finish_and_clear();
    result
}

async fn run_stress(
    args: StressArgs,
    telemetry: Arc<StressTelemetry>,
    startup: &ProgressBar,
) -> Result<()> {
    if args.runtime.network != Network::Testnet {
        return Err(eyre!("intent stress is testnet-only"));
    }
    let runtime = prepare_runtime(args.runtime.clone()).await?;
    startup.set_message("Locking wallet against other Axe intent runners…");
    let _execution_lock = ExecutionLock::acquire(runtime.signer.address())?;
    startup.set_message("Wallet locked · checking funded chains…");
    let discovery = discover_wallet(
        &runtime.client,
        &runtime.config,
        runtime.signer.address(),
        DiscoveryFeedback::Quiet,
    )
    .await?;
    startup.set_message(format!("Finding available {} routes…", args.symbol.trim()));
    let routes = plan_stress_routes(
        &runtime.client,
        &discovery,
        runtime.signer.address(),
        args.symbol.trim(),
        &args.amount,
    )
    .await?;
    let limits = resolve_limits(&args, &routes)?;
    let shutdown = Shutdown::install(DrainTarget::IntentStress);
    let sources = prepare_sources(&runtime, &discovery.chains, routes, &limits, startup).await?;
    startup.finish_and_clear();
    if !args.json {
        render_plan(&sources, &limits);
    }
    let progress = if args.json {
        indicatif::ProgressBar::hidden()
    } else {
        progress::bar()
    };
    let run = scheduler::run(SchedulerArgs {
        client: runtime.client,
        sources,
        wallet: runtime.signer.address(),
        limits: limits.clone(),
        shutdown,
        progress: progress.clone(),
        telemetry,
    })
    .await;
    progress.finish_and_clear();
    report::render(&run, &limits, args.json)?;
    if run.state.failed > 0 {
        return Err(eyre!(
            "{} deposits failed or remain unconfirmed",
            run.state.failed
        ));
    }
    Ok(())
}

fn resolve_limits(args: &StressArgs, routes: &[LegPlan]) -> Result<StressLimits> {
    let first = routes.first().ok_or_else(|| {
        eyre!(
            "no funded {} source routes returned quotes",
            args.symbol.trim()
        )
    })?;
    let decimals = first.from.decimals;
    if routes.iter().any(|route| route.from.decimals != decimals) {
        return Err(eyre!("stress input assets must have consistent decimals"));
    }
    let amount = args.amount.to_base_units(decimals)?;
    let max_volume = args.max_volume.to_base_units(decimals)?;
    let max_native_spend = args.max_native_spend.to_base_units(18)?;
    if amount.is_zero() || max_volume < amount || max_native_spend.is_zero() {
        return Err(eyre!(
            "amount and gas budget must be positive, and max-volume must cover one deposit"
        ));
    }
    Ok(StressLimits {
        duration: args.duration,
        max_intents: args.max_intents,
        max_in_flight: args.max_in_flight,
        amount,
        max_volume,
        max_native_spend,
        min_native_balance: args.min_native_balance.to_base_units(18)?,
        decimals,
        symbol: args.symbol.trim().to_owned(),
    })
}

async fn prepare_sources(
    runtime: &IntentRuntime,
    chains: &std::collections::HashMap<String, ChainRuntime>,
    routes: Vec<LegPlan>,
    limits: &StressLimits,
    startup: &ProgressBar,
) -> Result<Vec<SourceState>> {
    let mut grouped = BTreeMap::<String, Vec<LegPlan>>::new();
    let mut approved = HashSet::new();
    for plan in routes {
        if approved.insert((plan.from.id.clone(), plan.settlement_contract)) {
            startup.set_message(format!("Checking allowance · {}…", plan.from.label()));
            prepare_stress_approval(
                chains,
                &runtime.signer,
                &plan,
                limits.max_volume,
                &ExecutionFeedback::Startup(startup.clone()),
            )
            .await?;
        }
        grouped
            .entry(plan.from.id.chain_id.clone())
            .or_default()
            .push(plan);
    }
    grouped
        .into_iter()
        .map(|(chain_id, routes)| {
            let chain = chains
                .get(&chain_id)
                .ok_or_else(|| eyre!("missing RPC for {chain_id}"))?;
            let sender = PipelinedSender::new(
                EvmEndpoints::connect(std::slice::from_ref(&chain.rpc_url))?,
                runtime.signer.clone(),
                PipelineLimits {
                    native_reserve: limits.min_native_balance,
                    gas_budget: limits.max_native_spend,
                    receipt_timeout: Duration::from_secs(90),
                },
            );
            Ok(SourceState {
                routes,
                sender: Arc::new(sender),
                cursor: 0,
                ready_at: Instant::now(),
                report: SourceReport {
                    chain: chain.label.clone(),
                    ..Default::default()
                },
            })
        })
        .collect()
}

fn render_plan(sources: &[SourceState], limits: &StressLimits) {
    ui::section("intent deposit stress");
    ui::kv(
        "sources",
        &sources
            .iter()
            .map(|source| source.report.chain.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    );
    ui::kv(
        "sending",
        &format!(
            "{} {} per deposit | {} concurrent jobs",
            format_units(limits.amount, limits.decimals),
            limits.symbol,
            limits.max_in_flight
        ),
    );
    ui::kv(
        "limits",
        &format!(
            "{} deposits | {} {} input | {}",
            limits.max_intents,
            format_units(limits.max_volume, limits.decimals),
            limits.symbol,
            ui::format_duration(limits.duration)
        ),
    );
    ui::kv(
        "gas per chain",
        &format!(
            "{} native deposit budget | {} native balance reserve (approvals extra)",
            format_units(limits.max_native_spend, 18),
            format_units(limits.min_native_balance, 18)
        ),
    );
    ui::info(
        "Broadcasts overlap receipt waits. Ctrl-C stops new deposits and drains pending receipts.",
    );
}
