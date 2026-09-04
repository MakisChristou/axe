#[cfg(test)]
mod tests;
mod types;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use alloy::consensus::Transaction;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Result, eyre};
use tokio::sync::Mutex;

use self::types::{NonceState, PendingDeposit};
use super::{ERC20, EvmEndpoints, is_pending_state_error, tx_known_by_hash, wait_receipt_any};
use crate::retry::retry_with_fallback_all;
use crate::ui;

pub use self::types::{PipelineError, PipelineLimits, PipelineTransaction, PipelinedSender};

impl PipelinedSender {
    pub fn new(endpoints: EvmEndpoints, signer: PrivateKeySigner, limits: PipelineLimits) -> Self {
        Self {
            endpoints,
            signer,
            limits,
            state: Mutex::new(NonceState::default()),
            broadcasts: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    pub async fn gas_spent(&self) -> U256 {
        self.state.lock().await.gas_spent
    }

    pub async fn send(&self, tx: PipelineTransaction) -> Result<TransactionReceipt, PipelineError> {
        let pending = self.broadcast(tx).await?;
        let receipt = wait_receipt_any(
            &self.endpoints,
            &[pending.hash],
            self.limits.receipt_timeout,
        )
        .await;
        let mut state = self.state.lock().await;
        match receipt {
            Some(receipt) => {
                state.release(
                    &pending,
                    U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price),
                );
                Ok(receipt)
            }
            None => {
                let reason = format!(
                    "deposit {} is unconfirmed after {}s",
                    pending.hash,
                    self.limits.receipt_timeout.as_secs()
                );
                state.stopped = Some(reason.clone());
                self.stopped.store(true, Ordering::Relaxed);
                Err(PipelineError::Uncertain(reason))
            }
        }
    }

    async fn broadcast(
        &self,
        mut tx: PipelineTransaction,
    ) -> Result<PendingDeposit, PipelineError> {
        let mut state = self.state.lock().await;
        if let Some(reason) = &state.stopped {
            return Err(PipelineError::NotSent(reason.clone()));
        }
        let prepared = match self.prepare(&mut state, &mut tx).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.stopped
                    .store(state.stopped.is_some(), Ordering::Relaxed);
                return Err(PipelineError::NotSent(error.to_string()));
            }
        };
        let pending = PendingDeposit {
            hash: *prepared.tx_hash(),
            gas_reserved: U256::from(prepared.gas_limit()) * U256::from(prepared.max_fee_per_gas()),
            token: tx.token,
            amount: tx.amount,
        };
        state.reserve(&pending);
        if let Err(error) = self.endpoints.broadcast_raw(&prepared).await
            && !tx_known_by_hash(&self.endpoints, pending.hash).await
        {
            let reason = ui::scrub_urls(&format!(
                "broadcast uncertain for {}: {error}",
                pending.hash
            ));
            state.stopped = Some(reason.clone());
            self.stopped.store(true, Ordering::Relaxed);
            return Err(PipelineError::Uncertain(reason));
        }
        state.next = prepared.nonce().checked_add(1);
        if state.next.is_none() {
            state.stopped = Some("nonce space exhausted".to_owned());
            self.stopped.store(true, Ordering::Relaxed);
        }
        self.broadcasts.fetch_add(1, Ordering::Relaxed);
        Ok(pending)
    }

    async fn prepare(
        &self,
        state: &mut NonceState,
        tx: &mut PipelineTransaction,
    ) -> Result<alloy::consensus::TxEnvelope> {
        if state.next.is_none() {
            state.next = Some(self.starting_nonce().await?);
        }
        tx.request.nonce = state.next;
        let envelope = self
            .endpoints
            .fill_and_sign(&self.signer, tx.request.clone(), &ui::warn)
            .await?;
        let gas = U256::from(envelope.gas_limit()) * U256::from(envelope.max_fee_per_gas());
        if state.gas_spent.saturating_add(gas) > self.limits.gas_budget {
            state.stopped = Some("source gas budget reached".to_owned());
            return Err(eyre!("source gas budget reached"));
        }
        let (native, token) = self.balances(tx.token).await?;
        state.check_funding(&self.limits, native, token, tx.token, tx.amount, gas)?;
        if Instant::now() >= tx.deadline {
            return Err(eyre!("quote expired before broadcast"));
        }
        Ok(envelope)
    }

    async fn starting_nonce(&self) -> Result<u64> {
        let owner = self.signer.address();
        retry_with_fallback_all(
            "initial pipeline nonce",
            self.endpoints.providers(),
            |provider| async move {
                match provider.get_transaction_count(owner).pending().await {
                    Err(error) if is_pending_state_error(&error) => {
                        provider.get_transaction_count(owner).await
                    }
                    other => other,
                }
            },
        )
        .await
        .map_err(Into::into)
    }

    async fn balances(&self, token: Address) -> Result<(U256, U256)> {
        let owner = self.signer.address();
        retry_with_fallback_all(
            "pipeline balances",
            self.endpoints.providers(),
            |provider| async move {
                let erc20 = ERC20::new(token, &provider);
                let balance = erc20.balanceOf(owner);
                tokio::try_join!(
                    async {
                        provider
                            .get_balance(owner)
                            .await
                            .map_err(eyre::Report::from)
                    },
                    async { balance.call().await.map_err(eyre::Report::from) }
                )
            },
        )
        .await
    }
}

impl NonceState {
    fn check_funding(
        &self,
        limits: &PipelineLimits,
        native: U256,
        tokens: U256,
        token: Address,
        amount: U256,
        gas: U256,
    ) -> Result<()> {
        let reserved = self.gas_reserved.saturating_add(gas);
        if self.gas_spent.saturating_add(reserved) > limits.gas_budget {
            return Err(eyre!("waiting for pending gas reservations"));
        }
        if native < limits.native_reserve.saturating_add(reserved) {
            return Err(eyre!("native balance is reserved for pending deposits"));
        }
        if tokens
            < self
                .tokens_reserved
                .get(&token)
                .copied()
                .unwrap_or_default()
                .saturating_add(amount)
        {
            return Err(eyre!("token balance is reserved for pending deposits"));
        }
        Ok(())
    }

    fn reserve(&mut self, pending: &PendingDeposit) {
        self.gas_reserved = self.gas_reserved.saturating_add(pending.gas_reserved);
        let reserved = self.tokens_reserved.entry(pending.token).or_default();
        *reserved = reserved.saturating_add(pending.amount);
    }

    fn release(&mut self, pending: &PendingDeposit, gas_paid: U256) {
        self.gas_reserved = self.gas_reserved.saturating_sub(pending.gas_reserved);
        self.gas_spent = self.gas_spent.saturating_add(gas_paid);
        let reserved = self.tokens_reserved.entry(pending.token).or_default();
        *reserved = reserved.saturating_sub(pending.amount);
    }
}
