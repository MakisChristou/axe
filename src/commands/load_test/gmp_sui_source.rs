//! Shared sequential Sui source submission for GMP routes.

use std::time::Instant;

use super::metrics::{TxMetrics, TxOutcome};
use super::submitter::TransactionSubmitter;
use super::units::Mist;
use crate::sui::{SuiClient, SuiContractsConfig, SuiWallet};

/// Cross-chain gas attached to a Sui GMP send.
pub(super) const DEFAULT_GAS_VALUE: Mist = Mist::new(100_000_000);
/// On-chain Sui gas budget for executing the PTB itself.
pub(super) const DEFAULT_GAS_BUDGET: Mist = Mist::new(50_000_000);

pub(super) fn parse_gas_value(value: Option<&str>) -> eyre::Result<Mist> {
    let gas_value = match value {
        Some(value) => value
            .parse()
            .map_err(|error| eyre::eyre!("invalid --gas-value: {error}"))?,
        None => DEFAULT_GAS_VALUE.get(),
    };
    crate::ui::kv(
        "cross-chain gas",
        &format!("{gas_value} mist (paid via Sui GasService)"),
    );
    Ok(Mist::new(gas_value))
}

pub(super) fn parse_payload(value: Option<&str>) -> eyre::Result<Option<Vec<u8>>> {
    value
        .map(|value| {
            hex::decode(value.strip_prefix("0x").unwrap_or(value))
                .map_err(|error| eyre::eyre!("invalid --payload hex: {error}"))
        })
        .transpose()
}

#[derive(Clone)]
pub(super) struct GmpSuiSubmitter {
    pub client: SuiClient,
    pub wallet: SuiWallet,
    pub contracts: SuiContractsConfig,
    pub destination_chain: String,
    pub destination_address: String,
    pub gas_value: Mist,
    pub gas_budget: Mist,
}

pub(super) struct GmpSuiSubmitJob {
    pub payload: Vec<u8>,
}

impl TransactionSubmitter for GmpSuiSubmitter {
    type Job = GmpSuiSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        let send_start = Instant::now();
        let result = crate::sui::send_gmp_call(
            &self.client,
            &self.wallet,
            &self.contracts,
            &crate::sui::SuiGmpCall {
                destination_chain: self.destination_chain.clone(),
                destination_address: self.destination_address.clone(),
                payload: job.payload.clone(),
                gas_value_mist: self.gas_value.get(),
                gas_budget_mist: self.gas_budget.get(),
            },
        )
        .await;

        match result {
            Ok(result) if result.success => {
                let latency_ms = send_start
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                TxMetrics {
                    signature: format!("{}-{}", result.digest, result.event_index),
                    submit_time_ms: latency_ms,
                    confirm_time_ms: Some(latency_ms),
                    latency_ms: Some(latency_ms),
                    compute_units: None,
                    slot: None,
                    outcome: TxOutcome::Succeeded,
                    payload: job.payload,
                    payload_hash: result.payload_hash_hex,
                    source_address: format!("0x{}", result.source_address_hex),
                    gmp_destination_chain: self.destination_chain.clone(),
                    gmp_destination_address: self.destination_address.clone(),
                    send_instant: Some(send_start),
                    amplifier_timing: None,
                }
            }
            Ok(result) => failed_metrics(
                job.payload,
                TxOutcome::from_external(false, result.error, "Sui tx failed"),
            ),
            Err(error) => failed_metrics(job.payload, TxOutcome::failed(error.to_string())),
        }
    }
}

fn failed_metrics(payload: Vec<u8>, outcome: super::metrics::TxOutcome) -> TxMetrics {
    TxMetrics {
        signature: String::new(),
        submit_time_ms: 0,
        confirm_time_ms: None,
        latency_ms: None,
        compute_units: None,
        slot: None,
        outcome,
        payload,
        payload_hash: String::new(),
        source_address: String::new(),
        gmp_destination_chain: String::new(),
        gmp_destination_address: String::new(),
        send_instant: None,
        amplifier_timing: None,
    }
}

pub(super) async fn run_sequential(
    submitter: GmpSuiSubmitter,
    jobs: Vec<GmpSuiSubmitJob>,
) -> eyre::Result<super::submitter::BurstResult> {
    super::submitter::run_serial(submitter, jobs, None).await
}
