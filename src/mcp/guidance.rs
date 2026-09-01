//! What an agent needs to know before it tries anything.
//!
//! Route validity is answered by delegating to the same `SupportedRoute`
//! resolution the load test performs, so the answer cannot drift from what
//! actually runs. The narrative documentation is served as resources so an
//! agent can read it on demand rather than carrying it in every request.

use rmcp::model::Resource;
use serde::Serialize;

use crate::commands::load_test::route::is_supported;
use crate::commands::load_test::{Protocol, TestType};

/// One documentation page, embedded at compile time.
struct DocPage {
    file: &'static str,
    title: &'static str,
    description: &'static str,
    body: &'static str,
}

/// The pages worth putting in front of an agent, in the order a newcomer
/// would want them.
///
/// Curated rather than a directory scan, so a stray file cannot silently
/// become part of the contract, and embedded rather than read from disk, so
/// an installed binary serves them without needing the repo alongside it.
const DOC_PAGES: &[DocPage] = &[
    DocPage {
        file: "routes.md",
        title: "Supported routes",
        description: "Which chain pairs and protocols work, per network",
        body: include_str!("../../docs/routes.md"),
    },
    DocPage {
        file: "load-testing.md",
        title: "Testing and load testing",
        description: "Single messages, burst and sustained modes, per-chain keys",
        body: include_str!("../../docs/load-testing.md"),
    },
    DocPage {
        file: "load-test-coverage.md",
        title: "Load-test coverage matrix",
        description: "Dispatcher support by chain type",
        body: include_str!("../../docs/load-test-coverage.md"),
    },
    DocPage {
        file: "decode.md",
        title: "Decoding",
        description: "Calldata, transactions, and on-chain activity",
        body: include_str!("../../docs/decode.md"),
    },
    DocPage {
        file: "monitoring.md",
        title: "Monitoring",
        description: "Verifiers, votes, and ITS ownership",
        body: include_str!("../../docs/monitoring.md"),
    },
    DocPage {
        file: "axelar-debugging.md",
        title: "Debugging cross-chain messages",
        description: "Tracing GMP and ITS through the pipeline",
        body: include_str!("../../docs/axelar-debugging.md"),
    },
];

fn uri_for(file: &str) -> String {
    format!("axe://docs/{file}")
}

/// Whether a route can be attempted, and why not when it cannot.
#[derive(Debug, Serialize)]
pub struct RouteSupport {
    pub protocol: String,
    pub route: String,
    pub source_chain: String,
    pub destination_chain: String,
    pub supported: bool,
    /// Why the route was rejected, absent when it is supported.
    pub reason: Option<String>,
}

/// Ask the load test's own resolver whether a route is viable.
pub fn check_route(
    protocol: Protocol,
    route: TestType,
    source_chain: &str,
    destination_chain: &str,
) -> RouteSupport {
    let outcome = is_supported(protocol, route, source_chain, destination_chain);
    RouteSupport {
        protocol: format!("{protocol:?}").to_lowercase(),
        route: format!("{route:?}"),
        source_chain: source_chain.to_string(),
        destination_chain: destination_chain.to_string(),
        supported: outcome.is_ok(),
        reason: outcome.err().map(|e| format!("{e:#}")),
    }
}

/// The documentation pages, as protocol resources.
pub fn doc_resources() -> Vec<Resource> {
    DOC_PAGES
        .iter()
        .map(|page| {
            Resource::new(uri_for(page.file), page.title)
                .with_description(page.description)
                .with_mime_type("text/markdown")
        })
        .collect()
}

/// The body of a documentation page, or `None` for any URI outside the
/// curated list. Matching against the list is what stops a URI being used to
/// read arbitrary files.
pub fn doc_body(uri: &str) -> Option<&'static str> {
    DOC_PAGES
        .iter()
        .find(|page| uri == uri_for(page.file))
        .map(|page| page.body)
}
