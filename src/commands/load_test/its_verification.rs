//! Chain-neutral orchestration around the existing ITS destination verifiers.
//!
//! Destination adapters select the chain-specific verifier. The session owns
//! the streaming verifier lifecycle so route modules only coordinate setup
//! and transaction submission.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use alloy::primitives::Address;
use eyre::{Result, WrapErr, eyre};
use indicatif::ProgressBar;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::metrics::{
    AmplifierTiming, ComputeUnitSummary, LoadTestReport, ReportInput, TxMetrics, VerificationReport,
};
use super::submitter::BurstResult;
use super::sustained::SustainedResult;
use super::verify::{
    self, EvmItsDestination, ItsBatchVerification, PendingTx, SolanaItsDestination,
    StellarItsDestination, StreamingVerification, VerificationRoute, XrplItsDestination,
};
use super::{LoadTestArgs, finish_report};
use crate::types::Network;

/// Owned route data shared by every ITS destination adapter.
#[derive(Clone)]
pub(super) struct ItsVerificationRoute {
    config: PathBuf,
    source_chain: String,
    destination_chain: String,
    network: Network,
}

impl ItsVerificationRoute {
    pub(super) fn from_args(args: &LoadTestArgs) -> Self {
        Self {
            config: args.config.clone(),
            source_chain: args.source_axelar_id.clone(),
            destination_chain: args.destination_axelar_id.clone(),
            network: args.network,
        }
    }

    fn borrowed(&self) -> VerificationRoute<'_> {
        VerificationRoute {
            config: &self.config,
            source_chain: &self.source_chain,
            destination_chain: &self.destination_chain,
            network: self.network,
        }
    }
}

/// A destination capable of verifying ITS transfers in batch or while sends
/// are still in flight.
pub(super) trait ItsVerificationTarget: Send + 'static {
    fn verify_batch<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a;

    fn verify_streaming<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>> + Send + 'a;
}

pub(super) struct EvmItsTarget {
    pub gateway_addr: Address,
    pub rpc_url: String,
}

impl ItsVerificationTarget for EvmItsTarget {
    fn verify_batch<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a {
        verify::verify_onchain_evm_its(ItsBatchVerification {
            route: route.borrowed(),
            destination: EvmItsDestination {
                gateway_addr: self.gateway_addr,
                rpc_url: &self.rpc_url,
            },
            metrics,
        })
    }

    fn verify_streaming<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>> + Send + 'a
    {
        verify::verify_onchain_evm_its_streaming(StreamingVerification {
            route: route.borrowed(),
            destination: EvmItsDestination {
                gateway_addr: self.gateway_addr,
                rpc_url: &self.rpc_url,
            },
            rx,
            send_done,
            spinner,
        })
    }
}

pub(super) struct SolanaItsTarget {
    pub rpc_url: String,
}

impl ItsVerificationTarget for SolanaItsTarget {
    fn verify_batch<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a {
        verify::verify_onchain_solana_its(ItsBatchVerification {
            route: route.borrowed(),
            destination: SolanaItsDestination {
                rpc_url: &self.rpc_url,
            },
            metrics,
        })
    }

    fn verify_streaming<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>> + Send + 'a
    {
        verify::verify_onchain_solana_its_streaming(StreamingVerification {
            route: route.borrowed(),
            destination: SolanaItsDestination {
                rpc_url: &self.rpc_url,
            },
            rx,
            send_done,
            spinner,
        })
    }
}

pub(super) struct StellarItsTarget {
    pub rpc_url: String,
    pub network_type: String,
    pub gateway_contract: String,
    pub signer_pk: [u8; 32],
}

impl ItsVerificationTarget for StellarItsTarget {
    fn verify_batch<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a {
        verify::verify_onchain_stellar_its(ItsBatchVerification {
            route: route.borrowed(),
            destination: StellarItsDestination {
                rpc_url: &self.rpc_url,
                network_type: &self.network_type,
                gateway_contract: &self.gateway_contract,
                signer_pk: self.signer_pk,
            },
            metrics,
        })
    }

    fn verify_streaming<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>> + Send + 'a
    {
        verify::verify_onchain_stellar_its_streaming(StreamingVerification {
            route: route.borrowed(),
            destination: StellarItsDestination {
                rpc_url: &self.rpc_url,
                network_type: &self.network_type,
                gateway_contract: &self.gateway_contract,
                signer_pk: self.signer_pk,
            },
            rx,
            send_done,
            spinner,
        })
    }
}

pub(super) struct XrplItsTarget {
    pub rpc_url: String,
    pub recipient: String,
}

impl ItsVerificationTarget for XrplItsTarget {
    fn verify_batch<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a {
        verify::verify_onchain_xrpl_its(ItsBatchVerification {
            route: route.borrowed(),
            destination: XrplItsDestination {
                rpc_url: &self.rpc_url,
                recipient: &self.recipient,
            },
            metrics,
        })
    }

