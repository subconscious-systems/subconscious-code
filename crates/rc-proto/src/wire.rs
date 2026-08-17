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
use std::sync::Arc;

/// Deserialize a nullable sequence as its `Default` when the JSON value is
/// `null`. `#[serde(default)]` alone only covers the *absent*-field case; many
/// OpenAI-compatible gateways send `"tool_calls": null` in text-only responses
/// and streaming deltas, which otherwise fails with
/// "invalid type: null, expected a sequence". Use it with `#[serde(default)]`
/// so both absent and null collapse to the empty default.
pub fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A single message in the conversation — the wire projection of a `Turn`.
///
/// Field order in this enum is not significant to serialization (`serde` tags
/// on `role`), but the *conversation order* is rigid (see module docs).
///
/// Content fields are `Arc<str>` so projecting a `Turn` into a `WireMessage`
/// (per request) is a refcount bump, not a deep copy of (potentially) many
/// megabytes of tool-result or expanded-`@file` content. `Arc<str>` serializes
/// identically to `String`.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    System {
        content: Arc<str>,
    },
    User {
        content: UserContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Arc<str>>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: Arc<str>,
    },
}

/// User message content: a plain string today, an array of parts once vision
/// is wired (§3.5). `#[serde(untagged)]` with a single variant serializes as a
/// bare string; adding `Parts` later keeps the wire shape stable. A large user
/// message that would exceed the gateway's per-message byte limit is split
/// into multiple `role:user` messages in the projection (see `rc_core::project`),
/// not into an array here — the limit is per *message*, so a parts array does
/// not help.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum UserContent {
    Text(Arc<str>),
    // Parts(Vec<ContentPart>), // M1+ — image_url blocks for vision.
}

impl From<String> for UserContent {
    fn from(s: String) -> Self {
        UserContent::Text(Arc::from(s))
    }
}

impl From<Arc<str>> for UserContent {
    fn from(s: Arc<str>) -> Self {
        UserContent::Text(s)
    }
}

impl From<&str> for UserContent {
    fn from(s: &str) -> Self {
        UserContent::Text(Arc::from(s))
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
    pub arguments: Arc<str>,
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
    #[serde(default, deserialize_with = "null_to_default")]
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

    /// Fraction of prompt tokens served from the provider's prompt cache.
    /// `None` means the backend did not report prompt-token details (or did
    /// not report a usable prompt total); a reported zero is a real `0%` hit.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let details = self.prompt_tokens_details.as_ref()?;
        if self.prompt_tokens == 0 {
            return None;
        }
        Some((details.cached_tokens.min(self.prompt_tokens) as f64) / self.prompt_tokens as f64)
    }

    /// Accumulate `other` into `self` (saturating). For a session running total:
    /// each turn's prompt re-sends the prefix, so the summed `total_tokens` is an
    /// upper bound, not the marginal cost; `completion_tokens` is the true
    /// cumulative output, and `cached_tokens` sums cache-hit counts.
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        let cached = self
            .cached_tokens()
            .unwrap_or(0)
            .saturating_add(other.cached_tokens().unwrap_or(0));
        if cached > 0 {
            self.prompt_tokens_details = Some(PromptTokensDetails {
                cached_tokens: cached,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_sums_fields_and_cached() {
        let mut a = Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 4 }),
        };
        let b = Usage {
            prompt_tokens: 20,
            completion_tokens: 3,
            total_tokens: 23,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 6 }),
        };
        a.add(&b);
        assert_eq!(a.prompt_tokens, 30);
        assert_eq!(a.completion_tokens, 5);
        assert_eq!(a.total_tokens, 35);
        assert_eq!(a.cached_tokens(), Some(10), "cached summed");
    }

    #[test]
    fn add_with_no_cached_leaves_details_none() {
        let mut a = Usage::default();
        let b = Usage {
            prompt_tokens: 5,
            completion_tokens: 1,
            total_tokens: 6,
            prompt_tokens_details: None,
        };
        a.add(&b);
        assert_eq!(a.total_tokens, 6);
        assert!(
            a.prompt_tokens_details.is_none(),
            "no cached -> details stay None"
        );
    }

    #[test]
    fn cache_hit_rate_uses_returned_prompt_tokens() {
        let usage = Usage {
            prompt_tokens: 80,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 20 }),
            ..Usage::default()
        };
        assert_eq!(usage.cache_hit_rate(), Some(0.25));

        let miss = Usage {
            prompt_tokens: 80,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 0 }),
            ..Usage::default()
        };
        assert_eq!(miss.cache_hit_rate(), Some(0.0));
        assert_eq!(Usage::default().cache_hit_rate(), None);
    }

    /// GLM-class gateways send `"tool_calls": null` in text-only responses.
    /// `#[serde(default)]` only covers the *absent* field; an explicit null must
    /// collapse to an empty vec, not fail with "invalid type: null, expected a
    /// sequence" (observed against the real gateway via `sc --doctor`).
    #[test]
    fn response_message_tolerates_null_tool_calls() {
        let with_null: ResponseMessage =
            serde_json::from_str(r#"{"role":"assistant","content":"hi","tool_calls":null}"#)
                .expect("null tool_calls must deserialize to empty");
        assert!(with_null.tool_calls.is_empty(), "null -> empty");

        let without: ResponseMessage =
            serde_json::from_str(r#"{"role":"assistant","content":"hi"}"#)
                .expect("absent tool_calls must deserialize");
        assert!(without.tool_calls.is_empty(), "absent -> empty");

        let with_calls: ResponseMessage = serde_json::from_str(
            r#"{"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            with_calls.tool_calls.len(),
            1,
            "a real tool call still parses"
        );
    }
}
