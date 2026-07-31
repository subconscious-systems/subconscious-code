//! `RuntimeSink` — an [`rc_core::EventSink`] that forwards the loop's events to
//! the runtime's `broadcast` channel. Only the loop-driven surface flows here;
//! permission ask/decision and turn boundaries are emitted by the prompter and
//! the driver/pump (they don't pass through the sink).

use rc_core::{EventSink, ToolCall, ToolResultBody, Usage};
use tokio::sync::broadcast;

use crate::event::AgentEvent;

pub(crate) struct RuntimeSink {
    events: broadcast::Sender<AgentEvent>,
}

impl RuntimeSink {
    pub(crate) fn new(events: broadcast::Sender<AgentEvent>) -> Self {
        Self { events }
    }
}

impl EventSink for RuntimeSink {
    fn on_text(&self, delta: &str) {
        let _ = self.events.send(AgentEvent::Text(delta.to_string()));
    }
    fn on_reasoning(&self, delta: &str) {
        let _ = self.events.send(AgentEvent::Reasoning(delta.to_string()));
    }
    fn on_tool_start(&self, call: &ToolCall) {
        let _ = self.events.send(AgentEvent::ToolStart { call: call.clone() });
    }
    fn on_tool_end(&self, call_id: &str, tool: &str, result: &ToolResultBody) {
        let _ = self.events.send(AgentEvent::ToolEnd {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            result: result.clone(),
        });
    }
    fn on_iter(&self, count: u32, max: u32) {
        let _ = self.events.send(AgentEvent::Iter { count, max });
    }
    fn on_usage(&self, usage: &Usage) {
        let _ = self.events.send(AgentEvent::Usage(usage.clone()));
    }
    fn on_context(&self, chars: usize, est_tokens: usize) {
        let _ = self.events.send(AgentEvent::Context { chars, est_tokens });
    }
}