    fn verify_streaming<'a>(
        &'a self,
        route: &'a ItsVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>> + Send + 'a
    {
        verify::verify_onchain_xrpl_its_streaming(StreamingVerification {
            route: route.borrowed(),
            destination: XrplItsDestination {
                rpc_url: &self.rpc_url,
                recipient: &self.recipient,
            },
            rx,
            send_done,
            spinner,
        })
    }
}

type StreamingResult = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>;

/// Owns the channels and task that connect a sustained sender to an ITS
/// destination verifier.
pub(super) struct ItsVerificationSession {
    verify_tx: mpsc::UnboundedSender<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner_tx: Option<oneshot::Sender<ProgressBar>>,
    verify_handle: Option<JoinHandle<StreamingResult>>,
}

impl ItsVerificationSession {
    pub(super) fn start<T>(route: ItsVerificationRoute, target: T) -> Self
    where
        T: ItsVerificationTarget,
    {
        let (verify_tx, verify_rx) = mpsc::unbounded_channel();
        let send_done = Arc::new(AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = oneshot::channel();
        let verifier_done = Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx
                .await
                .wrap_err("ITS verification spinner was not attached")?;
            target
                .verify_streaming(&route, verify_rx, verifier_done, spinner)
                .await
        });

        Self {
            verify_tx,
            send_done,
            spinner_tx: Some(spinner_tx),
            verify_handle: Some(verify_handle),
        }
    }

    pub(super) fn sender(&self) -> mpsc::UnboundedSender<PendingTx> {
        self.verify_tx.clone()
    }

    pub(super) fn send_done(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.send_done)
    }

    pub(super) fn attach_spinner(&mut self, spinner: ProgressBar) -> Result<()> {
        self.spinner_tx
            .take()
            .ok_or_else(|| eyre!("ITS verification spinner already attached"))?
            .send(spinner)
            .map_err(|_| eyre!("ITS verification task stopped before the send phase"))
    }

    pub(super) async fn finish_sustained(
        mut self,
        args: &LoadTestArgs,
        result: SustainedResult,
        destination_address: &str,
        total_expected: u64,
        num_keys: usize,
        test_start: Instant,
    ) -> Result<()> {
        let mut report = super::sustained::build_sustained_report(
            result,
            &args.source_chain,
            &args.destination_chain,
            destination_address,
            total_expected,
            num_keys,
        );
        let handle = self
            .verify_handle
            .take()
            .ok_or_else(|| eyre!("ITS verification session already finished"))?;
        let (verification, timings) = handle.await.wrap_err("ITS verification task failed")??;
        merge_timings(&mut report, timings);
        report.verification = Some(verification);
        finish_report(args, &mut report, test_start)
    }
}

impl Drop for ItsVerificationSession {
    fn drop(&mut self) {
        if let Some(handle) = &self.verify_handle {
            handle.abort();
        }
    }
}

pub(super) async fn finish_batch<T>(
    args: &LoadTestArgs,
    target: &T,
    report: &mut LoadTestReport,
    test_start: Instant,
) -> Result<()>
where
    T: ItsVerificationTarget,
{
    let route = ItsVerificationRoute::from_args(args);
    let verification = target
        .verify_batch(&route, &mut report.transactions)
        .await?;
    report.verification = Some(verification);
    finish_report(args, report, test_start)
}

pub(super) struct ItsBurstReport {
    pub destination_address: String,
    pub num_txs: u64,
    pub num_keys: usize,
    pub compute_unit_summary: ComputeUnitSummary,
}

pub(super) async fn finish_burst<T>(
    args: &LoadTestArgs,
    target: &T,
    burst: BurstResult,
    spec: ItsBurstReport,
    test_start: Instant,
) -> Result<()>
where
    T: ItsVerificationTarget,
{
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            source_chain: args.source_chain.clone(),
            destination_chain: args.destination_chain.clone(),
            destination_address: spec.destination_address,
            num_txs: spec.num_txs,
            num_keys: spec.num_keys,
            total_submitted: burst.total_submitted,
            test_duration_secs: burst.test_duration_secs,
            compute_unit_summary: spec.compute_unit_summary,
        },
        burst.metrics,
    );
    finish_batch(args, target, &mut report, test_start).await
}

fn merge_timings(report: &mut LoadTestReport, timings: Vec<(String, AmplifierTiming)>) {
    for (message_id, timing) in timings {
        if let Some(transaction) = report
            .transactions
            .iter_mut()
            .find(|transaction| transaction.signature == message_id)
        {
            transaction.amplifier_timing = Some(timing);
        }
    }
}
