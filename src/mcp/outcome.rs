//! The result contract every tool returns.

use eyre::Result;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;

use crate::ui;

/// A structured payload plus a one-line summary.
///
/// The payload is what makes results composable: an agent can feed a field
/// from one call into the next question. The summary saves it from inferring
/// success by walking the payload. Both are needed, so both are mandatory here
/// rather than one being optional.
pub struct Outcome {
    summary: String,
    payload: serde_json::Value,
}

impl Outcome {
    /// Build an outcome from any serializable result.
    ///
    /// The summary is scrubbed of URLs for the same reason the load-test
    /// report already scrubs them: RPC endpoints carry provider API keys, and
    /// a summary is the part most likely to be quoted back verbatim.
    pub fn new<T: Serialize>(summary: impl Into<String>, value: &T) -> Result<Self> {
        Ok(Self {
            summary: ui::scrub_urls(&summary.into()),
            payload: serde_json::to_value(value)?,
        })
    }

    /// Render into the protocol's tool result: the summary as text content so
    /// a human reading the transcript sees it, the payload as structured
    /// content so the agent can address fields.
    pub fn into_tool_result(self) -> CallToolResult {
        let mut result = CallToolResult::structured(self.payload);
        // `structured` stuffs the whole payload into the text content, which
        // would make an agent read the same data twice. The summary is what a
        // human wants to see in the transcript.
        result.content = vec![ContentBlock::text(self.summary)];
        result
    }
}

/// Map an internal failure onto a protocol error.
///
/// URLs are scrubbed here too: an error is the most likely place for a raw
/// RPC endpoint to surface, since it often carries the request that failed.
pub fn to_error_data(context: &str, err: &eyre::Report) -> ErrorData {
    ErrorData::internal_error(ui::scrub_urls(&format!("{context}: {err:#}")), None)
}
