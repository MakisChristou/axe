//! Generic retry helper for transient RPC errors — 5xx upstreams, 429 rate
//! limits, dropped connections, and timeouts. Designed for single-shot
//! JSON-RPC or HTTP calls that may fail due to upstream flakiness but
//! would succeed on the next try.
//!
//! NOT for deterministic failures (revert, "not found", "insufficient
//! balance"): those need to surface immediately rather than burn the
//! retry budget. Pass an `is_transient` predicate that returns `false`
//! for known-permanent errors when in doubt.
//!
//! Composes with the per-client fallback patterns we already have
//! (`SuiClient::call` iterates configured endpoints; Solana
//! `fetch_confirmed_tx` retries 15× with capped exponential backoff).
//! `retry_async` adds an outer time-axis retry for the call sites that
//! don't already have one (alloy provider calls, XRPL `account_info`,
//! Stellar reads, etc.).

use std::future::Future;
use std::time::Duration;

/// Default attempts for `retry_all`. 3 attempts × geometric backoff
/// (500ms, 1s, 2s) ≈ 3.5s worst-case wall-clock if every attempt fails.
pub const DEFAULT_ATTEMPTS: u32 = 3;

/// Base backoff between attempts. Doubles each subsequent retry:
/// 500ms → 1s → 2s → 4s. Capped at 8s for any single sleep.
const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 8_000;

/// Retry an async fallible operation with geometric backoff. `op` must be
/// re-callable — it'll be invoked once per attempt, so any per-call state
/// (request body, fresh provider, etc.) needs to be re-constructed inside
/// the closure rather than captured by reference.
///
/// `is_transient` decides whether each error is worth retrying. Return
/// `true` to retry, `false` to bail immediately with that error. Useful
/// for separating "RPC was flaky" from "revert: insufficient balance" so
/// real errors aren't masked.
///
/// Logs a `ui::warn` line per retry so the run output makes it visible
/// when transient errors are happening (silent retries hide upstream
/// degradation).
pub async fn retry_async<F, Fut, T, E, P>(
    label: &str,
    attempts: u32,
    is_transient: P,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
    P: Fn(&E) -> bool,
{
    let mut attempt: u32 = 0;
    loop {
        match op().await {
            Ok(t) => return Ok(t),
            Err(e) if attempt + 1 < attempts && is_transient(&e) => {
                let backoff_ms = (BASE_BACKOFF_MS << attempt).min(MAX_BACKOFF_MS);
                crate::ui::warn(&format!(
                    "{label}: attempt {} failed: {e}; retrying in {backoff_ms}ms",
                    attempt + 1,
                ));
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Convenience: retry treating ALL errors as transient. Appropriate for
/// pure read-only / idempotent calls (`get_balance`, `get_code`,
/// `account_info`, view calls) where any error is plausibly an RPC
/// hiccup. Uses `DEFAULT_ATTEMPTS`.
pub async fn retry_all<F, Fut, T, E>(label: &str, op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    retry_async(label, DEFAULT_ATTEMPTS, |_| true, op).await
}

/// Default predicate: classify common error-string signatures as
/// transient. Matches HTTP 5xx, 429, "timeout", "connection", "network",
/// and JSON-RPC server-error messages. Use this for state-changing calls
/// (`send_transaction`, `submit`) where you want to retry network
/// flakiness but NOT contract reverts.
pub fn is_transient_default<E: std::fmt::Display>(err: &E) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("timeout")
        || msg.contains("timed out")
        || msg.contains("connection")
        || msg.contains("network")
        || msg.contains("502")
        || msg.contains("503")
        || msg.contains("504")
        || msg.contains("429")
        || msg.contains("too many requests")
        || msg.contains("bad gateway")
        || msg.contains("service unavailable")
        || msg.contains("gateway timeout")
        || msg.contains("server error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retry_succeeds_on_second_attempt() {
        let counter = AtomicU32::new(0);
        let result: Result<&'static str, &'static str> = retry_all("test", || {
            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    Err("transient")
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result, Ok("ok"));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_gives_up_after_max_attempts() {
        let counter = AtomicU32::new(0);
        let result: Result<(), &'static str> = retry_all("test", || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err("always fails") }
        })
        .await;
        assert_eq!(result, Err("always fails"));
        assert_eq!(counter.load(Ordering::SeqCst), DEFAULT_ATTEMPTS);
    }

    #[tokio::test]
    async fn retry_bails_immediately_when_predicate_says_not_transient() {
        let counter = AtomicU32::new(0);
        let result: Result<(), &'static str> = retry_async(
            "test",
            DEFAULT_ATTEMPTS,
            |_| false,
            || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Err("permanent") }
            },
        )
        .await;
        assert_eq!(result, Err("permanent"));
        assert_eq!(counter.load(Ordering::SeqCst), 1, "should not retry");
    }

    #[test]
    fn is_transient_classifies_common_strings() {
        for s in [
            "request timeout",
            "connection reset",
            "HTTP 502 bad gateway",
            "503 service unavailable",
            "429 Too Many Requests",
            "network error",
        ] {
            assert!(is_transient_default(&s), "expected transient: {s}");
        }
        for s in [
            "execution reverted: InsufficientBalance",
            "account not found",
            "invalid signature",
            "Wrong tokenManager type",
        ] {
            assert!(!is_transient_default(&s), "expected NOT transient: {s}");
        }
    }
}
