//! `RuntimeSink` — an [`rc_core::EventSink`] that forwards the loop's events to
//! the runtime's bounded event queue. Only the loop-driven surface flows here;
//! permission ask/decision and turn boundaries are emitted by the prompter and
//! the driver/pump (they don't pass through the sink).

use rc_core::{Artifact, EventSink, ToolCall, ToolResultBody, Turn, Usage};
use rc_session::SessionStore;
use std::sync::{Arc, Mutex};

use crate::event::{AgentEvent, EventSender};

pub(crate) struct RuntimeSink {
    events: EventSender,
    store: Option<SessionWriter>,
}

#[derive(Clone)]
pub(crate) struct SessionWriter {
    inner: Arc<SessionWriterInner>,
}

struct SessionWriterInner {
    sender: Mutex<Option<std::sync::mpsc::Sender<Turn>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SessionWriter {
    pub(crate) fn new(mut store: SessionStore) -> Self {
        // The dedicated writer owns every blocking filesystem operation. The
        // former bounded sync channel could park a Tokio worker when storage
        // fell behind, freezing unrelated model and tool work.
        let (sender, receiver) = std::sync::mpsc::channel::<Turn>();
        let thread = std::thread::spawn(move || {
            while let Ok(turn) = receiver.recv() {
                if let Err(error) = store.append_turn(&turn) {
                    tracing::warn!("session persist failed: {error}");
                }
            }
        });
        Self {
            inner: Arc::new(SessionWriterInner {
                sender: Mutex::new(Some(sender)),
                thread: Mutex::new(Some(thread)),
            }),
        }
    }

    pub(crate) fn append(&self, turn: &Turn) {
        let sender = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if sender
            .as_ref()
            .is_none_or(|sender| sender.send(turn.clone()).is_err())
        {
            tracing::warn!("session writer unavailable; completed turn was not persisted");
        }
    }
}

impl Drop for SessionWriterInner {
    fn drop(&mut self) {
        if let Ok(sender) = self.sender.get_mut() {
            sender.take();
        }
        if let Ok(thread) = self.thread.get_mut() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl RuntimeSink {
    pub(crate) fn new(events: EventSender, store: Option<SessionWriter>) -> Self {
        Self { events, store }
    }
}

impl EventSink for RuntimeSink {
    fn on_text(&self, delta: &str) {
        self.events.send(AgentEvent::Text(delta.to_string()));
    }
    fn on_reasoning(&self, delta: &str) {
        self.events.send(AgentEvent::Reasoning(delta.to_string()));
    }
    fn on_tool_start(&self, call: &ToolCall) {
        self.events
            .send(AgentEvent::ToolStart { call: call.clone() });
    }
    fn on_tool_end(&self, call_id: &str, tool: &str, result: &ToolResultBody) {
        self.events.send(AgentEvent::ToolEnd {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            result: result.clone(),
        });
    }
    fn on_artifact(&self, call_id: &str, tool: &str, artifact: &Artifact) {
        self.events.send(AgentEvent::Artifact {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            artifact: artifact.clone(),
        });
    }
    fn on_iter(&self, count: u32, max: u32) {
        self.events.send(AgentEvent::Iter { count, max });
    }
    fn on_retry(&self, retries: u32) {
        self.events.send(AgentEvent::Retry { retries });
    }
    fn on_usage(&self, usage: &Usage) {
        self.events.send(AgentEvent::Usage(usage.clone()));
    }
    fn on_context(&self, chars: usize, est_tokens: usize) {
        self.events.send(AgentEvent::Context { chars, est_tokens });
    }

    fn on_turn(&self, turn: &Turn) {
        if let Some(store) = &self.store {
            store.append(turn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_retries_reach_the_runtime_event_stream() {
        let (events, receiver) = crate::event::channel();
        let sink = RuntimeSink::new(events, None);

        sink.on_retry(2);

        assert!(matches!(
            receiver.try_recv(),
            Some(AgentEvent::Retry { retries: 2 })
        ));
    }
}
