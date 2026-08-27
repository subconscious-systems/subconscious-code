//! Stateful DLR request transport for chat completions.
//!
//! The model-facing API remains ordinary OpenAI-compatible JSON. Only the hop
//! from Subconscious Code to the colocated DLR sidecar is delta encoded.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use dlr_sidecar::{ChatSession, DlrChatClient, SidecarError};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::RequestPayloadStats;
use crate::error::ProtoError;
use crate::wire::{ToolCall, UserContent, WireMessage};

const CLIENT_HEADER: &str = "x-subconscious-client";
const CLIENT_NAME: &str = "subconscious_code";
const SESSION_HEADER: &str = "x-subconscious-code-session-id";

/// How failure of the sidecar capability probe is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlrMode {
    /// DLR must be available; never silently upload the full JSON request.
    Required,
    /// Fall back to JSON only if DLR is unavailable before a session starts.
    Prefer,
}

struct SessionState {
    chat: ChatSession,
    /// Last ACKed projection. Cloning a message retains its `Arc<str>` bodies,
    /// so pointer equality safely proves an unchanged large prefix without
    /// hashing or copying it again.
    messages: Vec<CachedMessage>,
    /// Sum of serialized message object sizes, excluding array punctuation.
    message_json_bytes: usize,
}

struct CachedMessage {
    message: WireMessage,
}

/// Shared, stateful transport owned by one [`crate::ChatClient`].
pub(crate) struct DlrTransport {
    client: DlrChatClient,
    probe: reqwest::Client,
    base_url: String,
    mode: DlrMode,
    repair_margin_pct: u32,
    // 0 unknown, 1 ready, 2 unavailable. Once ready, never fall back mid-run.
    capability: AtomicU8,
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl std::fmt::Debug for DlrTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DlrTransport")
            .field("base_url", &self.base_url)
            .field("mode", &self.mode)
            .field("repair_margin_pct", &self.repair_margin_pct)
            .finish_non_exhaustive()
    }
}

impl DlrTransport {
    pub(crate) fn new(
        base_url: String,
        ingress_token: Option<String>,
        mode: DlrMode,
        repair_margin_pct: u32,
    ) -> Result<Self, ProtoError> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let client = DlrChatClient::new(base_url.clone(), ingress_token)
            .map_err(|error| ProtoError::Dlr(error.to_string()))?;
        let probe = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .build()?;
        Ok(Self {
            client,
            probe,
            base_url,
            mode,
            repair_margin_pct,
            capability: AtomicU8::new(0),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// `Ok(None)` means Prefer mode may use the ordinary JSON path.
    pub(crate) async fn send(
        &self,
        session_id: Option<&str>,
        messages: &[WireMessage],
        metadata: Value,
        api_key: &str,
    ) -> Result<Option<(reqwest::Response, RequestPayloadStats)>, ProtoError> {
        let session_id = session_id.ok_or_else(|| {
            ProtoError::Dlr("a stable session_id is required for DLR transport".into())
        })?;
        validate_session_id(session_id)?;

        if !self.available().await? {
            return Ok(None);
        }

        // One session has one ordered request stream. Holding this lock through
        // response headers prevents two concurrent turns from sharing a base
        // root. The SSE response body itself is not held under the lock.
        let mut sessions = self.sessions.lock().await;
        let state = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionState {
                chat: ChatSession::new(session_id),
                messages: Vec::new(),
                message_json_bytes: 0,
            });

        let prefix_matches = state.messages.len() <= messages.len()
            && state
                .messages
                .iter()
                .zip(messages)
                .all(|(cached, message)| same_message(&cached.message, message));
        let append_start = if prefix_matches {
            state.messages.len()
        } else {
            // Compaction/reprojection can rewrite the effective transcript.
            // A fresh local view plus RESYNC atomically replaces the remote
            // manifest for the same stable session key.
            state.chat = ChatSession::new(session_id);
            0
        };
        // Only the actual append becomes an owned Value tree. A huge stable
        // prefix is hashed but never copied in steady state.
        let new_messages = messages[append_start..]
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;

        let prepared = state
            .chat
            .prepare_ref(&new_messages, &metadata)
            .map_err(dlr_error)?;
        let appended_json_bytes = prepared.new_messages_json_bytes;
        let message_json_bytes = if prefix_matches {
            state.message_json_bytes.saturating_add(appended_json_bytes)
        } else {
            appended_json_bytes
        };
        let messages_bytes = 2usize
            .saturating_add(message_json_bytes)
            .saturating_add(messages.len().saturating_sub(1));
        let full_json_bytes = prepared
            .request_json_bytes
            .saturating_add(12)
            .saturating_add(messages_bytes);
        let wire_bytes = prepared.body.len();
        let headers = request_headers(api_key, session_id)?;
        let mut response = self
            .client
            .send_chat(&mut state.chat, &prepared, headers.clone())
            .await
            .map_err(dlr_error)?;

        if response.status() == reqwest::StatusCode::CONFLICT {
            // The sidecar restarted, the client restarted, or a projection was
            // replaced. Synchronize the already-prepared complete local state,
            // then issue an empty APPEND to invoke the model exactly once.
            drop(response);
            self.client
                .synchronize(&mut state.chat, self.repair_margin_pct)
                .await
                .map_err(dlr_error)?;
            let invoke = state.chat.prepare_ref(&[], &metadata).map_err(dlr_error)?;
            response = self
                .client
                .send_chat(&mut state.chat, &invoke, headers)
                .await
                .map_err(dlr_error)?;
        }

        if state.chat.has_pending_request() {
            return Err(ProtoError::Dlr(
                "sidecar response did not contain a valid durable ACK".into(),
            ));
        }
        if prefix_matches {
            state.messages.extend(
                messages[append_start..]
                    .iter()
                    .map(|message| CachedMessage {
                        message: message.clone(),
                    }),
            );
        } else {
            state.messages = messages
                .iter()
                .map(|message| CachedMessage {
                    message: message.clone(),
                })
                .collect();
        }
        state.message_json_bytes = message_json_bytes;
        let stats = RequestPayloadStats {
            json_bytes: full_json_bytes,
            wire_bytes,
        };
        Ok(Some((response, stats)))
    }

    async fn available(&self) -> Result<bool, ProtoError> {
        match self.capability.load(Ordering::Acquire) {
            1 => return Ok(true),
            2 => return Ok(false),
            _ => {}
        }
        let result = self
            .probe
            .get(format!("{}/v1/dlr/capabilities", self.base_url))
            .send()
            .await;
        let ready = result
            .as_ref()
            .is_ok_and(|response| response.status().is_success());
        if ready {
            self.capability.store(1, Ordering::Release);
            return Ok(true);
        }
        if self.mode == DlrMode::Prefer {
            self.capability.store(2, Ordering::Release);
            tracing::warn!(sidecar = %self.base_url, "DLR unavailable; using JSON transport");
            return Ok(false);
        }
        let detail = result
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "capability endpoint returned a non-success status".into());
        Err(ProtoError::Dlr(format!(
            "sidecar capability probe failed at {}: {detail}",
            self.base_url
        )))
    }
}

