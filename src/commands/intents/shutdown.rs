use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio_util::sync::CancellationToken;

use crate::ui;

const FORCED_EXIT_CODE: i32 = 130;

#[derive(Debug, PartialEq, Eq)]
enum InterruptAction {
    Drain,
    ArmForce,
    Force,
}

pub struct Shutdown {
    graceful: CancellationToken,
    interrupts: AtomicU8,
}

impl Shutdown {
    pub fn install() -> Arc<Self> {
        let shutdown = Arc::new(Self::new());
        let listener = Arc::clone(&shutdown);
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break;
                }
                match listener.register_interrupt() {
                    InterruptAction::Drain => ui::warn(
                        "shutting down gracefully; waiting for the current round trip to finish",
                    ),
                    InterruptAction::ArmForce => {
                        ui::warn("press Ctrl-C once more to stop immediately")
                    }
                    InterruptAction::Force => {
                        ui::warn("forcing immediate shutdown");
                        std::process::exit(FORCED_EXIT_CODE);
                    }
                }
            }
        });
        shutdown
    }

    pub fn requested(&self) -> bool {
        self.graceful.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.graceful.cancelled().await;
    }

    fn new() -> Self {
        Self {
            graceful: CancellationToken::new(),
            interrupts: AtomicU8::new(0),
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
        let shutdown = Shutdown::new();
        assert!(!shutdown.requested());

        assert_eq!(shutdown.register_interrupt(), InterruptAction::Drain);
        assert!(shutdown.requested());
        assert_eq!(shutdown.register_interrupt(), InterruptAction::ArmForce);
        assert_eq!(shutdown.register_interrupt(), InterruptAction::Force);
    }
}
