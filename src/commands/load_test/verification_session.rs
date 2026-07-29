//! Streaming-verification lifecycle shared by the GMP and ITS routes.
//!
//! Both protocols run the same choreography: open a channel, spawn a verifier
//! that waits for the send phase's spinner, feed it confirmed transactions,
//! then join it and fold the per-message timings back into the report. Only
//! the destination adapter and the message-id shape differ, so those are the
//! two things this module is generic over.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use eyre::{Result, WrapErr, eyre};
use indicatif::ProgressBar;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::identifiers::MessageId;
use super::metrics::{LoadTestReport, TxMetrics, VerificationReport};
use super::verify::{PendingTx, VerificationRoute};
use super::{LoadTestArgs, finish_report};
use crate::types::Network;

/// Per-message timings a streaming verifier reports back alongside its
/// summary.
pub(super) use super::verify::StreamingTimings as Timings;

pub(super) type StreamingResult = Result<(VerificationReport, Timings)>;

/// A destination that can verify a completed batch of source transactions.
pub(super) trait BatchTarget {
    fn verify_batch(
        self,
        route: VerificationRoute,
        metrics: &mut [TxMetrics],
    ) -> impl Future<Output = Result<VerificationReport>>;
}

pub(super) async fn attach_batch<T: BatchTarget>(
    args: &LoadTestArgs,
    target: T,
    report: &mut LoadTestReport,
) -> Result<()> {
    let route = VerificationRoute::from_args(args);
    report.verification = Some(target.verify_batch(route, &mut report.transactions).await?);
    Ok(())
}

pub(super) async fn finish_batch<T: BatchTarget>(
    args: &LoadTestArgs,
    target: T,
    report: &mut LoadTestReport,
    test_start: Instant,
) -> Result<()> {
    attach_batch(args, target, report).await?;
    finish_report(report, test_start)
}

/// How a timing's message id maps back to the source transaction that
/// produced it.
#[derive(Clone, Copy)]
pub(super) enum MessageMatcher {
    /// The message id is the source signature verbatim.
    Exact,
    /// Solana appends `-{call_contract_index}.1` to the signature.
    Solana,
}

impl MessageMatcher {
    fn matches(self, signature: &str, message_id: &MessageId, network: Network) -> bool {
        if signature == message_id.as_ref() {
            return true;
        }
        match self {
            Self::Exact => false,
            Self::Solana => {
                let index = crate::solana::solana_call_contract_index(network);
                format!("{signature}-{index}.1") == message_id.as_ref()
            }
        }
    }
}

/// A destination that can verify transfers while sends are still in flight.
///
/// Implementors own their connection details so the session can move them
/// into a spawned task.
pub(super) trait StreamingTarget: Send + 'static {
    /// How this target's reported message ids map back to source signatures.
    fn message_matcher(&self) -> MessageMatcher {
        MessageMatcher::Exact
    }

    fn verify_streaming(
        self,
        route: VerificationRoute,
        rx: mpsc::UnboundedReceiver<PendingTx>,
        send_done: Arc<AtomicBool>,
        spinner: ProgressBar,
    ) -> impl Future<Output = StreamingResult> + Send;
}

/// Owns the channels and task connecting a sustained sender to a destination
/// verifier.
pub(super) struct VerificationSession {
    verify_tx: mpsc::UnboundedSender<PendingTx>,
    send_done: Arc<AtomicBool>,
    spinner_tx: Option<oneshot::Sender<ProgressBar>>,
    verify_handle: Option<JoinHandle<StreamingResult>>,
    matcher: MessageMatcher,
}

impl VerificationSession {
    pub(super) fn start<T: StreamingTarget>(route: VerificationRoute, target: T) -> Self {
        let matcher = target.message_matcher();
        let (verify_tx, verify_rx) = mpsc::unbounded_channel();
        let send_done = Arc::new(AtomicBool::new(false));
        let (spinner_tx, spinner_rx) = oneshot::channel();
        let verifier_done = Arc::clone(&send_done);
        let verify_handle = tokio::spawn(async move {
            let spinner = spinner_rx
                .await
                .wrap_err("verification spinner was not attached")?;
            target
                .verify_streaming(route, verify_rx, verifier_done, spinner)
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

    /// Hand the send phase's progress bar to the verifier so both phases
    /// share one line of output.
    pub(super) fn attach_spinner(&mut self, spinner: ProgressBar) -> Result<()> {
        self.spinner_sender()?
            .send(spinner)
            .map_err(|_| eyre!("verification task stopped before the send phase"))
    }

    /// Claim the spinner channel for a sender that builds the progress bar
    /// itself, deeper in the call stack than the session lives.
    pub(super) fn spinner_sender(&mut self) -> Result<oneshot::Sender<ProgressBar>> {
        self.spinner_tx
            .take()
            .ok_or_else(|| eyre!("verification spinner already attached"))
    }

    /// Await the verifier and fold its summary and timings into `report`.
    pub(super) async fn finish(
        mut self,
        report: &mut LoadTestReport,
        network: Network,
    ) -> Result<()> {
        let handle = self
            .verify_handle
            .take()
            .ok_or_else(|| eyre!("verification session already finished"))?;
        let (verification, timings) = handle.await.wrap_err("verification task failed")??;
        merge_timings(self.matcher, report, timings, network);
        report.verification = Some(verification);
        Ok(())
    }
}

fn merge_timings(
    matcher: MessageMatcher,
    report: &mut LoadTestReport,
    timings: Timings,
    network: Network,
) {
    for (message_id, timing) in timings {
        if let Some(transaction) = report
            .transactions
            .iter_mut()
            .find(|transaction| matcher.matches(&transaction.signature, &message_id, network))
        {
            transaction.amplifier_timing = Some(timing);
        }
    }
}

impl Drop for VerificationSession {
    fn drop(&mut self) {
        if let Some(handle) = &self.verify_handle {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MessageMatcher, merge_timings};
    use crate::commands::load_test::metrics::{AmplifierTiming, LoadTestReport, TxMetrics};
    use crate::types::Network;

    fn report_with_signature(signature: &str) -> LoadTestReport {
        let mut report = LoadTestReport::default();
        report.transactions.push(TxMetrics::succeeded(signature, 0));
        report
    }

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
            let mut report = report_with_signature("signature");

            merge_timings(
                matcher,
                &mut report,
                vec![(message_id.into(), AmplifierTiming::default())],
                Network::Testnet,
            );

            assert!(report.transactions[0].amplifier_timing.is_some());
        }
    }

    #[test]
    fn exact_matching_ignores_the_solana_suffix() {
        let mut report = report_with_signature("signature");

        merge_timings(
            MessageMatcher::Exact,
            &mut report,
            vec![("signature-1.1".into(), AmplifierTiming::default())],
            Network::Testnet,
        );

        assert!(report.transactions[0].amplifier_timing.is_none());
    }
}
