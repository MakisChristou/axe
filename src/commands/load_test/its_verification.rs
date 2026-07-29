//! ITS destination adapters over the existing `verify_onchain_*_its` entry
//! points.
//!
//! An adapter only selects the chain-specific verifier. The streaming
//! lifecycle it plugs into lives in [`super::verification_session`], shared
//! with the GMP routes, so route modules only coordinate setup, submission
//! and reporting.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use alloy::primitives::Address;
use eyre::Result;
use indicatif::ProgressBar;
use tokio::sync::mpsc;

use super::metrics::{
    ComputeUnitSummary, LoadTestReport, ReportInput, RunIdentity, TxMetrics, VerificationReport,
};
use super::submitter::BurstResult;
use super::sustained::SustainedResult;
use super::verification_session::{
    BatchTarget, StreamingResult, StreamingTarget, VerificationSession,
};
use super::verify::{
    self, EvmItsDestination, ItsBatchVerification, PendingTx, SolanaItsDestination,
    StellarItsDestination, StreamingVerification, SuiItsDestination, VerificationRoute,
    XrplItsDestination,
};
use super::{LoadTestArgs, finish_report};

pub(super) use super::verification_session::finish_batch;

/// Build the batch and streaming impls for a destination whose only
/// difference is which `verify_onchain_*_its` pair it calls.
///
/// Every ITS target is the same shape — bundle the owned connection details
/// into the destination struct and hand it to one of two free functions — so
/// spelling that out per chain would be ten copies of the same eight lines.
macro_rules! its_target {
    (
        $target:ident, $destination:ident,
        batch = $batch:ident
        $(, streaming = $streaming:ident)?
        , fields = { $($field:ident: $ty:ty),* $(,)? }
    ) => {
        pub(super) struct $target {
            $(pub $field: $ty,)*
        }

        impl $target {
            fn destination(self) -> $destination {
                $destination { $($field: self.$field,)* }
            }
        }

        impl BatchTarget for $target {
            fn verify_batch(
                self,
                route: VerificationRoute,
                metrics: &mut [TxMetrics],
            ) -> impl Future<Output = Result<VerificationReport>> {
                verify::$batch(
                    metrics,
                    ItsBatchVerification { route, destination: self.destination() },
                )
            }
        }

        $(
            impl StreamingTarget for $target {
                fn verify_streaming(
                    self,
                    route: VerificationRoute,
                    rx: mpsc::UnboundedReceiver<PendingTx>,
                    send_done: Arc<AtomicBool>,
                    spinner: ProgressBar,
                ) -> impl Future<Output = StreamingResult> + Send {
                    verify::$streaming(StreamingVerification {
                        route,
                        destination: self.destination(),
                        rx,
                        send_done,
                        spinner,
                    })
                }
            }
        )?
    };
}

its_target!(
    EvmItsTarget, EvmItsDestination,
    batch = verify_onchain_evm_its,
    streaming = verify_onchain_evm_its_streaming,
    fields = { gateway_addr: Address, rpc_url: String }
);

its_target!(
    SolanaItsTarget, SolanaItsDestination,
    batch = verify_onchain_solana_its,
    streaming = verify_onchain_solana_its_streaming,
    fields = { rpc_url: String }
);

its_target!(
    StellarItsTarget, StellarItsDestination,
    batch = verify_onchain_stellar_its,
    streaming = verify_onchain_stellar_its_streaming,
    fields = {
        rpc_url: String,
        network_type: String,
        gateway_contract: String,
        signer_pk: [u8; 32],
    }
);

its_target!(
    XrplItsTarget, XrplItsDestination,
    batch = verify_onchain_xrpl_its,
    streaming = verify_onchain_xrpl_its_streaming,
    fields = { rpc_url: String, recipient: String }
);

// Sui has no streaming ITS verifier yet, so it implements batch only —
// routes cannot accidentally ask it for a mode it does not support.
its_target!(
    SuiItsTarget, SuiItsDestination,
    batch = verify_onchain_sui_its,
    fields = { rpc_url: String }
);

/// Report inputs a burst ITS route cannot derive from [`BurstResult`] alone.
pub(super) struct ItsBurstReport {
    pub destination_address: String,
    pub num_txs: u64,
    pub num_keys: usize,
    pub compute_unit_summary: ComputeUnitSummary,
}

pub(super) async fn finish_burst<T: BatchTarget>(
    args: &LoadTestArgs,
    target: T,
    burst: BurstResult,
    spec: ItsBurstReport,
    test_start: Instant,
) -> Result<()> {
    let mut report = LoadTestReport::from_transactions(
        ReportInput {
            run: RunIdentity::burst(args),
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

/// Join a sustained ITS run's verifier and emit the finished report.
pub(super) async fn finish_sustained(
    session: VerificationSession,
    args: &LoadTestArgs,
    result: SustainedResult,
    destination_address: &str,
    total_expected: u64,
    num_keys: usize,
    test_start: Instant,
) -> Result<()> {
    let plan = result.plan;
    let mut report = super::sustained::build_sustained_report(
        result,
        RunIdentity::sustained(args, plan),
        destination_address,
        total_expected,
        num_keys,
    );
    session.finish(&mut report, args.network).await?;
    finish_report(&mut report, test_start)
}
