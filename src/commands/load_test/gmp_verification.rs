//! Chain-neutral lifecycle around the existing GMP destination verifiers.
//!
//! Route modules retain protocol choices and source submission. Destination
//! adapters select the existing chain-specific verifier, while the session
//! owns streaming channels, task lifetime, timing merge, and completion.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use alloy::primitives::Address;
use alloy::providers::Provider;
use eyre::{Result, WrapErr, eyre};
use indicatif::ProgressBar;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::metrics::{AmplifierTiming, LoadTestReport, TxMetrics, VerificationReport};
use super::verify::{
    self, EvmGmpDestination, EvmGmpStreamingDestination, GmpBatchVerification, PendingTx,
    SolanaGmpDestination, SourceChainType, StellarGmpDestination, StreamingVerification,
    SuiGmpDestination, VerificationRoute,
};
use super::{LoadTestArgs, finish_report};
use crate::types::Network;

#[derive(Clone)]
pub(super) struct GmpVerificationRoute {
    config: PathBuf,
    source_chain: String,
    destination_chain: String,
    network: Network,
}

impl GmpVerificationRoute {
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

pub(super) trait GmpBatchTarget {
    fn verify_batch<'a>(
        &'a self,
        route: &'a GmpVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a;
}

pub(super) struct EvmGmpTarget<'a, P> {
    pub address: &'a str,
    pub gateway_addr: Address,
    pub provider: &'a P,
    pub source_type: SourceChainType,
    pub legacy: bool,
}

impl<P> GmpBatchTarget for EvmGmpTarget<'_, P>
where
    P: Provider + Sync,
{
    async fn verify_batch(
        &self,
        route: &GmpVerificationRoute,
        metrics: &mut [TxMetrics],
    ) -> Result<VerificationReport> {
        let request = GmpBatchVerification {
            route: route.borrowed(),
            destination: EvmGmpDestination {
                address: self.address,
                gateway_addr: self.gateway_addr,
                provider: self.provider,
            },
            metrics,
            source_type: self.source_type,
        };
        if self.legacy {
            verify::verify_onchain_evm_legacy(request).await
        } else {
            verify::verify_onchain(request).await
        }
    }
}

pub(super) struct SolanaGmpTarget<'a> {
    pub address: &'a str,
    pub rpc_url: &'a str,
    pub source_type: SourceChainType,
}

impl GmpBatchTarget for SolanaGmpTarget<'_> {
    fn verify_batch<'a>(
        &'a self,
        route: &'a GmpVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a {
        verify::verify_onchain_solana(GmpBatchVerification {
            route: route.borrowed(),
            destination: SolanaGmpDestination {
                address: self.address,
                rpc_url: self.rpc_url,
            },
            metrics,
            source_type: self.source_type,
        })
    }
}

pub(super) struct StellarGmpTarget<'a> {
    pub contract: &'a str,
    pub rpc_url: &'a str,
    pub network_type: &'a str,
    pub gateway_contract: &'a str,
    pub signer_pk: [u8; 32],
    pub source_type: SourceChainType,
}

impl GmpBatchTarget for StellarGmpTarget<'_> {
    fn verify_batch<'a>(
        &'a self,
        route: &'a GmpVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a {
        verify::verify_onchain_stellar_gmp(GmpBatchVerification {
            route: route.borrowed(),
            destination: StellarGmpDestination {
                contract: self.contract,
                rpc_url: self.rpc_url,
                network_type: self.network_type,
                gateway_contract: self.gateway_contract,
                signer_pk: self.signer_pk,
            },
            metrics,
            source_type: self.source_type,
        })
    }
}

pub(super) struct SuiGmpTarget<'a> {
    pub address: &'a str,
    pub rpc_url: &'a str,
    pub source_type: SourceChainType,
}

impl GmpBatchTarget for SuiGmpTarget<'_> {
    fn verify_batch<'a>(
        &'a self,
        route: &'a GmpVerificationRoute,
        metrics: &'a mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> + Send + 'a {
        verify::verify_onchain_sui_gmp(GmpBatchVerification {
            route: route.borrowed(),
            destination: SuiGmpDestination {
                address: self.address,
                rpc_url: self.rpc_url,
            },
            metrics,
            source_type: self.source_type,
        })
    }
}

pub(super) async fn finish_batch<T>(
    args: &LoadTestArgs,
    target: &T,
    report: &mut LoadTestReport,
    test_start: Instant,
) -> Result<()>
where
    T: GmpBatchTarget,
{
    attach_batch(args, target, report).await?;
    finish_report(args, report, test_start)
}

pub(super) async fn attach_batch<T>(
    args: &LoadTestArgs,
    target: &T,
    report: &mut LoadTestReport,
) -> Result<()>
where
    T: GmpBatchTarget,
{
    let route = GmpVerificationRoute::from_args(args);
    let verification = target
        .verify_batch(&route, &mut report.transactions)
        .await?;
    report.verification = Some(verification);
    Ok(())
}

pub(super) trait GmpStreamingTarget: Send + 'static {
    fn message_matcher(&self) -> MessageMatcher;

    fn verify_streaming<'a>(
        &'a self,
        route: &'a GmpVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>> + Send + 'a;
}

pub(super) struct EvmGmpStreamingTarget {
    pub address: String,
    pub gateway_addr: Address,
    pub rpc_url: String,
    pub legacy: bool,
    pub message_matcher: MessageMatcher,
}

impl GmpStreamingTarget for EvmGmpStreamingTarget {
    fn message_matcher(&self) -> MessageMatcher {
        self.message_matcher
    }

