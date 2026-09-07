use alloy::primitives::U256;
use eyre::Result;
use serde_json::json;

use super::super::stats::percentile;
use super::super::types::format_units;
use super::types::{StressLimits, StressRun};
use crate::ui;

pub(super) fn render(run: &StressRun, limits: &StressLimits, json_output: bool) -> Result<()> {
    let elapsed = run.state.started.elapsed();
    let rate = |count| {
        if elapsed.is_zero() {
            0.0
        } else {
            count as f64 / elapsed.as_secs_f64()
        }
    };
    let volume = format_units(
        U256::from(run.state.confirmed).saturating_mul(limits.amount),
        limits.decimals,
    );
    let latency = run
        .records
        .iter()
        .map(|record| record.deposit_latency_ms)
        .collect::<Vec<_>>();
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "stop_reason": run.stop_reason,
                "elapsed_seconds": elapsed.as_secs_f64(),
                "broadcast": run.state.broadcast, "confirmed": run.state.confirmed,
                "skipped": run.state.skipped, "failed": run.state.failed,
                "attempts": run.state.attempts, "committed_attempts": run.state.committed,
                "peak_active": run.state.peak_active, "recovery_warnings": run.warnings,
                "broadcast_per_second": rate(run.state.broadcast),
                "confirmed_per_second": rate(run.state.confirmed),
                "deposited_input": volume, "symbol": limits.symbol,
                "max_volume": limits.max_volume.map(|cap| format_units(cap, limits.decimals)),
                "max_native_spend_per_chain": limits.max_native_spend.map(|cap| format_units(cap, 18)),
                "sources": run.sources, "deposits": run.records,
            }))?
        );
        return Ok(());
    }
    ui::section("deposit stress result");
    ui::kv(
        "stop",
        &format!(
            "{} after {}",
            run.stop_reason.label(),
            ui::format_duration(elapsed)
        ),
    );
    ui::kv(
        "deposits",
        &format!(
            "{} broadcast | {} confirmed | {} skipped | {} failed",
            run.state.broadcast, run.state.confirmed, run.state.skipped, run.state.failed
        ),
    );
    ui::kv(
        "rate",
        &format!(
            "{:.2} broadcast/s | {:.2} confirmed/s",
            rate(run.state.broadcast),
            rate(run.state.confirmed)
        ),
    );
    ui::kv("input", &format!("{volume} {} deposited", limits.symbol));
    ui::kv(
        "deposit latency",
        &format!(
            "p50 {} | p95 {} (includes sender queue)",
            ui::format_millis(percentile(&latency, 50)),
            ui::format_millis(percentile(&latency, 95))
        ),
    );
    for source in &run.sources {
        ui::kv(
            &source.chain,
            &format!(
                "{} confirmed | {} skipped | {} failed | {} native gas",
                source.confirmed, source.skipped, source.failed, source.gas_spent
            ),
        );
        if let Some(issue) = &source.last_issue {
            ui::info(&format!("  last issue: {issue}"));
        }
    }
    if run.state.failed > 0 {
        ui::info("Unconfirmed broadcasts remain counted against the spending limits.");
    }
    Ok(())
}
