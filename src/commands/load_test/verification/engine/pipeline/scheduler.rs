//! Verification polling scheduler.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::sync::mpsc;

use super::super::{INACTIVITY_TIMEOUT, PendingTx};

#[derive(Debug)]
pub(super) enum PollAction {
    Finished,
    Waiting,
    Ready {
        active: Vec<usize>,
        total: usize,
        sending_complete: bool,
    },
}

/// Scheduling shared by every polling pipeline: stream ingestion, terminal
/// detection, idle waiting, and inactivity timeout accounting.
pub(super) struct PollScheduler {
    last_progress: Instant,
}

impl PollScheduler {
    pub(super) fn new() -> Self {
        Self {
            last_progress: Instant::now(),
        }
    }

    pub(super) fn next_action(
        &mut self,
        txs: &mut Vec<PendingTx>,
        receiver: Option<&mut mpsc::UnboundedReceiver<PendingTx>>,
        send_done: Option<&AtomicBool>,
        mut normalize: impl FnMut(&mut PendingTx),
    ) -> PollAction {
        if let Some(receiver) = receiver {
            while let Ok(mut tx) = receiver.try_recv() {
                normalize(&mut tx);
                txs.push(tx);
            }
        }

        let sending_complete = send_done.is_none_or(|done| done.load(Ordering::Relaxed));
        if txs.is_empty() {
            return if sending_complete {
                PollAction::Finished
            } else {
                PollAction::Waiting
            };
        }

        let active = txs
            .iter()
            .enumerate()
            .filter_map(|(index, tx)| tx.is_active().then_some(index))
            .collect::<Vec<_>>();
        if active.is_empty() {
            if sending_complete {
                PollAction::Finished
            } else {
                // Future streamed transactions start a fresh inactivity
                // window after all currently received transactions settle.
                self.mark_progress();
                PollAction::Waiting
            }
        } else {
            PollAction::Ready {
                active,
                total: txs.len(),
                sending_complete,
            }
        }
    }

    pub(super) fn mark_progress(&mut self) {
        self.last_progress = Instant::now();
    }

    pub(super) fn timed_out(&self, sending_complete: bool) -> bool {
        let timeout = if sending_complete {
            INACTIVITY_TIMEOUT
        } else {
            INACTIVITY_TIMEOUT * 2
        };
        self.last_progress.elapsed() >= timeout
    }
}