    async fn verify_streaming(
        &self,
        route: &GmpVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> Result<(VerificationReport, Vec<(String, AmplifierTiming)>)> {
        let request = StreamingVerification {
            route: route.borrowed(),
            destination: EvmGmpStreamingDestination {
                address: &self.address,
                gateway_addr: self.gateway_addr,
                rpc_url: &self.rpc_url,
            },
            rx,
            send_done,
            spinner,
        };
        if self.legacy {
            verify::verify_onchain_evm_legacy_streaming(request).await
        } else {
            verify::verify_onchain_evm_streaming(request).await
        }
    }
}

pub(super) struct SolanaGmpStreamingTarget {
    pub address: String,
    pub rpc_url: String,
}

impl GmpStreamingTarget for SolanaGmpStreamingTarget {
    fn message_matcher(&self) -> MessageMatcher {
        MessageMatcher::Solana
    }

    fn verify_streaming<'a>(
        &'a self,
        route: &'a GmpVerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>> + Send + 'a
    {
        verify::verify_onchain_solana_streaming(StreamingVerification {
            route: route.borrowed(),
            destination: SolanaGmpDestination {
                address: &self.address,
                rpc_url: &self.rpc_url,
            },
            rx,
            send_done,
            spinner,
        })
    }
}

type StreamingResult = Result<(VerificationReport, Vec<(String, AmplifierTiming)>)>;

#[derive(Clone, Copy)]
pub(super) enum MessageMatcher {
    Exact,
    Solana,
}

pub(super) struct GmpVerificationSession {
    verify_tx: mpsc::UnboundedSender<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner_tx: Option<oneshot::Sender<ProgressBar>>,
    verify_handle: Option<JoinHandle<StreamingResult>>,
    matcher: MessageMatcher,
}

impl GmpVerificationSession {
    pub(super) fn start<T>(route: GmpVerificationRoute, target: T) -> Self
    where
        T: GmpStreamingTarget,
    {
        let matcher = target.message_matcher();
        let (verify_tx, verify_rx) = mpsc::unbounded_channel();
        let send_done = Arc::new(AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = oneshot::channel();
        let verifier_done = Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx
                .await
                .wrap_err("GMP verification spinner was not attached")?;
            target
                .verify_streaming(&route, verify_rx, verifier_done, spinner)
                .await
        });

        Self {
            verify_tx,
            send_done,
            spinner_tx: Some(spinner_tx),
            verify_handle: Some(verify_handle),
            matcher,
        }
    }

    pub(super) fn sender(&self) -> mpsc::UnboundedSender<PendingTx> {
        self.verify_tx.clone()
    }

    pub(super) fn send_done(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.send_done)
    }

    pub(super) fn take_spinner_sender(&mut self) -> Result<oneshot::Sender<ProgressBar>> {
        self.spinner_tx
            .take()
            .ok_or_else(|| eyre!("GMP verification spinner sender already taken"))
    }

    pub(super) async fn finish(
        mut self,
        report: &mut LoadTestReport,
        network: Network,
    ) -> Result<()> {
        let handle = self
            .verify_handle
            .take()
            .ok_or_else(|| eyre!("GMP verification session already finished"))?;
        let (verification, timings) = handle.await.wrap_err("GMP verification task failed")??;
        merge_timings(report, timings, self.matcher, network);
        report.verification = Some(verification);
        Ok(())
    }
}

impl Drop for GmpVerificationSession {
    fn drop(&mut self) {
        if let Some(handle) = &self.verify_handle {
            handle.abort();
        }
    }
}

fn merge_timings(
    report: &mut LoadTestReport,
    timings: Vec<(String, AmplifierTiming)>,
    matcher: MessageMatcher,
    network: Network,
) {
    for (message_id, timing) in timings {
        if let Some(transaction) = report.transactions.iter_mut().find(|transaction| {
            transaction.signature == message_id
                || matches!(matcher, MessageMatcher::Solana)
                    && format!(
                        "{}-{}.1",
                        transaction.signature,
                        crate::solana::solana_call_contract_index(network)
                    ) == message_id
        }) {
            transaction.amplifier_timing = Some(timing);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MessageMatcher, merge_timings};
    use crate::commands::load_test::metrics::{AmplifierTiming, LoadTestReport, TxMetrics};
    use crate::types::Network;

    #[test]
    fn timing_merge_supports_exact_and_solana_message_ids() {
        for (matcher, message_id) in [
            (MessageMatcher::Exact, "signature".to_string()),
            (
                MessageMatcher::Solana,
                format!(
                    "signature-{}.1",
                    crate::solana::solana_call_contract_index(Network::Testnet)
                ),
            ),
        ] {
            let mut report = LoadTestReport::default();
            report.transactions.push(TxMetrics {
                signature: "signature".to_string(),
                submit_time_ms: 0,
                confirm_time_ms: None,
                latency_ms: None,
                compute_units: None,
                slot: None,
                outcome: TxMetrics::succeeded_outcome(),
                payload: Vec::new(),
                payload_hash: String::new(),
                source_address: String::new(),
                gmp_destination_chain: String::new(),
                gmp_destination_address: String::new(),
                send_instant: None,
                amplifier_timing: None,
            });

            merge_timings(
                &mut report,
                vec![(message_id, AmplifierTiming::default())],
                matcher,
                Network::Testnet,
            );

            assert!(report.transactions[0].amplifier_timing.is_some());
        }
    }
}
