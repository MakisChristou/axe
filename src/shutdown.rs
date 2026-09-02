use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio_util::sync::CancellationToken;

use crate::ui;

const FORCED_EXIT_CODE: i32 = 130;
static PROCESS_SHUTDOWN: OnceLock<Arc<Shutdown>> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
pub enum DrainTarget {
    Intent,
    RoundTrip,
    QuoteRequests,
    LoadTestSubmissions,
}

impl DrainTarget {
    const fn message(self) -> &'static str {
        match self {
            Self::Intent => "shutting down gracefully; waiting for the current intent to finish",
            Self::RoundTrip => {
                "shutting down gracefully; waiting for the current round trip to finish"
            }
            Self::QuoteRequests => {
                "shutting down gracefully; waiting for in-flight quote requests to finish"
            }
            Self::LoadTestSubmissions => {
                "shutting down gracefully; stopping new submissions and waiting for in-flight work"
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InterruptAction {
    Drain,
    ArmForce,
    Force,
}

pub struct Shutdown {
    graceful: CancellationToken,
    interrupts: AtomicU8,
    target: DrainTarget,
}

impl Shutdown {
    pub fn install(target: DrainTarget) -> Arc<Self> {
        Arc::clone(PROCESS_SHUTDOWN.get_or_init(|| {
            let shutdown = Arc::new(Self::new(target));
            let listener = Arc::clone(&shutdown);
            tokio::spawn(async move {
                listener.listen().await;
            });
            shutdown
        }))
    }

    pub fn current() -> Option<Arc<Self>> {
        PROCESS_SHUTDOWN.get().map(Arc::clone)
    }

    pub fn requested(&self) -> bool {
        self.graceful.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.graceful.cancelled().await;
    }

    #[cfg(test)]
    pub(crate) fn test_instance(target: DrainTarget) -> Arc<Self> {
        Arc::new(Self::new(target))
    }

    #[cfg(test)]
    pub(crate) fn request_for_test(&self) {
        self.graceful.cancel();
    }

    async fn listen(&self) {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            match self.register_interrupt() {
                InterruptAction::Drain => ui::warn_stderr(self.target.message()),
                InterruptAction::ArmForce => {
                    ui::warn_stderr("press Ctrl-C once more to stop immediately")
                }
                InterruptAction::Force => {
                    ui::warn_stderr("forcing immediate shutdown");
                    std::process::exit(FORCED_EXIT_CODE);
                }
            }
        }
    }

    fn new(target: DrainTarget) -> Self {
        Self {
            graceful: CancellationToken::new(),
            interrupts: AtomicU8::new(0),
            target,
        }
    }

    fn register_interrupt(&self) -> InterruptAction {
        match self.interrupts.fetch_add(1, Ordering::Relaxed) {
            0 => {
                self.graceful.cancel();
                InterruptAction::Drain
            }
            1 => InterruptAction::ArmForce,
            _ => InterruptAction::Force,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_interrupts_escalate_from_drain_to_force() {
        let shutdown = Shutdown::new(DrainTarget::RoundTrip);
        assert!(!shutdown.requested());

        assert_eq!(shutdown.register_interrupt(), InterruptAction::Drain);
        assert!(shutdown.requested());
        assert_eq!(shutdown.register_interrupt(), InterruptAction::ArmForce);
        assert_eq!(shutdown.register_interrupt(), InterruptAction::Force);
    }

    #[test]
    fn drain_messages_describe_the_work_that_will_finish() {
        assert!(DrainTarget::Intent.message().contains("current intent"));
        assert!(
            DrainTarget::QuoteRequests
                .message()
                .contains("in-flight quote requests")
        );
        assert!(
            DrainTarget::LoadTestSubmissions
                .message()
                .contains("stopping new submissions")
        );
    }
}
