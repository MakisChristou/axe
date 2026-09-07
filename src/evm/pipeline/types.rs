use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use tokio::sync::Mutex;

use super::super::EvmEndpoints;

pub struct PipelineLimits {
    pub native_reserve: U256,
    pub gas_budget: Option<U256>,
    pub receipt_timeout: Duration,
}

pub struct PipelineTransaction {
    pub request: TransactionRequest,
    pub token: Address,
    pub amount: U256,
    pub deadline: Instant,
}

#[derive(Debug)]
pub enum PipelineError {
    NotSent(String),
    Uncertain(String),
}

pub struct PipelinedSender {
    pub(super) endpoints: EvmEndpoints,
    pub(super) signer: PrivateKeySigner,
    pub(super) limits: PipelineLimits,
    pub(super) state: Mutex<NonceState>,
    pub broadcasts: AtomicU64,
    pub(super) stopped: AtomicBool,
}

#[derive(Default)]
pub(super) struct NonceState {
    pub next: Option<u64>,
    pub gas_reserved: U256,
    pub gas_spent: U256,
    pub tokens_reserved: HashMap<Address, U256>,
    pub stopped: Option<String>,
}

pub(super) struct PendingDeposit {
    pub hash: alloy::primitives::TxHash,
    pub gas_reserved: U256,
    pub token: Address,
    pub amount: U256,
}
