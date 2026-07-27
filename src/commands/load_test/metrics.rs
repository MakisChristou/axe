use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Per-transaction metrics collected during load testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxMetrics {
    pub signature: String,
    pub submit_time_ms: u64,
    pub confirm_time_ms: Option<u64>,
    pub latency_ms: Option<u64>,
    pub compute_units: Option<u64>,
    pub slot: Option<u64>,
    pub success: bool,
    pub error: Option<String>,

    /// keccak256 of the payload, hex-encoded (no 0x prefix).
    #[serde(default)]
    pub payload_hash: String,
    /// The source address of the signer.
    #[serde(default)]
    pub source_address: String,
    /// Raw payload bytes (kept in-memory for verification, not serialized).
    #[serde(skip)]
    pub payload: Vec<u8>,
    /// Instant the tx was submitted (for computing T+X timing).
    #[serde(skip)]
    pub send_instant: Option<Instant>,
    /// GMP-level destination chain from ContractCall event (e.g. "axelar" for ITS hub routing).
    #[serde(default)]
    pub gmp_destination_chain: String,
    /// GMP-level destination address from ContractCall event (e.g. ITS Hub contract for ITS).
    #[serde(default)]
    pub gmp_destination_address: String,
    /// Amplifier pipeline timing (populated during verification phase).
    pub amplifier_timing: Option<AmplifierTiming>,
}

/// Per-step timing through the Amplifier pipeline, relative to tx send time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AmplifierTiming {
    /// Seconds from send to message verified on VotingVerifier (quorum reached).
    pub voted_secs: Option<f64>,
    /// Seconds from send to message routed on destination Cosmos Gateway.
    pub routed_secs: Option<f64>,
    /// Seconds from send to message approved on AxelarnetGateway hub (ITS only).
    pub hub_approved_secs: Option<f64>,
    /// Seconds from send to isMessageApproved on EVM gateway.
    pub approved_secs: Option<f64>,
    /// Seconds from send to execution on destination contract.
    pub executed_secs: Option<f64>,
    /// Whether execution succeeded.
    pub executed_ok: Option<bool>,
    /// The message stored by SenderReceiver (if readable).
    pub stored_message: Option<String>,
}

/// Comprehensive load test report containing all metrics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoadTestReport {
    pub source_chain: String,
    pub destination_chain: String,
    pub destination_address: String,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    pub num_txs: u64,
    pub num_keys: usize,

    pub total_submitted: u64,
    pub total_confirmed: u64,
    pub total_failed: u64,
    pub test_duration_secs: f64,
    pub tps_submitted: f64,
    pub tps_confirmed: f64,
    pub landing_rate: f64,

    pub avg_latency_ms: Option<f64>,
    pub min_latency_ms: Option<u64>,
    pub max_latency_ms: Option<u64>,
    pub avg_compute_units: Option<f64>,
    pub min_compute_units: Option<u64>,
    pub max_compute_units: Option<u64>,

    pub verification: Option<VerificationReport>,
    pub transactions: Vec<TxMetrics>,
}

/// Whether a report should summarize per-transaction compute-unit values.
///
/// Some chains do not expose a comparable resource-unit measurement. Keeping
/// this policy explicit preserves the existing JSON output instead of
/// accidentally populating compute-unit fields for routes that omitted them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ComputeUnitSummary {
    Include,
    Omit,
}

/// Caller-supplied facts for constructing a load-test report.
///
/// Totals, rates, and metric summaries are deliberately absent: they are
/// derived from `transactions` in one place so route implementations cannot
/// disagree about how those values are calculated.
#[derive(Debug)]
pub(super) struct ReportInput {
    pub source_chain: String,
    pub destination_chain: String,
    pub destination_address: String,
    pub num_txs: u64,
    pub num_keys: usize,
    pub total_submitted: u64,
    pub test_duration_secs: f64,
    pub compute_unit_summary: ComputeUnitSummary,
}

impl LoadTestReport {
    pub(super) fn from_transactions(input: ReportInput, transactions: Vec<TxMetrics>) -> Self {
        let total_confirmed = transactions.iter().filter(|metric| metric.success).count() as u64;
        let total_failed = transactions.iter().filter(|metric| !metric.success).count() as u64;
        let (avg_latency_ms, min_latency_ms, max_latency_ms) =
            summarize(transactions.iter().filter_map(|metric| metric.latency_ms));
        let (avg_compute_units, min_compute_units, max_compute_units) =
            match input.compute_unit_summary {
                ComputeUnitSummary::Include => summarize(
                    transactions
                        .iter()
                        .filter_map(|metric| metric.compute_units),
                ),
                ComputeUnitSummary::Omit => (None, None, None),
            };
        let tps_submitted = if input.test_duration_secs > 0.0 {
            input.total_submitted as f64 / input.test_duration_secs
        } else {
            0.0
        };
        let tps_confirmed = if input.test_duration_secs > 0.0 {
            total_confirmed as f64 / input.test_duration_secs
        } else {
            0.0
        };
        let landing_rate = if input.total_submitted > 0 {
            total_confirmed as f64 / input.total_submitted as f64
        } else {
            0.0
        };

        Self {
            source_chain: input.source_chain,
            destination_chain: input.destination_chain,
            destination_address: input.destination_address,
            protocol: String::new(),
            tps: None,
            duration_secs: None,
            num_txs: input.num_txs,
            num_keys: input.num_keys,
            total_submitted: input.total_submitted,
            total_confirmed,
            total_failed,
            test_duration_secs: input.test_duration_secs,
            tps_submitted,
            tps_confirmed,
            landing_rate,
            avg_latency_ms,
            min_latency_ms,
            max_latency_ms,
            avg_compute_units,
            min_compute_units,
            max_compute_units,
            verification: None,
            transactions,
        }
    }
}

