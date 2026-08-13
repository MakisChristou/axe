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

/// Attempts against EACH endpoint in `retry_with_fallback` before advancing
/// to the next one. 5 attempts × geometric backoff (500ms, 1s, 2s, 4s)
/// ≈ 7.5s worst-case per endpoint, ≈ 15s across a [private, public] pair.
pub const FALLBACK_ATTEMPTS: u32 = 5;

/// Base backoff between attempts. Doubles each subsequent retry:
/// 500ms → 1s → 2s → 4s. Capped at 8s for any single sleep.
const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 8_000;

/// Fraction each delay is randomly spread by, either side of its nominal
/// value. 0.2 gives the usual +/-20% band.
const JITTER_FRACTION: f64 = 0.2;

/// Spread `base` by +/-[`JITTER_FRACTION`] so concurrent retriers separate.
///
/// This matters whenever several runs contend for one resource - a shared
/// wallet's nonce, a rate-limited RPC, a Stellar account sequence. They fail
/// at the same instant, so an unjittered schedule makes them sleep the same
/// amount and collide again in lockstep, turning one collision into a run of
/// them. Spreading the waits lets one win each round.
pub(crate) fn jittered(base: Duration) -> Duration {
    use rand::Rng as _;
    let factor = rand::thread_rng().gen_range(1.0 - JITTER_FRACTION..1.0 + JITTER_FRACTION);
    base.mul_f64(factor)
}

/// Compute the geometric backoff for a zero-based attempt index, jittered.
pub(crate) fn backoff_for_attempt(attempt: u32) -> Duration {
    jittered(Duration::from_millis(
        (BASE_BACKOFF_MS << attempt.min(31)).min(MAX_BACKOFF_MS),
    ))
}

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
                let backoff = backoff_for_attempt(attempt);
                crate::ui::warn(&format!(
                    "{label}: attempt {} failed: {e}; retrying in {}ms",
                    attempt + 1,
                    backoff.as_millis(),
                ));
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Retry an operation against an ordered list of endpoints — typically
/// `[private RPC, public RPC]` — trying each endpoint up to
/// [`FALLBACK_ATTEMPTS`] times with geometric backoff before falling over to
/// the next. This is the application-level fallback that transport-level
/// failover cannot provide: a JSON-RPC *error body* (429, "state not
/// available for pending block", Hedera consensus timeouts) arrives as a
/// perfectly healthy HTTP 200, so only a layer that sees the decoded error
/// can decide to move to the next endpoint.
///
/// `op` receives one endpoint (cloned per attempt) and must be re-callable.
/// `is_transient` gates every retry AND the fallback itself: a deterministic
/// error (revert, "insufficient balance") returns immediately — the same
/// contract state answers on every endpoint, so retrying elsewhere just
/// delays the real error.
///
/// Returns the last endpoint's last error if every endpoint exhausts.
pub async fn retry_with_fallback<C, F, Fut, T, E, P>(
    label: &str,
    endpoints: &[C],
    is_transient: P,
    mut op: F,
) -> Result<T, E>
where
    C: Clone,
    F: FnMut(C) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
    P: Fn(&E) -> bool,
{
    debug_assert!(!endpoints.is_empty(), "retry_with_fallback: no endpoints");
    let last = endpoints.len().saturating_sub(1);
    let mut final_err: Option<E> = None;
    for (endpoint_idx, endpoint) in endpoints.iter().enumerate() {
        for attempt in 0..FALLBACK_ATTEMPTS {
            match op(endpoint.clone()).await {
                Ok(t) => return Ok(t),
                Err(e) if !is_transient(&e) => return Err(e),
                Err(e) if attempt + 1 < FALLBACK_ATTEMPTS => {
                    let backoff = backoff_for_attempt(attempt);
                    crate::ui::warn(&format!(
                        "{label}: endpoint {}/{} attempt {}/{FALLBACK_ATTEMPTS} failed: {e}; \
                         retrying in {}ms",
                        endpoint_idx + 1,
                        endpoints.len(),
                        attempt + 1,
                        backoff.as_millis(),
                    ));
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => {
                    if endpoint_idx < last {
                        crate::ui::warn(&format!(
                            "{label}: endpoint {}/{} exhausted ({FALLBACK_ATTEMPTS} attempts): \
                             {e}; falling back to next endpoint",
                            endpoint_idx + 1,
                            endpoints.len(),
                        ));
                    }
                    final_err = Some(e);
                }
            }
        }
    }
    // Unreachable while callers pass ≥1 endpoint (debug_assert above); the
    // loop structure guarantees `final_err` is set on every exhausted path.
    match final_err {
        Some(e) => Err(e),
        None => unreachable!("retry_with_fallback called with no endpoints"),
    }
}

/// Convenience: `retry_with_fallback` treating ALL errors as transient.
/// Appropriate for read-only / idempotent calls (`get_balance`, view calls,
/// receipt polls) where any error is plausibly endpoint-specific.
pub async fn retry_with_fallback_all<C, F, Fut, T, E>(
    label: &str,
    endpoints: &[C],
    op: F,
) -> Result<T, E>
where
    C: Clone,
    F: FnMut(C) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    retry_with_fallback(label, endpoints, |_| true, op).await
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
        // Avalanche coreth node that momentarily can't serve pending-block
        // state (nonce/gas fills, eth_call probes). Endpoint-transient — the
        // same call succeeds on retry or on the next endpoint.
        || msg.contains("state not available for pending block")
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

    #[tokio::test(start_paused = true)]
    async fn fallback_advances_to_second_endpoint_after_exhausting_first() {
        let counter = AtomicU32::new(0);
        let result: Result<u32, String> =
            retry_with_fallback_all("test", &["private", "public"], |endpoint| {
                counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if endpoint == "private" {
                        Err("429 too many requests".to_string())
                    } else {
                        Ok(42)
                    }
                }
            })
            .await;
        assert_eq!(result, Ok(42));
        // All FALLBACK_ATTEMPTS burned on the private endpoint, then the
        // public endpoint succeeds on its first attempt.
        assert_eq!(counter.load(Ordering::SeqCst), FALLBACK_ATTEMPTS + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fallback_returns_last_error_when_all_endpoints_exhaust() {
        let counter = AtomicU32::new(0);
        let result: Result<(), String> =
            retry_with_fallback_all("test", &["private", "public"], |endpoint| {
                counter.fetch_add(1, Ordering::SeqCst);
                async move { Err(format!("{endpoint} down")) }
            })
            .await;
        assert_eq!(result, Err("public down".to_string()));
        assert_eq!(counter.load(Ordering::SeqCst), FALLBACK_ATTEMPTS * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn fallback_bails_immediately_on_deterministic_error() {
        let counter = AtomicU32::new(0);
        let result: Result<(), &'static str> = retry_with_fallback(
            "test",
            &["private", "public"],
            |_| false,
            |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Err("execution reverted") }
            },
        )
        .await;
        assert_eq!(result, Err("execution reverted"));
        assert_eq!(counter.load(Ordering::SeqCst), 1, "no retry, no fallback");
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
            "error code -32000: state not available for pending block",
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
