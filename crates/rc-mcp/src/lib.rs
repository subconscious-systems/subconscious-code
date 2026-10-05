//! rc-mcp: the MCP client (§11.5).
//!
//! Servers come from the `mcpServers` settings block and `--mcp-config` files
//! ([`config`]). [`McpHub::connect`] starts every server concurrently before
//! the first model request, so the tool set never changes mid-session and the
//! request prefix stays cacheable. Each server tool becomes an ordinary
//! [`rc_core::tool::Tool`] named `mcp__<server>__<tool>`, so it goes through
//! the same permission checks, concurrency rules and result caps as built-in
//! tools. A server that fails to start or later exits never fails the session:
//! its tools answer with the error, and `/mcp` shows why.

pub mod config;
mod hub;
mod process;
mod tool;

pub use config::{env_lookup, parse_servers, redact, ServerConfig, Transport};
pub use hub::{McpHub, ServerState, ServerStatus, Skipped};
pub use tool::tool_wire_name;
