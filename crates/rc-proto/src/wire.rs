//! Request/response wire types for `/v1/chat/completions`.
//!
//! See §3.1 for the Messages↔ChatCompletions delta. The traps handled here:
//!   - Assistant `content` is `Option<String>`, omitted when `None` (some
//!     providers reject `""`, others reject `null`; §3.1 trap 1).
//!   - `role: "tool"` content is a plain string, not a structured block
//!     (§3.1 trap 2). Multimodal tool results are faked via a follow-up user
//!     message (§3.5) — wired in M1.
//!   - Conversation order is rigid: every `tool_call.id` in an Assistant
//!     message must be answered by exactly one *contiguous* Tool message
//!     before the next message (§3.1 trap 3). `rc-core::project()` asserts this;
//!     any early exit from a turn must synthesize results for outstanding
//!     calls.

use serde::{Deserialize, Serialize};

/// A single message in the conversation — the wire projection of a `Turn`.
///
/// Field order in this enum is not significant to serialization (`serde` tags
/// on `role`), but the *conversation order* is rigid (see module docs).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    System {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// User message content: a plain string today, an array of parts once vision
/// is wired (§3.5). `#[serde(untagged)]` with a single variant serializes as a
/// bare string; adding `Parts` later keeps the wire shape stable.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    // Parts(Vec<ContentPart>), // M1+ — image_url blocks for vision.
}

impl From<String> for UserContent {
    fn from(s: String) -> Self {
        UserContent::Text(s)
    }
}

impl From<&str> for UserContent {
    fn from(s: &str) -> Self {
        UserContent::Text(s.to_string())
    }
}

/// A tool call issued by the assistant. Defined now, exercised in M1.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ToolCallType,
    pub function: FunctionCall,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallType {
    #[default]
    Function,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Chat completions request. `stream: false` for the non-streaming path,
/// `true` for streaming (M1 adds `tools`, `tool_choice`, `parallel_tool_calls`,
/// `stream_options`). Serialized canonically (§4.6).
#[derive(Serialize, Debug, Clone)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

/// A tool definition in the request's `tools` array (§3.2). Names must match
/// `^[a-zA-Z0-9_-]{1,64}$`; MCP tools are namespaced `mcp__server__tool`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub ty: ToolType,
    pub function: FunctionDefinition,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    #[default]
    Function,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    /// A JSON Schema object. Generated once per tool and reused; canonical
    /// serialization (§4.6) makes the on-wire bytes stable across turns.
    pub parameters: serde_json::Value,
}

/// `tool_choice`. M1 supports the string forms; the `{type:"function",...}`
/// pin form is P2.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceValue {
    Auto,
    None,
    Required,
}

#[derive(Serialize, Debug, Clone)]
pub struct StreamOptions {
    /// Request a final chunk carrying `usage` (§3.6). Some backends emit it
    /// with an empty `choices` array — don't assume `choices[0]` exists.
    pub include_usage: bool,
}

/// Non-streaming response. Streaming deltas land in M1 (`rc-proto::stream`).
#[derive(Deserialize, Debug, Clone)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Choice {
    pub index: u32,
    pub message: ResponseMessage,
    pub finish_reason: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ResponseMessage {
    pub role: String,
    pub content: Option<String>,
    /// Present when the assistant requested tool calls. M1 wires the loop.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// Token usage. `cached_tokens` (§3.6, O6) is the cache-hit feedback loop —
/// surface it in the status line; it's the only signal on whether the harness
/// is preserving its prefix.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

impl Usage {
    /// Cached (hit) prompt tokens, if the backend reports any.
    pub fn cached_tokens(&self) -> Option<u64> {
        self.prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .filter(|c| *c > 0)
    }
}