fn summarize(values: impl Iterator<Item = u64>) -> (Option<f64>, Option<u64>, Option<u64>) {
    let values: Vec<_> = values.collect();
    if values.is_empty() {
        return (None, None, None);
    }
    (
        Some(values.iter().sum::<u64>() as f64 / values.len() as f64),
        values.iter().min().copied(),
        values.iter().max().copied(),
    )
}

/// Report from transaction verification phase.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VerificationReport {
    pub total_verified: u64,
    pub successful: u64,
    pub pending: u64,
    pub failed: u64,
    pub success_rate: f64,
    pub failure_reasons: Vec<FailureCategory>,
    pub avg_voted_secs: Option<f64>,
    pub avg_routed_secs: Option<f64>,
    pub avg_hub_approved_secs: Option<f64>,
    pub avg_approved_secs: Option<f64>,
    pub avg_executed_secs: Option<f64>,
    pub min_executed_secs: Option<f64>,
    pub max_executed_secs: Option<f64>,
    /// Seconds from earliest send to last successful execution (for throughput).
    pub time_to_last_success_secs: Option<f64>,
    /// Peak throughput (tx/s) observed per pipeline step in a 5s sliding window.
    #[serde(default)]
    pub peak_throughput: PeakThroughput,
    /// Number of txs that timed out before completing all phases.
    pub stuck: u64,
    /// Which phase each stuck tx got stuck at.
    pub stuck_at: Vec<FailureCategory>,
    /// Number of txs that timed out in the polling loop but were reclassified
    /// as successful by the final Axelarscan GMP-API check (the message
    /// actually executed on-chain — a slow final leg, not a real failure).
    #[serde(default)]
    pub recovered_via_api: u64,
}

/// Peak throughput per pipeline step, measured in 5-second sliding windows.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeakThroughput {
    pub voted_tps: Option<f64>,
    pub routed_tps: Option<f64>,
    pub hub_approved_tps: Option<f64>,
    pub approved_tps: Option<f64>,
    pub executed_tps: Option<f64>,
}

/// Categorized failure count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureCategory {
    pub reason: String,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::{ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics};

    fn metric(success: bool, latency_ms: Option<u64>, compute_units: Option<u64>) -> TxMetrics {
        TxMetrics {
            signature: String::new(),
            submit_time_ms: 0,
            confirm_time_ms: None,
            latency_ms,
            compute_units,
            slot: None,
            success,
            error: None,
            payload_hash: String::new(),
            source_address: String::new(),
            payload: Vec::new(),
            send_instant: None,
            gmp_destination_chain: String::new(),
            gmp_destination_address: String::new(),
            amplifier_timing: None,
        }
    }

    fn input(compute_unit_summary: ComputeUnitSummary) -> ReportInput {
        ReportInput {
            source_chain: "source".to_string(),
            destination_chain: "destination".to_string(),
            destination_address: "address".to_string(),
            num_txs: 4,
            num_keys: 2,
            total_submitted: 4,
            test_duration_secs: 2.0,
            compute_unit_summary,
        }
    }

    #[test]
    fn derives_counts_rates_and_metric_summaries() {
        let report = LoadTestReport::from_transactions(
            input(ComputeUnitSummary::Include),
            vec![
                metric(true, Some(10), Some(100)),
                metric(true, Some(30), Some(300)),
                metric(false, None, None),
            ],
        );

        assert_eq!(report.total_submitted, 4);
        assert_eq!(report.total_confirmed, 2);
        assert_eq!(report.total_failed, 1);
        assert_eq!(report.tps_submitted, 2.0);
        assert_eq!(report.tps_confirmed, 1.0);
        assert_eq!(report.landing_rate, 0.5);
        assert_eq!(report.avg_latency_ms, Some(20.0));
        assert_eq!(report.min_latency_ms, Some(10));
        assert_eq!(report.max_latency_ms, Some(30));
        assert_eq!(report.avg_compute_units, Some(200.0));
        assert_eq!(report.min_compute_units, Some(100));
        assert_eq!(report.max_compute_units, Some(300));
    }

    #[test]
    fn omits_compute_unit_summary_when_route_does_not_report_it() {
        let report = LoadTestReport::from_transactions(
            input(ComputeUnitSummary::Omit),
            vec![metric(true, Some(10), Some(100))],
        );

        assert_eq!(report.avg_compute_units, None);
        assert_eq!(report.min_compute_units, None);
        assert_eq!(report.max_compute_units, None);
    }

    #[test]
    fn handles_empty_metrics_and_zero_duration() {
        let mut input = input(ComputeUnitSummary::Include);
        input.total_submitted = 0;
        input.test_duration_secs = 0.0;

        let report = LoadTestReport::from_transactions(input, Vec::new());

        assert_eq!(report.total_confirmed, 0);
        assert_eq!(report.total_failed, 0);
        assert_eq!(report.tps_submitted, 0.0);
        assert_eq!(report.tps_confirmed, 0.0);
        assert_eq!(report.landing_rate, 0.0);
        assert_eq!(report.avg_latency_ms, None);
        assert_eq!(report.avg_compute_units, None);
    }
}
