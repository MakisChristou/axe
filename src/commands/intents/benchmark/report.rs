use std::time::Duration;

use alloy::primitives::U256;
use serde_json::{Value, json};

use super::types::{BenchmarkReport, FailureKind, Sample, SampleOutcome};
use crate::commands::intents::stats::percentile;
use crate::commands::intents::types::format_units;
use crate::ui;

struct OutcomeCounts {
    available: usize,
    unavailable: usize,
    failed: usize,
    timed_out: usize,
}

struct FailureCounts {
    request: usize,
    invalid_quote: usize,
    invalid_output: usize,
}

pub(super) fn render_report(
    report: &BenchmarkReport,
    concurrency: usize,
    warmup: u64,
    max_rps: Option<u64>,
) {
    let counts = outcome_counts(&report.samples);
    ui::section("intent quote benchmark");
    ui::kv("requests", &report.samples.len().to_string());
    ui::kv("elapsed", &ui::format_duration(report.elapsed));
    ui::kv(
        "throughput",
        &format!("{:.1} requests/s", throughput(report)),
    );
    ui::kv("concurrency", &concurrency.to_string());
    ui::kv("warmup requests", &warmup.to_string());
    if let Some(max_rps) = max_rps {
        ui::kv("rate limit", &format!("{max_rps} requests/s"));
    }
    ui::kv(
        "available quotes",
        &format!(
            "{}/{} ({:.1}%)",
            counts.available,
            report.samples.len(),
            percentage(counts.available, report.samples.len())
        ),
    );
    ui::kv(
        "other outcomes",
        &format!(
            "{} unavailable │ {} failed │ {} timed out",
            counts.unavailable, counts.failed, counts.timed_out
        ),
    );
    if counts.failed > 0 {
        render_failures(&report.samples);
    }
    ui::kv("request latency", &latency_summary(&report.samples));
    if let Some(output) = output_summary(
        &report.samples,
        report.output_decimals,
        &report.output_symbol,
    ) {
        ui::kv("output amount", &output);
    }
    if let Some(validity) = validity_summary(&report.samples) {
        ui::kv("quote validity", &validity);
    }
}

fn render_failures(samples: &[Sample]) {
    let failures = failure_counts(samples);
    ui::kv(
        "failures",
        &format!(
            "{} request │ {} invalid quote │ {} invalid output",
            failures.request, failures.invalid_quote, failures.invalid_output
        ),
    );
}

pub(super) fn report_json(report: &BenchmarkReport) -> Value {
    let counts = outcome_counts(&report.samples);
    let failures = failure_counts(&report.samples);
    let latency = latency_values(&report.samples);
    let outputs = output_values(&report.samples);
    let validity = validity_values(&report.samples);
    json!({
        "requests": {
            "attempted": report.samples.len(),
            "available": counts.available,
            "unavailable": counts.unavailable,
            "failed": counts.failed,
            "timedOut": counts.timed_out,
            "failures": {
                "request": failures.request,
                "invalidQuote": failures.invalid_quote,
                "invalidOutput": failures.invalid_output,
            },
        },
        "elapsedMs": duration_ms(report.elapsed),
        "throughputRps": throughput(report),
        "latencyMs": numeric_summary(&latency),
        "outputAmountBaseUnits": amount_summary(&outputs),
        "quoteValidityMs": numeric_summary(&validity),
    })
}

fn outcome_counts(samples: &[Sample]) -> OutcomeCounts {
    let mut counts = OutcomeCounts {
        available: 0,
        unavailable: 0,
        failed: 0,
        timed_out: 0,
    };
    for sample in samples {
        match sample.outcome {
            SampleOutcome::Available { .. } => counts.available += 1,
            SampleOutcome::Unavailable => counts.unavailable += 1,
            SampleOutcome::Failed(_) => counts.failed += 1,
            SampleOutcome::TimedOut => counts.timed_out += 1,
        }
    }
    counts
}

fn failure_counts(samples: &[Sample]) -> FailureCounts {
    let mut counts = FailureCounts {
        request: 0,
        invalid_quote: 0,
        invalid_output: 0,
    };
    for sample in samples {
        match sample.outcome {
            SampleOutcome::Failed(FailureKind::Request) => counts.request += 1,
            SampleOutcome::Failed(FailureKind::InvalidQuote) => counts.invalid_quote += 1,
            SampleOutcome::Failed(FailureKind::InvalidOutput) => counts.invalid_output += 1,
            _ => {}
        }
    }
    counts
}

fn latency_summary(samples: &[Sample]) -> String {
    let values = latency_values(samples);
    format!(
        "p50 {} │ p90 {} │ p95 {} │ p99 {} │ max {}",
        ui::format_millis(percentile(&values, 50)),
        ui::format_millis(percentile(&values, 90)),
        ui::format_millis(percentile(&values, 95)),
        ui::format_millis(percentile(&values, 99)),
        ui::format_millis(values.iter().copied().max().unwrap_or_default())
    )
}

fn output_summary(samples: &[Sample], decimals: u8, symbol: &str) -> Option<String> {
    let mut values = output_values(samples);
    values.sort_unstable();
    Some(format!(
        "min {} │ median {} │ max {} {}",
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
            "median {} │ min {}",
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
    report.samples.len() as f64 / seconds
}

fn percentage(count: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    count as f64 / total as f64 * 100.0
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

        assert_eq!(
            latency_summary(&samples),
            "p50 30 ms │ p90 50 ms │ p95 50 ms │ p99 50 ms │ max 50 ms"
        );
        assert_eq!(
            output_summary(&samples, 0, "TOKEN").as_deref(),
            Some("min 80 │ median 100 │ max 120 TOKEN")
        );
        assert_eq!(
            validity_summary(&samples).as_deref(),
            Some("median 800 ms │ min 600 ms")
        );
    }

    #[test]
    fn outcome_counts_cover_every_request() {
        let samples = [
            available(10, 100, 1_000),
            Sample {
                latency_ms: 20,
                outcome: SampleOutcome::Unavailable,
            },
            Sample {
                latency_ms: 30,
                outcome: SampleOutcome::Failed(FailureKind::Request),
            },
            Sample {
                latency_ms: 40,
                outcome: SampleOutcome::TimedOut,
            },
        ];
        let counts = outcome_counts(&samples);

        assert_eq!(counts.available, 1);
        assert_eq!(counts.unavailable, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.timed_out, 1);
    }
}
