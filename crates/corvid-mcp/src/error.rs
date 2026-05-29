//! Errors returned by the MCP tool layer.

use thiserror::Error;

/// An error handling a tool call.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The requested tool name is not recognized.
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    /// The parameters were missing or the wrong shape.
    #[error("bad params: {0}")]
    BadParams(String),

    /// The underlying engine returned an error.
    #[error(transparent)]
    Engine(#[from] corvid::Error),
}
