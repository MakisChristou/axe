mod benchmark;
mod client;
mod execution;
mod inventory;
mod presentation;
mod read;
mod route;
mod stats;
mod traffic;
mod types;

use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, WrapErr, eyre};
use indicatif::ProgressBar;

use self::client::RfqClient;
use self::execution::{ExecutionFeedback, execute_leg, execute_planned_leg, execute_round_trip};
use self::route::{
    DiscoveryFeedback, PlanningFeedback, RouteDiscovery, discover_wallet, plan_roundtrip,
    plan_send, plan_sweep, render_plans,
};
use self::stats::percentile;
use self::types::{LegExecution, LegResult, RoutePlan, RunLimits};
use crate::config::ChainsConfig;
use crate::shutdown::{DrainTarget, Shutdown};
use crate::types::Network;
use crate::ui;

pub use self::types::{AssetSpec, AssetType, HumanAmount, OrderType};
pub use benchmark::{
    QuoteBenchmarkArgs, QuoteBenchmarkLimit, QuoteBenchmarkMode, QuoteBenchmarkTarget,
    benchmark_quotes,
};
pub use inventory::{InventoryArgs, inventory};
pub use read::{ApiArgs, CatalogArgs, StatusArgs, catalog, status};
pub use traffic::{TrafficArgs, run as traffic};

pub fn resolve_quote_sender(
    sender: Option<Address>,
    private_key: Option<String>,
) -> Result<Address> {
    if let Some(sender) = sender {
        return Ok(sender);
    }
    let Some(private_key) = private_key else {
        return Ok(Address::ZERO);
    };
    let signer: PrivateKeySigner = private_key
        .parse()
        .wrap_err("EVM_PRIVATE_KEY is not a valid hex private key")?;
    Ok(signer.address())
}

pub fn resolve_private_key(
    override_key: Option<String>,
    evm_private_key: Option<String>,
    default_private_key: Option<String>,
) -> Result<String> {
    [override_key, evm_private_key, default_private_key]
        .into_iter()
        .flatten()
        .find(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            eyre!(
                "intent execution needs an EVM signing key; set EVM_PRIVATE_KEY or PRIVATE_KEY, or pass --private-key"
            )
        })
}

pub struct IntentRuntimeArgs {
    pub network: Network,
    pub rfq_url: Option<String>,
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
        asset_type: AssetType,
    },
    Explicit {
        from: AssetSpec,
        to: AssetSpec,
        amount: Option<HumanAmount>,
        wallet_bps: u16,
        order_type: OrderType,
        asset_type: AssetType,
    },
}

