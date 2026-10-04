//! `RuntimeSink` — an [`rc_core::EventSink`] that forwards the loop's events to
//! the runtime's bounded event queue. Only the loop-driven surface flows here;
//! permission ask/decision and turn boundaries are emitted by the prompter and
//! the driver/pump (they don't pass through the sink).

use rc_core::{Artifact, EventSink, ToolCall, ToolResultBody, Turn, Usage};
use rc_session::SessionStore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex};

use crate::event::{AgentEvent, EventSender};

/// How many completed turns may wait for the writer thread. A whole model
/// turn is only a handful of records, so this tolerates a slow disk without
/// letting a stalled writer accumulate unbounded memory. When the queue is
/// full the turn is dropped **with a visible event** — blocking the caller
/// instead would park a Tokio worker and freeze unrelated model/tool work.
const TURN_QUEUE_CAP: usize = 128;

pub(crate) struct RuntimeSink {
    events: EventSender,
    store: Option<SessionWriter>,
}

#[derive(Clone)]
pub(crate) struct SessionWriter {
    inner: Arc<SessionWriterInner>,
}

struct SessionWriterInner {
    events: EventSender,
    sender: Mutex<Option<std::sync::mpsc::SyncSender<Turn>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set while the queue is saturated so one Notice covers a whole burst
    /// of dropped turns instead of one event per turn.
    overflow_notified: AtomicBool,
}

