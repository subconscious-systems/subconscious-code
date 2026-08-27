//! `RuntimePrompter` — the async [`rc_core::Prompter`] the loop awaits when a
//! tool call escalates to Ask. It registers a `oneshot`, broadcasts
//! `PermissionAsk`, and awaits the answer. A separate pump task fulfills the
//! oneshot from a `UserAction::PermissionAnswer` (or cancels it as `Deny` on
//! `Cancel`/`Quit`).
//!
//! At most one ask is ever pending: rc-core's `execute_batch` runs all
//! permission checks serially before spawning any tool, so an Ask and a tool
//! run never overlap.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rc_core::{AskResponse, Prompter};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::event::{AgentEvent, EventSender};

/// Shared table of pending asks: the prompter inserts, the pump resolves or
/// drains. Held by the prompter and the pump behind an `Arc`.
pub(crate) struct PendingAsks {
    map: Mutex<HashMap<u64, oneshot::Sender<AskResponse>>>,
    next_id: AtomicU64,
}

impl PendingAsks {
    pub(crate) fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Register a fresh ask; returns its id and the receiver to await.
    pub(crate) fn register(&self) -> (u64, oneshot::Receiver<AskResponse>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.map.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Fulfill a pending ask (no-op if it was already cancelled/dropped).
    pub(crate) fn resolve(&self, id: u64, response: AskResponse) {
        if let Some(tx) = self.map.lock().unwrap().remove(&id) {
            let _ = tx.send(response);
        }
    }

    /// Cancel every pending ask — each resolves as `Deny` so its prompter unblocks.
    pub(crate) fn drain_cancel(&self) {
        for (_, tx) in self.map.lock().unwrap().drain() {
            let _ = tx.send(AskResponse::Deny("cancelled by user".into()));
        }
    }
}

pub(crate) struct RuntimePrompter {
    events: EventSender,
    pending: Arc<PendingAsks>,
}

impl RuntimePrompter {
    pub(crate) fn new(events: EventSender, pending: Arc<PendingAsks>) -> Self {
        Self { events, pending }
    }
}

#[async_trait]
impl Prompter for RuntimePrompter {
    async fn ask(&self, tool: &str, input: &Value, reason: &str) -> AskResponse {
        let (id, rx) = self.pending.register();
        self.events.send(AgentEvent::PermissionAsk {
            id,
            tool: tool.to_string(),
            input: input.clone(),
            reason: reason.to_string(),
        });
        match rx.await {
            Ok(response) => {
                self.events.send(AgentEvent::PermissionDecision {
                    id,
                    response: response.clone(),
                });
                response
            }
            // The sender was dropped (shutdown) — deny so the loop winds down.
            Err(_) => {
                let denied = AskResponse::Deny("ask cancelled".into());
                self.events.send(AgentEvent::PermissionDecision {
                    id,
                    response: denied.clone(),
                });
                denied
            }
        }
    }
}