fn request_headers(api_key: &str, session_id: &str) -> Result<HeaderMap, ProtoError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| ProtoError::Dlr("API key is not a valid HTTP header value".into()))?,
    );
    headers.insert(CLIENT_HEADER, HeaderValue::from_static(CLIENT_NAME));
    headers.insert(
        SESSION_HEADER,
        HeaderValue::from_str(session_id).map_err(|_| ProtoError::InvalidSessionId)?,
    );
    Ok(headers)
}

fn validate_session_id(id: &str) -> Result<(), ProtoError> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(ProtoError::InvalidSessionId);
    }
    Ok(())
}

fn dlr_error(error: SidecarError) -> ProtoError {
    ProtoError::Dlr(error.to_string())
}

fn same_message(left: &WireMessage, right: &WireMessage) -> bool {
    match (left, right) {
        (WireMessage::System { content: a }, WireMessage::System { content: b }) => same_arc(a, b),
        (
            WireMessage::User {
                content: UserContent::Text(a),
            },
            WireMessage::User {
                content: UserContent::Text(b),
            },
        ) => same_arc(a, b),
        (
            WireMessage::Assistant {
                content: ac,
                reasoning_content: ar,
                tool_calls: at,
            },
            WireMessage::Assistant {
                content: bc,
                reasoning_content: br,
                tool_calls: bt,
            },
        ) => same_optional_arc(ac, bc) && same_optional_arc(ar, br) && same_tool_calls(at, bt),
        (
            WireMessage::Tool {
                tool_call_id: ai,
                content: ac,
            },
            WireMessage::Tool {
                tool_call_id: bi,
                content: bc,
            },
        ) => ai == bi && same_arc(ac, bc),
        _ => false,
    }
}

fn same_arc(left: &std::sync::Arc<str>, right: &std::sync::Arc<str>) -> bool {
    std::sync::Arc::ptr_eq(left, right) || left == right
}

fn same_optional_arc(
    left: &Option<std::sync::Arc<str>>,
    right: &Option<std::sync::Arc<str>>,
) -> bool {
    match (left, right) {
        (Some(a), Some(b)) => same_arc(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn same_tool_calls(left: &[ToolCall], right: &[ToolCall]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.id == b.id
                && std::mem::discriminant(&a.kind) == std::mem::discriminant(&b.kind)
                && a.function.name == b.function.name
                && same_arc(&a.function.arguments, &b.function.arguments)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_arc_prefix_matches_without_changing_wire_identity() {
        let content = std::sync::Arc::<str>::from("large immutable content");
        let cached = WireMessage::User {
            content: UserContent::Text(content.clone()),
        };
        let projected_again = WireMessage::User {
            content: UserContent::Text(content),
        };
        assert!(same_message(&cached, &projected_again));
    }

    #[test]
    fn equal_reallocated_content_matches_but_mutation_does_not() {
        let cached = WireMessage::System {
            content: std::sync::Arc::from("same bytes"),
        };
        let reallocated = WireMessage::System {
            content: std::sync::Arc::from(String::from("same bytes")),
        };
        let changed = WireMessage::System {
            content: std::sync::Arc::from("different bytes"),
        };
        assert!(same_message(&cached, &reallocated));
        assert!(!same_message(&cached, &changed));
    }
}
