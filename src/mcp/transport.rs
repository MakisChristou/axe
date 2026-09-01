//! The only module that names a transport.
//!
//! Everything above this speaks the protocol through rmcp's service traits, so
//! adding a second transport means adding a function here and nothing else.

use eyre::Result;
use rmcp::ServiceExt;
use rmcp::transport::stdio;

use crate::mcp::server::AxeMcp;

/// Serve over stdio until the client closes the connection.
///
/// The client launches axe as a child process, so stdin and stdout are the
/// channel and the process inherits the operator's environment. That is what
/// lets keys stay out of the tool schemas.
pub async fn serve_stdio(server: AxeMcp) -> Result<()> {
    let running = server
        .serve(stdio())
        .await
        .map_err(|e| eyre::eyre!("MCP server failed to start: {e}"))?;

    running
        .waiting()
        .await
        .map_err(|e| eyre::eyre!("MCP server stopped unexpectedly: {e}"))?;

    Ok(())
}
