use std::time::Duration;

use alloy::primitives::U256;
use serde_json::{Value, json};

use super::types::{BenchmarkReport, BenchmarkSelection, Sample, SampleOutcome};
use crate::commands::intents::stats::percentile;
use crate::commands::intents::types::format_units;
use crate::ui;

pub(super) fn render_report(
    report: &BenchmarkReport,
    concurrency: usize,
    warmup: u64,
    max_rps: Option<u64>,
) {
    let counts = report.counts;
    let retained_samples = u64::try_from(report.samples.len()).unwrap_or(u64::MAX);
    ui::section("intent quote benchmark result");
    render_target(report);
    ui::kv("mode", report.mode.label());
    if report.interrupted {
        ui::kv(
            "status",
            "stopped gracefully after draining in-flight requests",
        );
    }
    ui::kv(
        "requests",
        &format!(
            "{} total | {} available | {} unavailable | {} failed | {} timed out",
            counts.attempted, counts.available, counts.unavailable, counts.failed, counts.timed_out
        ),
    );
    if retained_samples < counts.attempted {
        ui::kv(
            "statistics sample",
            &format!("{} retained requests", report.samples.len()),
        );
    }
    ui::kv("elapsed", &ui::format_duration(report.elapsed));
    ui::kv("rate", &format!("{:.1} requests/s", throughput(report)));
    let rate_limit = max_rps.map_or_else(
        || "uncapped rate".to_owned(),
        |rate| format!("{rate} requests/s cap"),
    );
    ui::kv(
        "settings",
        &format!("{concurrency} concurrent | {warmup} warmup | {rate_limit}"),
    );
    if counts.failed > 0 {
        render_failures(report);
    }
    ui::kv("request latency", &latency_summary(&report.samples));
    if matches!(&report.selection, BenchmarkSelection::Fixed)
        && let Some(output) = output_summary(
            &report.samples,
            report.output_decimals,
            &report.output_symbol,
        )
    {
        ui::kv("output amount", &output);
    }
    if let Some(validity) = validity_summary(&report.samples) {
        ui::kv("quote validity", &validity);
    }
}

fn render_target(report: &BenchmarkReport) {
    match &report.selection {
        BenchmarkSelection::Fixed => {
            ui::kv(
                "route",
                &format!("{} -> {}", report.from_label, report.to_label),
            );
            ui::kv(
                "amount",
                &format!(
                    "{} {}",
                    format_units(report.requested_amount, report.requested_decimals),
                    report.requested_symbol
                ),
            );
        }
        BenchmarkSelection::Randomized {
            bidirectional_routes,
            amount,
            asset_type,
        } => {
            ui::kv(
                "routes",
                &format!(
                    "{bidirectional_routes} bidirectional · {} quote directions",
                    bidirectional_routes * 2
                ),
            );
            ui::kv("selection", "shuffled round trips (quote-only)");
            ui::kv(
                "amount",
                &format!("{amount} {} per request", asset_type.label()),
            );
        }
    }
}

fn render_failures(report: &BenchmarkReport) {
    ui::kv(
        "failures",
        &format!(
            "{} request | {} invalid quote | {} invalid output",
            report.counts.request_failures,
            report.counts.invalid_quotes,
            report.counts.invalid_outputs
        ),
    );
}

pub(super) fn report_json(report: &BenchmarkReport) -> Value {
    let counts = report.counts;
    let latency = latency_values(&report.samples);
    let outputs = output_values(&report.samples);
    let validity = validity_values(&report.samples);
    let output_amount = if matches!(&report.selection, BenchmarkSelection::Fixed) {
        amount_summary(&outputs)
    } else {
        Value::Null
    };
    let target = match &report.selection {
        BenchmarkSelection::Fixed => json!({
            "selection": "fixed",
            "from": report.from_label,
            "to": report.to_label,
            "requestedAmount": report.requested_amount.to_string(),
            "requestedSymbol": report.requested_symbol,
        }),
        BenchmarkSelection::Randomized {
            bidirectional_routes,
            amount,
            asset_type,
        } => json!({
            "selection": "randomized-bidirectional",
            "bidirectionalRoutes": bidirectional_routes,
            "directedRoutes": bidirectional_routes * 2,
            "humanAmount": amount,
            "assetType": asset_type.label(),
        }),
    };
    json!({
        "mode": report.mode.label(),
        "interrupted": report.interrupted,
        "target": target,
        "requests": {
            "attempted": counts.attempted,
            "statisticsSampled": report.samples.len(),
            "available": counts.available,
            "unavailable": counts.unavailable,
            "failed": counts.failed,
            "timedOut": counts.timed_out,
            "failures": {
                "request": counts.request_failures,
                "invalidQuote": counts.invalid_quotes,
                "invalidOutput": counts.invalid_outputs,
            },
        },
        "elapsedMs": duration_ms(report.elapsed),
        "throughputRps": throughput(report),
        "latencyMs": numeric_summary(&latency),
        "outputAmountBaseUnits": output_amount,
        "quoteValidityMs": numeric_summary(&validity),
    })
}

