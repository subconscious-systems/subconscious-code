//! The `Model` abstraction, `ChatModel` (wraps `rc_proto::ChatClient`), the
//! `EventSink` seam (M4 TUI), and tag-mode reasoning stripping (§3.4).
//!
//! `Model` is the seam that makes the loop testable with a `MockModel` and zero
//! network (§13). `ChatModel` streams internally, accumulates the assistant
//! turn, and forwards deltas to the sink.

use crate::turn::{ToolCall, ToolResultBody};
use async_trait::async_trait;
use rc_proto::stream::AgentStreamEvent;
use rc_proto::{
    ChatClient, CompleteOpts, FinishReason, ProtoError, ToolChoiceValue, ToolDefinition, Usage,
    WireMessage,
};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};

/// What the loop sends to a model.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<WireMessage>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoiceValue,
    pub opts: CompleteOpts,
}

/// A model's response: one assembled assistant turn.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub text: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<FinalizedToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub enum FinalizedToolCall {
    Call(ToolCall),
    /// The model emitted a tool call whose arguments could not be parsed; the
    /// loop synthesizes a `role:tool` error result so the model can retry (§3.3).
    ParseError {
        id: Option<String>,
        name: Option<String>,
        raw: String,
        error: String,
    },
}

#[derive(thiserror::Error, Debug)]
pub enum ModelError {
    #[error("proto: {0}")]
    Proto(#[from] rc_proto::ProtoError),
}

/// A sink for live agent events (the TUI seam, M4). M1 uses [`NullSink`].
///
/// Streaming deltas (`on_text`/`on_reasoning`/`on_tool_start`/`on_finish`) are
/// driven by [`ChatModel`] from the wire stream. The loop emits the per-turn
/// lifecycle (`on_iter`/`on_usage`/`on_tool_end`): `on_iter` at the top of each
/// iteration, `on_usage` once the model returns, and one `on_tool_end` for every
/// batch item — including denied / parse-error / unknown-tool results — so the
/// host observes a terminal state for every announced call. Turn-boundary
/// events (outcome, error, idle) are emitted by the runtime driver, not here.
pub trait EventSink: Send + Sync {
    fn on_text(&self, _delta: &str) {}
    fn on_reasoning(&self, _delta: &str) {}
    fn on_tool_start(&self, _call: &ToolCall) {}
    fn on_finish(&self, _reason: &FinishReason) {}
    /// One terminal result per batch item. `tool` is the call's tool name
    /// (carried so the host can render a result line without holding the call).
    fn on_tool_end(&self, _call_id: &str, _tool: &str, _result: &ToolResultBody) {}
    fn on_iter(&self, _count: u32, _max: u32) {}
    fn on_usage(&self, _usage: &Usage) {}
    /// M8: the size of the context about to be sent — its char length and the
    /// calibrated token estimate. Purely informational (there is no window to
    /// exceed); a UI shows it so the operator can watch the context grow.
    fn on_context(&self, _chars: usize, _est_tokens: usize) {}
}

#[derive(Default)]
pub struct NullSink;
impl EventSink for NullSink {}

/// The model abstraction (§13 MockModel for tests).
#[async_trait]
pub trait Model: Send + Sync {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &dyn EventSink,
    ) -> Result<ModelResponse, ModelError>;
}

/// A `Model` backed by an `rc_proto::ChatClient`. Streams internally, accumulates
/// the assistant turn, and forwards deltas to the sink.
pub struct ChatModel {
    client: std::sync::Arc<ChatClient>,
    /// Inline reasoning tag to split out of `text` (tag mode, §3.4). Default
    /// `think` (matches Qwen3's `<think>…</think>`); a no-op when absent.
    reasoning_tag: Option<String>,
}

impl ChatModel {
    pub fn new(client: std::sync::Arc<ChatClient>) -> Self {
        Self {
            client,
            reasoning_tag: Some("think".to_string()),
        }
    }
}

#[async_trait]
impl Model for ChatModel {
    async fn complete(
        &self,
        req: ModelRequest,
        sink: &dyn EventSink,
    ) -> Result<ModelResponse, ModelError> {
        let stream = self
            .client
            .stream(&req.messages, &req.opts, &req.tools)
            .await?;
        consume_stream(
            stream,
            req.opts.idle_timeout,
            self.reasoning_tag.as_deref(),
            sink,
        )
        .await
    }
}

