//! corvid-mcp — a sidecar that exposes a [`corvid`] store to agentic tools
//! over MCP.
//!
//! This crate is a *consumer* of the embedded engine, kept strictly separate
//! from it: the engine has no networking, and all protocol/transport code
//! lives here. The MCP transport (JSON-RPC over stdio: `initialize`,
//! `tools/list`, `tools/call`) is a thin shell over [`Server::handle`], which
//! holds all the behavior and is fully testable on its own.

pub mod convert;
pub mod error;
pub mod server;

pub use error::ToolError;
pub use server::Server;
