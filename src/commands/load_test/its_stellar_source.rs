//! Shared source-side sending for Stellar ITS routes.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use super::metrics::TxMetrics;
use super::sustained;
use crate::stellar::{StellarClient, StellarWallet};

pub(super) struct TransferRequest<'a> {
    pub client: &'a StellarClient,
    pub wallet: &'a StellarWallet,
    pub its_contract: &'a str,
    pub gateway_contract: &'a str,
    pub token_id: [u8; 32],
    pub destination_chain: &'a str,
    pub destination_address_bytes: &'a [u8],
    pub gas_token: &'a str,
    pub gas_amount_stroops: u64,
    pub transfer_amount: u128,
    pub gmp_dest_address: &'a str,
}

pub(super) async fn submit_transfer(request: TransferRequest<'_>) -> TxMetrics {
    let submit_start = Instant::now();
    // ITS emits the `ContractCall` event from the AxelarGateway contract,
    // so VotingVerifier records the ITS contract as the source address.
    let source_addr = request.its_contract.to_string();
    match request
        .client
        .its_interchain_transfer(
            request.wallet,
            request.its_contract,
            request.gateway_contract,
            request.token_id,
            request.destination_chain,
            request.destination_address_bytes,
            request.transfer_amount,
            None,
            request.gas_token,
            request.gas_amount_stroops,
        )
        .await
    {
        Ok(invoked) => {
            let submit_time_ms = submit_start.elapsed().as_millis() as u64;
            let event_index = invoked.event_index.unwrap_or(0);
            let message_id = format!("0x{}-{event_index}", invoked.tx_hash_hex.to_lowercase());
            TxMetrics {
                signature: message_id,
                submit_time_ms,
                confirm_time_ms: Some(submit_time_ms),
                latency_ms: Some(submit_time_ms),
                compute_units: None,
                slot: None,
                success: invoked.success,
                error: if invoked.success {
                    None
                } else {
                    Some("interchain_transfer reverted".to_string())
                },
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: source_addr,
                gmp_destination_chain: "axelar".to_string(),
                gmp_destination_address: request.gmp_dest_address.to_string(),
                send_instant: Some(submit_start),
                amplifier_timing: None,
            }
        }
        Err(error) => {
            let elapsed_ms = submit_start.elapsed().as_millis() as u64;
            TxMetrics {
                signature: String::new(),
                submit_time_ms: elapsed_ms,
                confirm_time_ms: None,
                latency_ms: None,
                compute_units: None,
                slot: None,
                success: false,
                error: Some(error.to_string()),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: source_addr,
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            }
        }
    }
}

/// Owned transaction context captured by the sustained task adapter.
pub(super) struct SustainedTransferContext {
    pub client: StellarClient,
    pub wallets: Vec<StellarWallet>,
    pub its_contract: String,
    pub gateway_contract: String,
    pub token_id: [u8; 32],
    pub destination_chain: String,
    pub destination_address_bytes: Vec<u8>,
    pub gas_token: String,
    pub gas_stroops: u64,
    pub amount_per_tx: u128,
    pub axelarnet_gw_addr: String,
}

/// Pacing and reporting inputs for one sustained Stellar-source run.
pub(super) struct SustainedTransferArgs {
    pub context: SustainedTransferContext,
    pub tps: usize,
    pub duration_secs: u64,
    pub key_cycle: usize,
    pub verify_tx: Option<tokio::sync::mpsc::UnboundedSender<super::verify::PendingTx>>,
    pub send_done: Option<Arc<AtomicBool>>,
    pub spinner: indicatif::ProgressBar,
}

pub(super) async fn run_sustained(args: SustainedTransferArgs) -> sustained::SustainedResult {
    let context = Arc::new(args.context);
    let verify_tx = args.verify_tx;

    let make_task: sustained::MakeTask = Box::new(move |key_idx: usize, _nonce: Option<u64>| {
        let context = Arc::clone(&context);
        let verify_tx = verify_tx.clone();

        Box::pin(async move {
            let wallet = &context.wallets[key_idx % context.wallets.len()];
            let mut metrics = submit_transfer(TransferRequest {
                client: &context.client,
                wallet,
                its_contract: &context.its_contract,
                gateway_contract: &context.gateway_contract,
                token_id: context.token_id,
                destination_chain: &context.destination_chain,
                destination_address_bytes: &context.destination_address_bytes,
                gas_token: &context.gas_token,
                gas_amount_stroops: context.gas_stroops,
                transfer_amount: context.amount_per_tx,
                gmp_dest_address: &context.axelarnet_gw_addr,
            })
            .await;
            if metrics.success
                && let Some(ref tx_sender) = verify_tx
            {
                // Stellar ITS verification starts at the Voted stage.
                match super::verify::tx_to_pending_its(&metrics, true) {
                    Ok(pending) => {
                        let _ = tx_sender.send(pending);
                    }
                    Err(error) => {
                        metrics.success = false;
                        metrics.error =
                            Some(format!("failed to build verification state: {error}"));
                    }
                }
            }
            metrics
        })
    });

    sustained::run_sustained_loop(
        args.tps,
        args.duration_secs,
        args.key_cycle,
        None,
        make_task,
        args.send_done,
        args.spinner,
    )
    .await
}