fn latency_summary(samples: &[Sample]) -> String {
    let values = latency_values(samples);
    format!(
        "p50 {} | p95 {}",
        ui::format_millis(percentile(&values, 50)),
        ui::format_millis(percentile(&values, 95)),
    )
}

fn output_summary(samples: &[Sample], decimals: u8, symbol: &str) -> Option<String> {
    let mut values = output_values(samples);
    values.sort_unstable();
    Some(format!(
        "min {} | median {} | max {} {}",
        format_units(*values.first()?, decimals),
        format_units(values[(values.len() - 1) / 2], decimals),
        format_units(*values.last()?, decimals),
        symbol
    ))
}

fn validity_summary(samples: &[Sample]) -> Option<String> {
    let values = validity_values(samples);
    (!values.is_empty()).then(|| {
        format!(
            "median {} | min {}",
            ui::format_millis(percentile(&values, 50)),
            ui::format_millis(values.iter().copied().min().unwrap_or_default())
        )
    })
}

fn latency_values(samples: &[Sample]) -> Vec<u64> {
    samples.iter().map(|sample| sample.latency_ms).collect()
}

fn output_values(samples: &[Sample]) -> Vec<U256> {
    samples
        .iter()
        .filter_map(|sample| match sample.outcome {
            SampleOutcome::Available { output_amount, .. } => Some(output_amount),
            _ => None,
        })
        .collect()
}

fn validity_values(samples: &[Sample]) -> Vec<u64> {
    samples
        .iter()
        .filter_map(|sample| match sample.outcome {
            SampleOutcome::Available { validity_ms, .. } => Some(validity_ms),
            _ => None,
        })
        .collect()
}

fn numeric_summary(values: &[u64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    json!({
        "p50": percentile(values, 50),
        "p90": percentile(values, 90),
        "p95": percentile(values, 95),
        "p99": percentile(values, 99),
        "max": values.iter().copied().max().unwrap_or_default(),
    })
}

fn amount_summary(values: &[U256]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    json!({
        "min": values.first().map(ToString::to_string),
        "median": values.get((values.len() - 1) / 2).map(ToString::to_string),
        "max": values.last().map(ToString::to_string),
    })
}

fn throughput(report: &BenchmarkReport) -> f64 {
    let seconds = report.elapsed.as_secs_f64();
    if seconds == 0.0 {
        return 0.0;
    }
    report.counts.attempted as f64 / seconds
}

pub(super) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available(latency_ms: u64, output_amount: u64, validity_ms: u64) -> Sample {
        Sample {
            latency_ms,
            outcome: SampleOutcome::Available {
                output_amount: U256::from(output_amount),
                validity_ms,
            },
        }
    }

    #[test]
    fn benchmark_summaries_use_nearest_rank_percentiles() {
        let samples = [
            available(10, 100, 1_000),
            available(20, 110, 900),
            available(30, 90, 800),
            available(40, 120, 700),
            available(50, 80, 600),
        ];

        assert_eq!(latency_summary(&samples), "p50 30 ms | p95 50 ms");
        assert_eq!(
            output_summary(&samples, 0, "TOKEN").as_deref(),
            Some("min 80 | median 100 | max 120 TOKEN")
        );
        assert_eq!(
            validity_summary(&samples).as_deref(),
            Some("median 800 ms | min 600 ms")
        );
    }
}
