//! Shared source-side submission for Sui ITS routes.
//!
//! Sui keeps its existing sequential, single-wallet behavior. This adapter
//! only moves the chain-specific PTB mechanics behind the common
//! [`TransactionSubmitter`] capability; it does not add sustained mode.

use std::time::Instant;

use super::identifiers::TokenId;
use super::metrics::{TxMetrics, TxOutcome};
use super::submitter::TransactionSubmitter;
use super::units::Mist;
use crate::sui::{SuiClient, SuiItsContractsConfig, SuiWallet};

#[derive(Clone)]
pub(super) struct ItsSuiSubmitter {
    pub client: SuiClient,
    pub wallet: SuiWallet,
    pub contracts: SuiItsContractsConfig,
    pub coin_type: String,
    pub token_id: TokenId,
    pub destination_chain: String,
    pub destination_address_bytes: Vec<u8>,
    pub transfer_amount: u64,
    pub gas_value: Mist,
    pub gas_budget: Mist,
    pub its_hub_address: String,
}

impl TransactionSubmitter for ItsSuiSubmitter {
    type Job = ();

    async fn submit(&self, (): Self::Job) -> TxMetrics {
        let send_start = Instant::now();
        let result =
            crate::sui::send_its_interchain_transfer(crate::sui::InterchainTransferRequest {
                client: &self.client,
                wallet: &self.wallet,
                contracts: &self.contracts,
                coin_type_tag: &self.coin_type,
                token_id: self.token_id.into_bytes(),
                destination_chain: &self.destination_chain,
                destination_address_bytes: &self.destination_address_bytes,
                transfer_amount: self.transfer_amount,
                gas_value_mist: self.gas_value.get(),
                gas_budget_mist: self.gas_budget.get(),
            })
            .await;

        match result {
            Ok(result) if result.success => {
                let latency_ms = send_start
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                TxMetrics {
                    confirm_time_ms: Some(latency_ms),
                    latency_ms: Some(latency_ms),
                    payload_hash: result.payload_hash_hex,
                    source_address: format!("0x{}", result.source_address_hex),
                    gmp_destination_chain: "axelar".to_string(),
                    gmp_destination_address: self.its_hub_address.clone(),
                    send_instant: Some(send_start),
                    ..TxMetrics::succeeded(
                        format!("{}-{}", result.digest, result.event_index),
                        latency_ms,
                    )
                }
            }
            Ok(result) => failed_metrics(TxOutcome::from_external(
                false,
                result.error,
                "Sui ITS tx failed",
            )),
            Err(error) => failed_metrics(TxOutcome::failed(error.to_string())),
        }
    }
}

fn failed_metrics(outcome: super::metrics::TxOutcome) -> TxMetrics {
    TxMetrics::from_outcome("", 0, outcome)
}

pub(super) async fn run_its_sequential(
    submitter: ItsSuiSubmitter,
    num_txs: usize,
) -> eyre::Result<super::submitter::BurstResult> {
    super::submitter::run_serial(submitter, vec![(); num_txs], None).await
}
