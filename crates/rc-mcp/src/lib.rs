//! rc-mcp: MCP client — stdio/http/sse (§11.5).
//!
//! Not yet implemented — lands in M9. Uses `rmcp`. Connect all servers before
//! the first model request (don't mutate the tool array mid-turn); namespace
//! tools `mcp__server__tool`; per-server timeout + circuit breaker so a hung
//! server never hangs the agent.
