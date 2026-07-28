//! Shared source-side submission for Solana ITS load-test routes.
//!
//! Route modules prepare wallets, token accounts, and destination-specific
//! verification. This adapter owns the blocking Solana RPC call, ITS message
//! ID extraction, and normalized transaction metrics.

use std::sync::Arc;
use std::time::Instant;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

use super::metrics::TxMetrics;
use super::submitter::TransactionSubmitter;
use crate::solana;

#[derive(Clone)]
pub(super) enum MetricContext {
    /// Preserve the richer hub-routing metadata used by Solana -> EVM.
    HubRouted { hub_address: String },
    /// Preserve the lean metrics used by the Sui destination route.
    DestinationManaged,
}

#[derive(Clone)]
pub(super) struct ItsSolanaSubmitter {
    pub rpc_url: String,
    pub network: crate::types::Network,
    pub token_id: [u8; 32],
    pub mint: Pubkey,
    pub destination_chain: String,
    pub destination_address: Vec<u8>,
    pub amount: u64,
    pub gas_value: u64,
    pub metric_context: MetricContext,
}

#[derive(Clone)]
pub(super) struct ItsSolanaSubmitJob {
    pub keypair: Arc<Keypair>,
    pub source_account: Pubkey,
}

impl TransactionSubmitter for ItsSolanaSubmitter {
    type Job = ItsSolanaSubmitJob;

    async fn submit(&self, job: Self::Job) -> TxMetrics {
        let submitter = self.clone();
        let source_address = job.keypair.pubkey().to_string();
        let submit_start = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let result = solana::send_its_interchain_transfer(solana::InterchainTransferRequest {
                rpc_url: &submitter.rpc_url,
                keypair: &job.keypair,
                network: submitter.network,
                token_id: &submitter.token_id,
                source_account: &job.source_account,
                mint: &submitter.mint,
                destination_chain: &submitter.destination_chain,
                destination_address: &submitter.destination_address,
                amount: submitter.amount,
                gas_value: submitter.gas_value,
            });
            result.map(|(signature, mut metrics)| {
                metrics.signature = solana::extract_its_message_id(
                    &submitter.rpc_url,
                    submitter.network,
                    &signature,
                )
                .unwrap_or_else(|_| submitter.fallback_message_id(&signature));
                metrics
            })
        })
        .await;

        match result {
            Ok(Ok(mut metrics)) => {
                if let MetricContext::HubRouted { hub_address } = &self.metric_context {
                    metrics.source_address = source_address;
                    metrics.send_instant = Some(submit_start);
                    metrics.gmp_destination_chain = crate::types::HubChain::NAME.to_string();
                    metrics.gmp_destination_address = hub_address.clone();
                }
                metrics
            }
            Ok(Err(error)) => self.failed_metric(source_address, error.to_string()),
            Err(error) => {
                self.failed_metric(source_address, format!("spawn_blocking join: {error}"))
            }
        }
    }
}

impl ItsSolanaSubmitter {
    fn fallback_message_id(&self, signature: &str) -> String {
        match self.metric_context {
            MetricContext::HubRouted { .. } => format!("{signature}-1.4"),
            MetricContext::DestinationManaged => format!(
                "{}-{}.1",
                signature,
                solana::solana_call_contract_index(self.network)
            ),
        }
    }

    fn failed_metric(&self, source_address: String, error: String) -> TxMetrics {
        let source_address = match self.metric_context {
            MetricContext::HubRouted { .. } => String::new(),
            MetricContext::DestinationManaged => source_address,
        };
        TxMetrics {
            signature: String::new(),
            submit_time_ms: 0,
            confirm_time_ms: None,
            latency_ms: None,
            compute_units: None,
            slot: None,
            outcome: TxMetrics::failed_outcome(error),
            payload: Vec::new(),
            payload_hash: String::new(),
            source_address,
            gmp_destination_chain: String::new(),
            gmp_destination_address: String::new(),
            send_instant: None,
            amplifier_timing: None,
        }
    }
}

pub(super) fn source_account(keypair: &Keypair, mint: &Pubkey) -> Pubkey {
    let token_program = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
    let ata_program = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    Pubkey::find_program_address(
        &[
            keypair.pubkey().as_ref(),
            token_program.as_ref(),
            mint.as_ref(),
        ],
        &ata_program,
    )
    .0
}

pub(super) async fn run_its_burst(
    submitter: ItsSolanaSubmitter,
    jobs: Vec<ItsSolanaSubmitJob>,
    max_concurrent: usize,
) -> eyre::Result<super::submitter::BurstResult> {
    super::submitter::run_burst(submitter, jobs, max_concurrent).await
}

pub(super) fn its_sustained_tasks(
    submitter: ItsSolanaSubmitter,
    jobs: Vec<ItsSolanaSubmitJob>,
    verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
) -> super::sustained::MakeTask {
    let submitter = Arc::new(submitter);
    let jobs = Arc::new(jobs);
    Box::new(move |key_idx: usize, _nonce: Option<u64>| {
        let submitter = Arc::clone(&submitter);
        let job = jobs[key_idx].clone();
        let verify_tx = verify_tx.clone();
        Box::pin(async move {
            let mut metrics = submitter.submit(job).await;
            if metrics.is_success()
                && let Some(verify_tx) = verify_tx
            {
                match super::verify::tx_to_pending_its(&metrics, false) {
                    Ok(pending) => {
                        let _ = verify_tx.send(pending);
                    }
                    Err(error) => {
                        metrics.mark_failed(format!("failed to build verification state: {error}"));
                    }
                }
            }
            metrics
        })
    })
}
