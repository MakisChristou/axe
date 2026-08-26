//! Startup state, fixed for the life of the server.

use std::path::PathBuf;

use eyre::Result;

use crate::mcp::runs::RunRegistry;
use crate::types::Network;

/// What the operator chose when they started the server.
///
/// The network lives here rather than in tool arguments because the ITS and
/// GMP caches are scoped by a process-global, write-once network value. Chain
/// ids are not unique across Axelar networks, and letting one process serve
/// two networks would silently reuse the first network's cache for the second
/// -- a deterministic revert that has been observed live. Fixing the network
/// at startup reproduces the invariant the CLI already relies on.
#[derive(Clone)]
pub struct McpContext {
    network: Network,
    runs: RunRegistry,
}

impl McpContext {
    /// Refuse mainnet unless the operator opted in when starting the server.
    pub fn new(network: Network, allow_mainnet: bool, reports_dir: PathBuf) -> Result<Self> {
        if network == Network::Mainnet && !allow_mainnet {
            return Err(eyre::eyre!(
                "refusing to serve mainnet without --allow-mainnet: these flows spend real funds"
            ));
        }

        Ok(Self {
            network,
            runs: RunRegistry::new(reports_dir),
        })
    }

    /// The pinned network. No tool can change it.
    pub fn network(&self) -> Network {
        self.network
    }

    /// Background load-test runs started through this server.
    pub fn runs(&self) -> &RunRegistry {
        &self.runs
    }
}