/// Drive an [`AgentStreamEvent`] stream to completion: forward deltas to `sink`,
/// accumulate the assistant turn, split tag-mode reasoning, and bound the gap
/// between chunks by `idle` (T2). A stall — no chunk for `idle` — aborts with
/// [`ProtoError::Idle`] so the loop fails fast instead of hanging until the
/// total request timeout. Extracted from [`ChatModel::complete`] so the idle
/// logic is unit-testable with synthetic streams (no network).
async fn consume_stream(
    mut stream: Pin<Box<dyn Stream<Item = Result<AgentStreamEvent, ProtoError>> + Send>>,
    idle: Option<Duration>,
    reasoning_tag: Option<&str>,
    sink: &dyn EventSink,
) -> Result<ModelResponse, ModelError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = FinishReason::Stop;
    let mut usage = None;

    loop {
        // T2: bound the gap between chunks. `None` preserves the legacy
        // behavior (wait on the stream indefinitely, bounded only by the total
        // request timeout).
        let next = match idle {
            Some(d) => match tokio::time::timeout(d, stream.next()).await {
                Ok(inner) => inner,
                Err(_) => return Err(ModelError::Proto(ProtoError::Idle(d))),
            },
            None => stream.next().await,
        };
        let Some(ev) = next else {
            break;
        };
        match ev? {
            AgentStreamEvent::Text(t) => {
                sink.on_text(&t);
                text.push_str(&t);
            }
            AgentStreamEvent::Reasoning(r) => {
                sink.on_reasoning(&r);
                reasoning.push_str(&r);
            }
            AgentStreamEvent::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                // `arguments` arrives as an owned `String` from the stream parser;
                // wrap it once here so every later re-send of this call is a
                // refcount bump, not a copy.
                let call = ToolCall {
                    id,
                    name,
                    arguments: Arc::from(arguments),
                };
                sink.on_tool_start(&call);
                tool_calls.push(FinalizedToolCall::Call(call));
            }
            AgentStreamEvent::ToolCallFailed {
                id,
                name,
                raw_arguments,
                error,
                ..
            } => {
                tool_calls.push(FinalizedToolCall::ParseError {
                    id,
                    name,
                    raw: raw_arguments,
                    error,
                });
            }
            AgentStreamEvent::Finish { reason } => {
                sink.on_finish(&reason);
                finish_reason = reason;
            }
            AgentStreamEvent::Usage(u) => usage = Some(u),
        }
    }

    // Tag-mode reasoning (§3.4): split `<think>…</think>` out of text. No-op
    // when absent, so it's safe to always run.
    let field_reasoning = if reasoning.is_empty() {
        None
    } else {
        Some(reasoning)
    };
    let (text, tag_reasoning) = strip_reasoning_tag(&text, reasoning_tag);
    let reasoning = match (field_reasoning, tag_reasoning) {
        (Some(r), Some(t)) => Some(format!("{r}\n{t}")),
        (Some(r), None) => Some(r),
        (None, Some(t)) => Some(t),
        (None, None) => None,
    };

    tracing::debug!(
        "← model finish={:?} text_bytes={} reasoning_bytes={} tool_calls={} usage={:?}",
        finish_reason,
        text.len(),
        reasoning.as_ref().map_or(0, String::len),
        tool_calls.len(),
        usage,
    );

    Ok(ModelResponse {
        text,
        reasoning,
        tool_calls,
        finish_reason,
        usage,
    })
}

/// Split `<tag>…</tag>` out of `text`, returning (clean_text, reasoning).
fn strip_reasoning_tag(text: &str, tag: Option<&str>) -> (String, Option<String>) {
    let Some(tag) = tag else {
        return (text.to_string(), None);
    };
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if !text.contains(&open) {
        return (text.to_string(), None);
    }
    let mut clean = String::new();
    let mut reasoning = String::new();
    let mut rest = text;
    while let Some(s) = rest.find(&open) {
        clean.push_str(&rest[..s]);
        let after = &rest[s + open.len()..];
        if let Some(e) = after.find(&close) {
            reasoning.push_str(&after[..e]);
            rest = &after[e + close.len()..];
        } else {
            // Unterminated `<think>` with no closer: the rest is reasoning.
            reasoning.push_str(after);
            rest = "";
        }
    }
    clean.push_str(rest);
    (
        clean,
        if reasoning.trim().is_empty() {
            None
        } else {
            Some(reasoning)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_think_tags_from_text() {
        let (clean, r) = strip_reasoning_tag("hello <think>secret</think> world", Some("think"));
        assert_eq!(clean, "hello  world");
        assert_eq!(r.as_deref(), Some("secret"));
    }

    #[test]
    fn no_op_when_tag_absent() {
        let (clean, r) = strip_reasoning_tag("plain answer", Some("think"));
        assert_eq!(clean, "plain answer");
        assert!(r.is_none());
    }

    // ---- T2: idle timeout (consume_stream) -----------------------------------

    use rc_proto::{FinishReason, ProtoError};
    use std::time::{Duration, SystemTime};
    use tokio_stream::wrappers::ReceiverStream;

    fn boxed(
        evs: Vec<Result<AgentStreamEvent, ProtoError>>,
    ) -> Pin<Box<dyn Stream<Item = Result<AgentStreamEvent, ProtoError>> + Send>> {
        Box::pin(tokio_stream::iter(evs))
    }

    #[tokio::test]
    async fn idle_timeout_aborts_a_stalled_stream() {
        // One chunk, then the channel stays open but empty → the second
        // `stream.next()` never resolves, so the 50ms idle bound fires.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentStreamEvent, ProtoError>>(8);
        let stream: Pin<Box<dyn Stream<Item = Result<AgentStreamEvent, ProtoError>> + Send>> =
            Box::pin(ReceiverStream::new(rx));
        tx.send(Ok(AgentStreamEvent::Text("first".into())))
            .await
            .unwrap();

        let start = SystemTime::now();
        let res = consume_stream(stream, Some(Duration::from_millis(50)), None, &NullSink).await;
        let elapsed = start.elapsed().unwrap_or_default();
        drop(tx); // held open through the await above
        assert!(
            matches!(res, Err(ModelError::Proto(ProtoError::Idle(_)))),
            "stalled stream should hit Idle, got {res:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "should fail fast, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn idle_timeout_allows_a_normal_stream() {
        let stream = boxed(vec![
            Ok(AgentStreamEvent::Text("hi".into())),
            Ok(AgentStreamEvent::Finish {
                reason: FinishReason::Stop,
            }),
        ]);
        let res = consume_stream(stream, Some(Duration::from_millis(50)), None, &NullSink).await;
        let resp = res.expect("normal stream completes");
        assert_eq!(resp.text, "hi");
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }

    #[tokio::test]
    async fn no_idle_bound_completes_a_normal_stream() {
        let stream = boxed(vec![Ok(AgentStreamEvent::Text("ok".into()))]);
        let res = consume_stream(stream, None, None, &NullSink).await;
        assert_eq!(res.unwrap().text, "ok");
    }
}
