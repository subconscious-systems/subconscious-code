//! `AgentEvent` — the observation stream a host (TUI or test harness) reads
//! from the runtime over a lossless single-consumer queue.
//!
//! Streaming deltas and tool lifecycle are forwarded from the loop by
//! [`crate::sink::RuntimeSink`]; permission ask/decision come from the async
//! prompter ([`crate::prompter`]); turn boundaries (`Ready`/`Idle`/`Outcome`/
//! `Error`) and `ModeChanged` come from the driver/pump. Nothing here is
//! emitted by rc-core directly — the runtime is the only producer.

use rc_core::{AgentMode, Artifact, AskResponse, LoopOutcome, ToolCall, ToolResultBody, Usage};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const EVENT_QUEUE_CAP: usize = 1_024;
const STREAM_CHUNK_CAP: usize = 512 * 1024;

/// One observable agent event.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Assistant text delta (streaming).
    Text(String),
    /// Reasoning delta (streaming).
    Reasoning(String),
    /// A tool call is about to run (announced by the model stream).
    ToolStart { call: ToolCall },
    /// A tool call finished — one per batch item, including denied /
    /// parse-error / unknown-tool results, so the host always observes a
    /// terminal state for every announced call.
    ToolEnd {
        call_id: String,
        tool: String,
        result: ToolResultBody,
    },
    /// A successful tool mutation. Emitted before its `ToolEnd`, allowing the
    /// host to show the actual diff while the turn is still running.
    Artifact {
        call_id: String,
        tool: String,
        artifact: Artifact,
    },
    /// A tool call escalated to Ask; the async prompter is awaiting an answer
    /// (a `UserAction::PermissionAnswer` with this `id`).
    PermissionAsk {
        id: u64,
        tool: String,
        input: Value,
        reason: String,
    },
    /// The prompter got an answer (or the ask was cancelled → `Deny`).
    PermissionDecision { id: u64, response: AskResponse },
    /// Top of a loop iteration.
    Iter { count: u32, max: u32 },
    /// A model request succeeded only after one or more wire-layer retries.
    /// Emitted before the response body is consumed so hosts can make a slow
    /// 429/5xx/connection recovery visible instead of presenting it as opaque
    /// model thinking time.
    Retry { retries: u32 },
    /// Token usage from the model response.
    Usage(Usage),
    /// M8: size of the context about to be sent — char length and the
    /// calibrated token estimate. Informational: there is no window to exceed,
    /// but the operator wants to watch it grow.
    Context { chars: usize, est_tokens: usize },
    /// A turn completed normally.
    Outcome(LoopOutcome),
    /// A turn failed (model error).
    Error(String),
    /// The active mode changed (Shift+Tab). Emitted as soon as the engine's
    /// mode is swapped — the next permission check sees it.
    ModeChanged(AgentMode),
    /// A turn is starting (driver).
    Ready,
    /// A turn finished (driver) — the runtime is ready for the next submit.
    Idle,
    /// A host-side notice from the driver (e.g. `/rewind` outcome). Rendered as
    /// a system line, never injected into the model's prompt.
    Notice(String),
}

/// A bounded, single-consumer event queue. Streaming deltas are coalesced at
/// production time, so an SSE provider emitting one token per chunk does not
/// allocate one queue node per token. If a completely stalled UI fills the
/// queue, presentation-only events are evicted before turn boundaries and
/// permission decisions; the next `Idle` carries an explicit overflow notice.
pub(crate) struct EventSender {
    inner: Arc<EventQueue>,
}

pub(crate) struct EventReceiver {
    inner: Arc<EventQueue>,
}

struct EventQueue {
    state: Mutex<EventQueueState>,
    notify: Notify,
}

struct EventQueueState {
    events: VecDeque<AgentEvent>,
    compactable: usize,
    dropped: usize,
    senders: usize,
    closed: bool,
    receiver_open: bool,
}

pub(crate) fn channel() -> (EventSender, EventReceiver) {
    let inner = Arc::new(EventQueue {
        state: Mutex::new(EventQueueState {
            events: VecDeque::with_capacity(EVENT_QUEUE_CAP),
            compactable: 0,
            dropped: 0,
            senders: 1,
            closed: false,
            receiver_open: true,
        }),
        notify: Notify::new(),
    });
    (
        EventSender {
            inner: inner.clone(),
        },
        EventReceiver { inner },
    )
}

