use std::time::Duration;

use alloy::primitives::U256;

use super::super::read::{ApiArgs, PreparedQuote, QuoteRequestArgs};
use super::super::types::{AssetId, OrderType, QuoteRequest};

pub enum QuoteBenchmarkLimit {
    Requests(u64),
    Duration(Duration),
}

pub struct QuoteBenchmarkArgs {
    pub api: ApiArgs,
    pub request: QuoteRequestArgs,
    pub limit: QuoteBenchmarkLimit,
    pub concurrency: usize,
    pub warmup: u64,
    pub request_timeout: Duration,
    pub max_rps: Option<u64>,
    pub json: bool,
}

#[derive(Clone, Copy)]
pub(super) enum RunLimit {
    Requests(u64),
    Duration(Duration),
}

pub(super) struct BenchmarkTarget {
    pub request: QuoteRequest,
    pub from: AssetId,
    pub to: AssetId,
    pub requested_amount: U256,
    pub order_type: OrderType,
    pub output_symbol: String,
    pub output_decimals: u8,
}

impl From<PreparedQuote> for BenchmarkTarget {
    fn from(prepared: PreparedQuote) -> Self {
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
        }
    }
}

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

pub(super) struct Sample {
    pub latency_ms: u64,
    pub outcome: SampleOutcome,
}

pub(super) struct BenchmarkReport {
    pub samples: Vec<Sample>,
    pub elapsed: Duration,
    pub output_symbol: String,
    pub output_decimals: u8,
}
