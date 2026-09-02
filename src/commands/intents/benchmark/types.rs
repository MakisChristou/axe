use std::time::Duration;

use alloy::primitives::U256;
use eyre::{Result, eyre};

use super::super::read::{ApiArgs, PreparedQuote};
use super::super::types::{AssetId, AssetSpec, AssetType, HumanAmount, OrderType, QuoteRequest};

const DEFAULT_BURST_REQUESTS: u64 = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum QuoteBenchmarkMode {
    #[default]
    Burst,
    Continuous,
}

impl QuoteBenchmarkMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Burst => "burst",
            Self::Continuous => "continuous",
        }
    }
}

#[derive(Clone, Copy)]
pub enum QuoteBenchmarkLimit {
    Requests(u64),
    Duration(Duration),
    Continuous,
}

impl QuoteBenchmarkLimit {
    pub fn resolve(
        mode: Option<QuoteBenchmarkMode>,
        requests: Option<u64>,
        duration: Option<Duration>,
    ) -> Result<Self> {
        let mode = mode.unwrap_or_else(|| {
            if duration.is_some() {
                QuoteBenchmarkMode::Continuous
            } else {
                QuoteBenchmarkMode::Burst
            }
        });
        match mode {
            QuoteBenchmarkMode::Burst => {
                if duration.is_some() {
                    return Err(eyre!(
                        "--duration-secs requires --mode continuous (or omit --mode)"
                    ));
                }
                Ok(Self::Requests(requests.unwrap_or(DEFAULT_BURST_REQUESTS)))
            }
            QuoteBenchmarkMode::Continuous => {
                if requests.is_some() {
                    return Err(eyre!("--requests cannot be used with --mode continuous"));
                }
                Ok(duration.map_or(Self::Continuous, Self::Duration))
            }
        }
    }

    pub const fn mode(self) -> QuoteBenchmarkMode {
        match self {
            Self::Requests(_) => QuoteBenchmarkMode::Burst,
            Self::Duration(_) | Self::Continuous => QuoteBenchmarkMode::Continuous,
        }
    }
}

pub struct QuoteBenchmarkArgs {
    pub api: ApiArgs,
    pub target: QuoteBenchmarkTarget,
    pub limit: QuoteBenchmarkLimit,
    pub concurrency: usize,
    pub warmup: u64,
    pub request_timeout: Duration,
    pub max_rps: Option<u64>,
    pub json: bool,
}

pub struct QuoteBenchmarkTarget {
    pub from: Option<AssetSpec>,
    pub to: Option<AssetSpec>,
    pub amount: Option<HumanAmount>,
    pub sender: alloy::primitives::Address,
    pub recipient: alloy::primitives::Address,
    pub order_type: OrderType,
    pub asset_type: AssetType,
}

pub(super) struct BenchmarkTarget {
    pub request: QuoteRequest,
    pub from: AssetId,
    pub to: AssetId,
    pub requested_amount: U256,
    pub order_type: OrderType,
    pub output_symbol: String,
    pub output_decimals: u8,
    pub from_label: String,
    pub to_label: String,
    pub requested_symbol: String,
    pub requested_decimals: u8,
}

impl From<PreparedQuote> for BenchmarkTarget {
    fn from(prepared: PreparedQuote) -> Self {
        let from_label = format!("{}/{}", prepared.from.chain_id, prepared.from.symbol);
        let to_label = format!("{}/{}", prepared.to.chain_id, prepared.to.symbol);
        let (requested_symbol, requested_decimals) = match prepared.order_type {
            OrderType::ExactInput => (prepared.from.symbol.clone(), prepared.from.decimals),
            OrderType::ExactOutput => (prepared.to.symbol.clone(), prepared.to.decimals),
        };
        Self {
            request: prepared.request,
            from: AssetId {
                chain_id: prepared.from.chain_id,
                token_address: prepared.from.address,
            },
            to: AssetId {
                chain_id: prepared.to.chain_id,
                token_address: prepared.to.address,
            },
            requested_amount: prepared.requested_amount,
            order_type: prepared.order_type,
            output_symbol: prepared.to.symbol,
            output_decimals: prepared.to.decimals,
            from_label,
            to_label,
            requested_symbol,
            requested_decimals,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum SampleOutcome {
    Available {
        output_amount: U256,
        validity_ms: u64,
    },
    Unavailable,
    Failed(FailureKind),
    TimedOut,
}

#[derive(Clone, Copy)]
pub(super) enum FailureKind {
    Request,
    InvalidQuote,
    InvalidOutput,
}

#[derive(Clone, Copy)]
pub(super) struct Sample {
    pub latency_ms: u64,
    pub outcome: SampleOutcome,
}

pub(super) struct BenchmarkReport {
    pub mode: QuoteBenchmarkMode,
    pub interrupted: bool,
    pub counts: SampleCounts,
    pub samples: Vec<Sample>,
    pub elapsed: Duration,
    pub output_symbol: String,
    pub output_decimals: u8,
    pub from_label: String,
    pub to_label: String,
    pub requested_amount: U256,
    pub requested_symbol: String,
    pub requested_decimals: u8,
}

#[derive(Clone, Copy, Default)]
pub(super) struct SampleCounts {
    pub attempted: u64,
    pub available: u64,
    pub unavailable: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub request_failures: u64,
    pub invalid_quotes: u64,
    pub invalid_outputs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_mode_resolves_legacy_and_explicit_inputs() {
        assert!(matches!(
            QuoteBenchmarkLimit::resolve(None, None, None),
            Ok(QuoteBenchmarkLimit::Requests(100))
        ));
        assert!(matches!(
            QuoteBenchmarkLimit::resolve(None, None, Some(Duration::from_secs(3))),
            Ok(QuoteBenchmarkLimit::Duration(duration)) if duration == Duration::from_secs(3)
        ));
        assert!(matches!(
            QuoteBenchmarkLimit::resolve(Some(QuoteBenchmarkMode::Continuous), None, None),
            Ok(QuoteBenchmarkLimit::Continuous)
        ));
    }

    #[test]
    fn benchmark_mode_rejects_mismatched_limit_flags() {
        assert!(
            QuoteBenchmarkLimit::resolve(
                Some(QuoteBenchmarkMode::Burst),
                None,
                Some(Duration::from_secs(1))
            )
            .is_err()
        );
        assert!(
            QuoteBenchmarkLimit::resolve(Some(QuoteBenchmarkMode::Continuous), Some(1), None)
                .is_err()
        );
    }
}