impl EventSender {
    pub(crate) fn send(&self, event: AgentEvent) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || !state.receiver_open {
            return;
        }
        if merge_stream_delta(state.events.back_mut(), &event) {
            drop(state);
            self.inner.notify.notify_one();
            return;
        }

        if matches!(event, AgentEvent::Idle) && state.dropped > 0 {
            let dropped = std::mem::take(&mut state.dropped);
            state.events.push_back(AgentEvent::Notice(format!(
                "UI event queue saturated; {dropped} presentation event(s) were compacted"
            )));
        }
        if is_compactable(&event) {
            make_compactable_room(&mut state);
            state.compactable += 1;
        }
        state.events.push_back(event);
        drop(state);
        self.inner.notify.notify_one();
    }
}

impl Clone for EventSender {
    fn clone(&self) -> Self {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.senders = state.senders.saturating_add(1);
        drop(state);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for EventSender {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.senders = state.senders.saturating_sub(1);
        if state.senders == 0 {
            state.closed = true;
            drop(state);
            self.inner.notify.notify_waiters();
        }
    }
}

impl EventReceiver {
    pub(crate) fn try_recv(&self) -> Option<AgentEvent> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let event = state.events.pop_front();
        if event.as_ref().is_some_and(is_compactable) {
            state.compactable = state.compactable.saturating_sub(1);
        }
        event
    }

    pub(crate) async fn recv(&mut self) -> Option<AgentEvent> {
        loop {
            let notified = self.inner.notify.notified();
            if let Some(event) = self.try_recv() {
                return Some(event);
            }
            if self
                .inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
            {
                return None;
            }
            notified.await;
        }
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.receiver_open = false;
        state.events.clear();
        state.compactable = 0;
    }
}

fn merge_stream_delta(last: Option<&mut AgentEvent>, next: &AgentEvent) -> bool {
    match (last, next) {
        (Some(AgentEvent::Text(current)), AgentEvent::Text(delta))
        | (Some(AgentEvent::Reasoning(current)), AgentEvent::Reasoning(delta))
            if current.len().saturating_add(delta.len()) <= STREAM_CHUNK_CAP =>
        {
            current.push_str(delta);
            true
        }
        _ => false,
    }
}

fn make_compactable_room(state: &mut EventQueueState) {
    if state.compactable < EVENT_QUEUE_CAP {
        return;
    }
    if let Some(index) = state.events.iter().position(is_compactable) {
        state.events.remove(index);
        state.compactable = state.compactable.saturating_sub(1);
        state.dropped = state.dropped.saturating_add(1);
    }
}

/// Events that can be compacted without destroying a lifecycle invariant.
/// Tool start/end/artifact, permission, turn-boundary, error, mode, and notice
/// events are structural and are never discarded. Keeping a separate budget
/// for these presentation events preserves global event order without letting
/// token-level deltas grow without bound.
fn is_compactable(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Text(_)
            | AgentEvent::Reasoning(_)
            | AgentEvent::Iter { .. }
            | AgentEvent::Retry { .. }
            | AgentEvent::Usage(_)
            | AgentEvent::Context { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_never_discards_structural_events() {
        let (tx, rx) = channel();
        tx.send(AgentEvent::ToolStart {
            call: ToolCall {
                id: "call".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            },
        });
        for index in 0..EVENT_QUEUE_CAP + 20 {
            tx.send(AgentEvent::Iter {
                count: index as u32,
                max: u32::MAX,
            });
        }
        tx.send(AgentEvent::ToolEnd {
            call_id: "call".into(),
            tool: "Read".into(),
            result: ToolResultBody::Ok {
                content: "done".into(),
                truncated: false,
            },
        });

        let events = std::iter::from_fn(|| rx.try_recv()).collect::<Vec<_>>();
        assert!(matches!(events.first(), Some(AgentEvent::ToolStart { .. })));
        assert!(matches!(events.last(), Some(AgentEvent::ToolEnd { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::Iter { .. }))
                .count(),
            EVENT_QUEUE_CAP
        );
    }

    #[tokio::test]
    async fn receiver_observes_explicit_sender_closure() {
        let (tx, mut rx) = channel();
        let clone = tx.clone();
        drop(tx);
        clone.send(AgentEvent::Ready);
        drop(clone);
        assert!(matches!(rx.recv().await, Some(AgentEvent::Ready)));
        assert!(rx.recv().await.is_none());
    }
}
