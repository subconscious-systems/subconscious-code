//! An MCP server tool exposed to the model as an ordinary Marathon tool.

use crate::hub::ServerHandle;
use async_trait::async_trait;
use rc_core::tool::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use rmcp::model::CallToolRequestParams;
use rmcp::ServiceError;
use serde_json::Value;
use std::sync::Arc;

/// Providers cap function names at 64 characters of `[a-zA-Z0-9_-]`.
const MAX_WIRE_NAME: usize = 64;

/// `mcp__<server>__<tool>`, the convention Claude Code uses, so permission
/// rules written for it carry over. Characters a provider rejects become `_`;
/// an over-long name keeps a prefix plus a hash of the full name, so two long
/// tools on one server still get distinct names.
pub fn tool_wire_name(server: &str, tool: &str) -> String {
    let sanitized: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let full = format!("mcp__{server}__{sanitized}");
    if full.len() <= MAX_WIRE_NAME {
        return full;
    }
    let hash = blake3::hash(format!("{server}\0{tool}").as_bytes()).to_hex();
    let suffix = &hash.as_str()[..8];
    format!("{}_{suffix}", &full[..MAX_WIRE_NAME - suffix.len() - 1])
}

pub(crate) struct McpTool {
    wire_name: String,
    /// The server's own tool name, sent back on every call.
    remote_name: String,
    description: String,
    schema: Value,
    server: Arc<ServerHandle>,
}

impl McpTool {
    pub(crate) fn new(
        wire_name: String,
        tool: rmcp::model::Tool,
        server: Arc<ServerHandle>,
    ) -> Self {
        let description = tool
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .or(tool.title.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "The `{}` tool from MCP server `{}`.",
                    tool.name, server.name
                )
            });
        Self {
            wire_name,
            remote_name: tool.name.to_string(),
            description,
            schema: object_schema(Value::Object((*tool.input_schema).clone())),
            server,
        }
    }
}

/// Providers require a function's parameters to be an object schema with
/// `properties`; some servers omit one or both for no-argument tools.
fn object_schema(mut schema: Value) -> Value {
    if let Value::Object(map) = &mut schema {
        map.entry("type")
            .or_insert_with(|| Value::String("object".into()));
        map.entry("properties")
            .or_insert_with(|| Value::Object(Default::default()));
    }
    schema
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.wire_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    /// A remote tool's side effects are unknown, so MCP calls run one at a time
    /// in model order, like file writes.
    fn concurrency(&self) -> Concurrency {
        Concurrency::SerialWrite
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let arguments = match input {
            Value::Object(map) => map,
            Value::Null => Default::default(),
            other => {
                return Ok(ToolOutcome::error(format!(
                    "arguments must be a JSON object, got {other}"
                )))
            }
        };
        let Some(peer) = self.server.peer() else {
            self.server.mark_closed();
            return Ok(ToolOutcome::error(self.server.unavailable_reason()));
        };
        let request =
            CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments);
        let call = tokio::time::timeout(self.server.tool_timeout, peer.call_tool(request));
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(ToolOutcome::Interrupted),
            result = call => result,
        };
        match result {
            Err(_) => Ok(ToolOutcome::Error {
                message: format!(
                    "MCP tool `{}` on server `{}` did not answer within {} ms",
                    self.remote_name,
                    self.server.name,
                    self.server.tool_timeout.as_millis()
                ),
                retryable: true,
            }),
            Ok(Err(error)) => {
                let transport_gone = matches!(
                    error,
                    ServiceError::TransportClosed | ServiceError::TransportSend(_)
                );
                if transport_gone || self.server.peer().is_none() {
                    self.server.mark_closed();
                    return Ok(ToolOutcome::error(self.server.unavailable_reason()));
                }
                Ok(ToolOutcome::error(self.server.scrub(&format!(
                    "MCP tool `{}` on server `{}` failed: {error}",
                    self.remote_name, self.server.name
                ))))
            }
            Ok(Ok(result)) => {
                let text = render_result(&result);
                if result.is_error == Some(true) {
                    Ok(ToolOutcome::error(text))
                } else {
                    Ok(ToolOutcome::ok(text))
                }
            }
        }
    }
}

/// Flatten a tool result to the text the model sees. Text blocks pass through;
/// binary blocks become a one-line placeholder, since the chat wire format here
/// carries tool results as text only.
pub(crate) fn render_result(result: &rmcp::model::CallToolResult) -> String {
    let mut parts = Vec::new();
    for block in &result.content {
        let value = serde_json::to_value(block).unwrap_or(Value::Null);
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let part = match kind {
            "text" => value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            "image" | "audio" => format!(
                "[{kind} omitted: {}]",
                value
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type")
            ),
            "resource" => {
                let resource = value.get("resource").cloned().unwrap_or(Value::Null);
                match resource.get("text").and_then(Value::as_str) {
                    Some(text) => text.to_string(),
                    None => format!(
                        "[binary resource omitted: {}]",
                        resource.get("uri").and_then(Value::as_str).unwrap_or("")
                    ),
                }
            }
            "resource_link" => format!(
                "[resource: {}]",
                value.get("uri").and_then(Value::as_str).unwrap_or("")
            ),
            _ => value.to_string(),
        };
        parts.push(part);
    }
    if parts.is_empty() {
        if let Some(structured) = &result.structured_content {
            return structured.to_string();
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_follow_the_claude_code_convention() {
        assert_eq!(
            tool_wire_name("files", "read_file"),
            "mcp__files__read_file"
        );
        assert_eq!(
            tool_wire_name("web", "fetch.url v2"),
            "mcp__web__fetch_url_v2"
        );
    }

    #[test]
    fn long_wire_names_stay_unique_and_within_the_provider_limit() {
        let a = tool_wire_name("server", &"a".repeat(80));
        let b = tool_wire_name("server", &format!("{}b", "a".repeat(79)));
        assert_eq!(a.len(), MAX_WIRE_NAME);
        assert_eq!(b.len(), MAX_WIRE_NAME);
        assert_ne!(a, b);
        assert!(a.starts_with("mcp__server__"));
    }

    #[test]
    fn schemas_gain_the_object_type_and_properties() {
        let schema = object_schema(serde_json::json!({}));
        assert_eq!(
            schema,
            serde_json::json!({"type": "object", "properties": {}})
        );
        let kept = object_schema(
            serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        );
        assert_eq!(kept["properties"]["q"]["type"], "string");
    }

    #[test]
    fn results_render_to_text() {
        let result: rmcp::model::CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "AAAA", "mimeType": "image/png"},
                {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "body"}},
            ]
        }))
        .unwrap();
        assert_eq!(
            render_result(&result),
            "hello\n[image omitted: image/png]\nbody"
        );

        let structured: rmcp::model::CallToolResult = serde_json::from_value(serde_json::json!({
            "content": [], "structuredContent": {"n": 1}
        }))
        .unwrap();
        assert_eq!(render_result(&structured), r#"{"n":1}"#);
    }
}