impl RouteChoice {
    pub fn new(
        from: Option<AssetSpec>,
        to: Option<AssetSpec>,
        amount: Option<HumanAmount>,
        wallet_bps: u16,
        order_type: OrderType,
        asset_type: AssetType,
    ) -> Result<Self> {
        match (from, to, amount) {
            (None, None, None) => Ok(Self::Random {
                wallet_bps,
                order_type,
                asset_type,
            }),
            (Some(from), Some(to), amount) => Ok(Self::Explicit {
                from,
                to,
                amount,
                wallet_bps,
                order_type,
                asset_type,
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

pub struct QuoteArgs {
    pub runtime: IntentRuntimeArgs,
    pub route: RouteChoice,
    pub sender: Option<Address>,
    pub recipient: Option<Address>,
    pub json: bool,
}

pub struct RoundtripArgs {
    pub runtime: IntentRuntimeArgs,
    pub route: RouteChoice,
}

pub struct SweepArgs {
    pub runtime: IntentRuntimeArgs,
    pub sweeps: u64,
    pub continuous: bool,
    pub dry_run: bool,
    pub wallet_bps: u16,
    pub order_type: OrderType,
    pub asset_type: AssetType,
}

struct IntentRuntime {
    signer: PrivateKeySigner,
    config: ChainsConfig,
    client: RfqClient,
    limits: RunLimits,
    auto_confirm: bool,
}

pub async fn quote(args: QuoteArgs) -> Result<()> {
    let runtime = prepare_runtime(args.runtime).await?;
    let wallet = runtime.signer.address();
    let sender = args.sender.unwrap_or(wallet);
    let recipient = args.recipient.unwrap_or(sender);
    let discovery_feedback = if args.json {
        DiscoveryFeedback::Quiet
    } else {
        DiscoveryFeedback::Detailed
    };
    let discovery =
        discover_wallet(&runtime.client, &runtime.config, sender, discovery_feedback).await?;
    let plan = plan_send(
        &runtime.client,
        &discovery,
        sender,
        recipient,
        &args.route,
        PlanningFeedback::Hidden,
    )
    .await?;
    read::render_planned_quote(&plan, args.json)?;
    if args.json {
        return Ok(());
    }
    if sender != wallet {
        ui::warn(
            "The quote sender differs from the axe wallet, so this quote cannot be deposited.",
        );
        return Ok(());
    }
    if !ui::confirm("Deposit this quote and watch it to fulfillment?").await {
        ui::info("Quote not deposited.");
        return Ok(());
    }

    let quote_id = plan.quote.quote.quote_id.clone();
    let _shutdown = Shutdown::install(DrainTarget::Intent);
    let result = execute_planned_leg(
        &runtime.client,
        &discovery.chains,
        &runtime.signer,
        plan,
        runtime.limits,
        &ExecutionFeedback::Debugger,
    )
    .await?;
    render_summary(std::slice::from_ref(&result), 1);
    ui::success(&format!("Intent {quote_id} fulfilled successfully."));
    Ok(())
}

pub async fn send(args: SendArgs) -> Result<()> {
    let runtime = prepare_runtime(args.runtime).await?;
    let discovery = discover_wallet(
        &runtime.client,
        &runtime.config,
        runtime.signer.address(),
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
        PlanningFeedback::Visible,
    )
    .await?;
    ui::kv("mode", "one intent");
    ui::kv("intent deposits", "1");
    confirm_execution(runtime.auto_confirm, "Execute this intent?").await?;
    let _shutdown = Shutdown::install(DrainTarget::Intent);

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
            max_input_amount: None,
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
        runtime.signer.address(),
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
    let _shutdown = Shutdown::install(DrainTarget::RoundTrip);

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
    let shutdown = Shutdown::install(DrainTarget::RoundTrip);
    let mut results = Vec::new();
    let mut planned_intents = 0usize;
    let mut sweep = 0u64;

    loop {
        sweep += 1;
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
            args.asset_type,
            args.wallet_bps,
            args.order_type,
            PlanningFeedback::Visible,
        )
        .await;
        if plans.is_empty() {
            render_summary(&results, planned_intents);
            return Err(eyre!(
                "No {}-to-{} round-trip routes are funded and quoted. Fund matching assets or choose a different --asset-type.",
                args.asset_type.label(),
                args.asset_type.label()
            ));
        }
        if args.dry_run {
            render_plans(&plans);
            return Ok(());
        }
        let pass_intents = plans.len() * 2;
        planned_intents += pass_intents;
        ui::info(&format!(
            "sweep {sweep}: {} {} round trips, {pass_intents} intents",
            plans.len(),
            args.asset_type.label()
        ));

        let executed =
            execute_sweep_pass(&runtime, &discovery, &plans, &mut results, &shutdown).await;
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
        if shutdown.requested() {
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
    shutdown: &Shutdown,
) -> Result<bool> {
    let progress = sweep_progress(plans.len() * 2);
    let feedback = ExecutionFeedback::Progress(progress.clone());
    for plan in plans {
        if shutdown.requested() {
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
    presentation::intent_progress_bar(total as u64, "starting intent sweep")
}

async fn prepare_runtime(args: IntentRuntimeArgs) -> Result<IntentRuntime> {
    let signer: PrivateKeySigner = args
        .private_key
        .parse()
        .wrap_err("intent EVM private key is not valid hex")?;
    let config = ChainsConfig::load(&args.config).await?;
    let client = RfqClient::new(args.network, args.rfq_url.as_deref())?;
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

    #[test]
    fn resolves_quote_sender_from_override_key_or_zero() {
        let signer = PrivateKeySigner::random();
        let address = signer.address();

        assert_eq!(
            resolve_quote_sender(Some(address), Some("ignored".to_owned())).unwrap(),
            address
        );
        assert_eq!(
            resolve_quote_sender(None, Some(signer.to_bytes().to_string())).unwrap(),
            address
        );
        assert_eq!(resolve_quote_sender(None, None).unwrap(), Address::ZERO);
    }

    #[test]
    fn resolves_intent_key_by_precedence() {
        assert_eq!(
            resolve_private_key(
                Some("override".to_owned()),
                Some("evm".to_owned()),
                Some("default".to_owned()),
            )
            .unwrap(),
            "override"
        );
        assert_eq!(
            resolve_private_key(None, Some("evm".to_owned()), Some("default".to_owned())).unwrap(),
            "evm"
        );
        assert_eq!(
            resolve_private_key(None, None, Some("default".to_owned())).unwrap(),
            "default"
        );
        assert!(resolve_private_key(None, None, None).is_err());
    }
}
