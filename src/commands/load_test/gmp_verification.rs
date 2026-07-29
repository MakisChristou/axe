//! GMP destination adapters over the existing `verify_onchain_*` entry points.
//!
//! Route modules retain protocol choices and source submission; the adapters
//! here only pick the chain-specific verifier. Channel ownership, task
//! lifetime, timing merge, and completion live in
//! [`super::verification_session`], shared with the ITS routes.

use alloy::primitives::Address;
use alloy::providers::Provider;
use eyre::Result;
use indicatif::ProgressBar;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{mpsc, oneshot};

use super::LoadTestArgs;
use super::metrics::{LoadTestReport, TxMetrics, VerificationReport};
use super::verification_session::{
    BatchTarget, MessageMatcher, StreamingResult, StreamingTarget, VerificationSession,
};
pub(super) use super::verification_session::{attach_batch, finish_batch};
use super::verify::{
    self, EvmGmpDestination, EvmGmpStreamingDestination, GmpBatchVerification, PendingTx,
    SolanaGmpDestination, SourceChainType, StellarGmpDestination, StreamingVerification,
    SuiGmpDestination, VerificationRoute,
};

/// Resources a GMP source adapter needs to feed a streaming verifier.
pub(super) struct StreamingSendContext {
    pub verify_tx: mpsc::UnboundedSender<PendingTx>,
    pub send_done: Arc<AtomicBool>,
    pub spinner_tx: oneshot::Sender<ProgressBar>,
}

/// Run the chain-neutral outer lifecycle for a sustained GMP route.
///
/// Source adapters still own their pacing, spinner creation, and transaction
/// semantics. This helper owns only the repeated session startup, channels,
/// join, and timing merge.
pub(super) async fn run_streaming<T, F, Fut>(
    args: &LoadTestArgs,
    target: T,
    send: F,
) -> Result<LoadTestReport>
where
    T: StreamingTarget,
    F: FnOnce(StreamingSendContext) -> Fut,
    Fut: Future<Output = Result<LoadTestReport>>,
{
    let mut verification = VerificationSession::start(VerificationRoute::from_args(args), target);
    let context = StreamingSendContext {
        verify_tx: verification.sender(),
        send_done: verification.send_done(),
        spinner_tx: verification.spinner_sender()?,
    };
    let mut report = send(context).await?;
    verification.finish(&mut report, args.network).await?;
    Ok(report)
}

/// EVM destination. Amplifier and legacy gateways expose different approval
/// APIs, so the gateway generation is part of the target.
pub(super) struct EvmGmpTarget<P> {
    pub address: String,
    pub gateway_addr: Address,
    pub provider: P,
    pub source_type: SourceChainType,
    pub legacy: bool,
}

impl<P: Provider> BatchTarget for EvmGmpTarget<P> {
    async fn verify_batch(
        self,
        route: VerificationRoute,
        metrics: &mut [TxMetrics],
    ) -> Result<VerificationReport> {
        let request = GmpBatchVerification {
            route,
            destination: EvmGmpDestination {
                address: self.address,
                gateway_addr: self.gateway_addr,
                provider: &self.provider,
            },
            source_type: self.source_type,
        };
        if self.legacy {
            verify::verify_onchain_evm_legacy(metrics, request).await
        } else {
            verify::verify_onchain(metrics, request).await
        }
    }
}

pub(super) struct SolanaGmpTarget {
    pub address: String,
    pub rpc_url: String,
    pub source_type: SourceChainType,
}

impl BatchTarget for SolanaGmpTarget {
    fn verify_batch(
        self,
        route: VerificationRoute,
        metrics: &mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> {
        verify::verify_onchain_solana(
            metrics,
            GmpBatchVerification {
                route,
                destination: SolanaGmpDestination {
                    address: self.address,
                    rpc_url: self.rpc_url,
                },
                source_type: self.source_type,
            },
        )
    }
}

pub(super) struct StellarGmpTarget {
    pub contract: String,
    pub rpc_url: String,
    pub network_type: String,
    pub gateway_contract: String,
    pub signer_pk: [u8; 32],
    pub source_type: SourceChainType,
}

impl BatchTarget for StellarGmpTarget {
    fn verify_batch(
        self,
        route: VerificationRoute,
        metrics: &mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> {
        verify::verify_onchain_stellar_gmp(
            metrics,
            GmpBatchVerification {
                route,
                destination: StellarGmpDestination {
                    contract: self.contract,
                    rpc_url: self.rpc_url,
                    network_type: self.network_type,
                    gateway_contract: self.gateway_contract,
                    signer_pk: self.signer_pk,
                },
                source_type: self.source_type,
            },
        )
    }
}

pub(super) struct SuiGmpTarget {
    pub address: String,
    pub rpc_url: String,
    pub source_type: SourceChainType,
}

impl BatchTarget for SuiGmpTarget {
    fn verify_batch(
        self,
        route: VerificationRoute,
        metrics: &mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>> {
        verify::verify_onchain_sui_gmp(
            metrics,
            GmpBatchVerification {
                route,
                destination: SuiGmpDestination {
                    address: self.address,
                    rpc_url: self.rpc_url,
                },
                source_type: self.source_type,
            },
        )
    }
}

pub(super) struct EvmGmpStreamingTarget {
    pub address: String,
    pub gateway_addr: Address,
    pub rpc_url: String,
    pub legacy: bool,
    pub message_matcher: MessageMatcher,
}

impl StreamingTarget for EvmGmpStreamingTarget {
    fn message_matcher(&self) -> MessageMatcher {
        self.message_matcher
    }

    async fn verify_streaming(
        self,
        route: VerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> StreamingResult {
        let request = StreamingVerification {
            route,
            destination: EvmGmpStreamingDestination {
                address: self.address,
                gateway_addr: self.gateway_addr,
                rpc_url: self.rpc_url,
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

impl StreamingTarget for SolanaGmpStreamingTarget {
    fn message_matcher(&self) -> MessageMatcher {
        MessageMatcher::Solana
    }

    fn verify_streaming(
        self,
        route: VerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = StreamingResult> + Send {
        verify::verify_onchain_solana_streaming(StreamingVerification {
            route,
            destination: SolanaGmpDestination {
                address: self.address,
                rpc_url: self.rpc_url,
            },
            rx,
            send_done,
            spinner,
        })
    }
}
