//! Shared outbound HTTP client with bounded timeouts.
//!
//! Every ad-hoc `reqwest::Client::new()` / `reqwest::get(..)` ships with NO
//! timeout, so a single stalled RPC connection hangs its route until the CI
//! job timeout kills it (observed: 30 min on xrpl -> xrpl-evm in cron run
//! 32638549694). All outbound HTTP goes through this client instead, and
//! `clippy.toml` disallows the untimed constructors.

use std::sync::LazyLock;
use std::time::Duration;

/// Total per-request deadline. Generous for every JSON-RPC/REST call axe
/// makes - retries and endpoint fallback layer on top of this at call sites.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on TCP+TLS establishment, so a black-holed endpoint fails fast
/// instead of consuming the whole request deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        // Builder failure means the TLS backend could not initialize, and
        // nothing network-related works then, so surface it at first use.
        .unwrap_or_default()
});

/// The process-wide timed HTTP client. Cheap to clone (shared pool).
pub fn client() -> &'static reqwest::Client {
    &CLIENT
}
