//! MCP server front end.
//!
//! A second front end over the same command implementations the CLI calls, so
//! a flow behaves identically whether a human or an agent drives it.

use eyre::Result;

use crate::mcp::context::McpContext;
use crate::mcp::server::AxeMcp;
use crate::types::Network;

pub mod context;
pub mod guidance;
pub mod outcome;
pub mod runs;
pub mod server;
pub mod transport;

/// Start the server and serve until the client disconnects.
///
/// The network is taken once, here, and held for the process lifetime.
pub async fn serve(network: Network, allow_mainnet: bool) -> Result<()> {
    // A client-launched server inherits whatever directory the client chose,
    // so the CLI default of a relative path would put reports somewhere
    // unpredictable. Anchor them under the per-user data dir instead.
    crate::commands::load_test::set_report_dir(report_dir());

    let context = McpContext::new(network, allow_mainnet, report_dir())?;
    transport::serve_stdio(AxeMcp::new(context)).await
}

/// Where load-test reports written by this server land.
///
/// Deliberately not the CLI's working-directory-relative location, which
/// existing scripts glob and which must keep working unchanged.
fn report_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("axe")
        .join("load-test-runs")
}