impl SessionWriter {
    pub(crate) fn new(events: EventSender, mut store: SessionStore) -> Self {
        // The dedicated writer owns every blocking filesystem operation. The
        // channel is bounded: a dead-stalled disk fills it at TURN_QUEUE_CAP
        // instead of buffering unboundedly, and the producer then drops turns
        // with a visible Notice rather than blocking (parking a Tokio worker
        // froze unrelated model and tool work back when the channel parked).
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Turn>(TURN_QUEUE_CAP);
        let events_for_thread = events.clone();
        let thread = std::thread::spawn(move || {
            let mut reported = false;
            while let Ok(turn) = receiver.recv() {
                if let Err(error) = store.append_turn(&turn) {
                    // The store poisons itself on failure, so this surfaces
                    // once; later queued turns error out silently after it.
                    if !reported {
                        reported = true;
                        events_for_thread.send(AgentEvent::Error(format!(
                            "session persistence failed: {error}; no further turns will be written to this session file"
                        )));
                    }
                    tracing::warn!("session persist failed: {error}");
                }
            }
        });
        Self {
            inner: Arc::new(SessionWriterInner {
                events,
                sender: Mutex::new(Some(sender)),
                thread: Mutex::new(Some(thread)),
                overflow_notified: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn append(&self, turn: &Turn) {
        let sender = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(sender) = sender.as_ref() else {
            tracing::warn!("session writer unavailable; completed turn was not persisted");
            return;
        };
        match sender.try_send(turn.clone()) {
            Ok(()) => {
                self.inner
                    .overflow_notified
                    .store(false, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                if !self
                    .inner
                    .overflow_notified
                    .swap(true, Ordering::Relaxed)
                {
                    self.inner.events.send(AgentEvent::Notice(
                        "session persistence is running behind; some completed turns were not persisted".into(),
                    ));
                }
                tracing::warn!("session writer queue full; completed turn was not persisted");
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("session writer unavailable; completed turn was not persisted");
            }
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
    use rc_core::Turn;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    /// Drain the event queue via `try_recv` until `pred` matches or `secs`
    /// elapse. Panics on timeout.
    fn expect_event(
        receiver: &crate::event::EventReceiver,
        secs: u64,
        pred: impl Fn(&AgentEvent) -> bool,
    ) -> AgentEvent {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let mut found = None;
            while let Some(event) = receiver.try_recv() {
                if pred(&event) {
                    found = Some(event);
                }
            }
            if let Some(event) = found {
                return event;
            }
            assert!(Instant::now() < deadline, "timed out waiting for the event");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn drain(receiver: &crate::event::EventReceiver) -> Vec<AgentEvent> {
        std::iter::from_fn(|| receiver.try_recv()).collect()
    }

    fn user_turn(label: &str) -> Turn {
        Turn::User {
            content: format!("<{label}>").into(),
            ts: UNIX_EPOCH,
        }
    }

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

    /// A disk that accepts the line itself but fails on the newline: a
    /// partial line sits there with no way to terminate it.
    struct PartialLineWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        writes_left: usize,
    }

    impl std::io::Write for PartialLineWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.writes_left == 0 {
                return Err(std::io::Error::other("disk gone"));
            }
            self.writes_left -= 1;
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_append_failure_surfaces_an_error_event_and_never_glues_new_lines() {
        let (events, receiver) = crate::event::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let store = rc_session::SessionStore::from_writer(
            PathBuf::from("poisoned.jsonl"),
            PartialLineWriter {
                bytes: bytes.clone(),
                writes_left: 1, // the JSON makes it out; the newline doesn't
            },
        );
        let writer = SessionWriter::new(events, store);

        writer.append(&user_turn("one"));
        let error = expect_event(&receiver, 5, |event| {
            matches!(event, AgentEvent::Error(_))
        });
        let AgentEvent::Error(text) = error else {
            unreachable!("checked the variant above");
        };
        assert!(
            text.contains("session persistence failed"),
            "the failure must be visible to the host: {text}"
        );

        // The store is poisoned; further turns are refused rather than
        // written, so nothing gets concatenated onto the corrupt tail.
        writer.append(&user_turn("two"));
        drop(writer); // joins the writer thread: drains and exits

        let written = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(written.contains("<one>"), "the first turn was attempted");
        assert!(
            !written.contains("<two>"),
            "no later turn was glued onto the corrupt tail"
        );
        let errors = drain(&receiver)
            .into_iter()
            .filter(|event| matches!(event, AgentEvent::Error(_)))
            .count();
        // Exactly one error per failure episode, never one per queued turn.
        assert_eq!(errors, 0, "the initial error is the only one: {written}");
    }

    /// A consumer that signals when it starts writing and then parks until
    /// released — the "slow consumer" the bounded queue has to survive.
    struct GatedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        entered: std::sync::mpsc::Sender<()>,
        gate: Option<std::sync::mpsc::Receiver<()>>,
    }

    impl std::io::Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(gate) = self.gate.take() {
                let _ = self.entered.send(()); // consumer is now writing
                let _ = gate.recv(); // … and now it stalls
            }
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stalled_writer_drops_turns_visibly_instead_of_blocking_forever() {
        let (events, receiver) = crate::event::channel();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (entered, consumer_started) = std::sync::mpsc::channel::<()>();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let store = rc_session::SessionStore::from_writer(
            PathBuf::from("slow.jsonl"),
            GatedWriter {
                bytes: bytes.clone(),
                entered,
                gate: Some(gate),
            },
        );
        let writer = SessionWriter::new(events, store);

        // Park the consumer on the very first turn, then fill the queue: the
        // consumer holds turn 0, and TURN_QUEUE_CAP more fit in the buffer.
        writer.append(&user_turn("0"));
        consumer_started
            .recv_timeout(Duration::from_secs(5))
            .expect("the writer thread started consuming");
        for i in 1..=TURN_QUEUE_CAP {
            writer.append(&user_turn(&i.to_string()));
        }

        // Queue full: further turns are dropped, visibly, once per burst.
        let overflow_a = (TURN_QUEUE_CAP + 1).to_string();
        let overflow_b = (TURN_QUEUE_CAP + 2).to_string();
        writer.append(&user_turn(&overflow_a));
        writer.append(&user_turn(&overflow_b));
        let notices = drain(&receiver)
            .into_iter()
            .filter(|event| matches!(event, AgentEvent::Notice(_)))
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 1, "one overflow notice covers the burst");
        assert!(
            matches!(&notices[0], AgentEvent::Notice(text) if text.contains("running behind")),
            "the host is told turns were dropped: {:?}",
            notices[0]
        );

        // Release: the writer drains every accepted turn and none of the
        // dropped ones (no turn is silently lost, none is duplicated).
        release.send(()).expect("the gated consumer is still parked");
        drop(writer); // takes the sender and joins the writer thread

        let written = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(written.contains(&format!("<{}>", TURN_QUEUE_CAP)));
        assert!(
            !written.contains(&format!("<{overflow_a}>")),
            "the first overflow turn was dropped"
        );
        assert!(
            !written.contains(&format!("<{overflow_b}>")),
            "the second overflow turn was dropped"
        );
        assert_eq!(
            written.matches('\n').count(),
            TURN_QUEUE_CAP + 1, // turns 0..=TURN_QUEUE_CAP were accepted
            "each accepted turn is one complete line"
        );
        // Nothing further arrived after the burst notice was observed.
        assert!(drain(&receiver).is_empty());
    }
}
