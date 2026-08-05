//! Retry policy for scheduled submissions.

use std::future::Future;
use std::time::Duration;
use tokio::time;

use super::metrics::TxMetrics;

const MAX_RATE_LIMIT_RETRIES: u32 = 5;

/// Retry only explicit HTTP 429 failures with bounded exponential backoff.
///
/// Every non-rate-limit result is terminal. The returned value is always a
/// concrete outcome, avoiding the `Option<TxMetrics>` state previously used
/// by each route to represent "the retry loop has not run yet".
pub(super) async fn rate_limited<F, Fut>(mut submit: F) -> TxMetrics
where
    F: FnMut() -> Fut,
    Fut: Future<Output = TxMetrics>,
{
    let mut attempt = 0;
    loop {
        let result = submit().await;
        if result.is_success() || attempt == MAX_RATE_LIMIT_RETRIES || !result.is_rate_limited() {
            return result;
        }
        time::sleep(Duration::from_secs(1 << attempt)).await;
        attempt += 1;
    }
}
