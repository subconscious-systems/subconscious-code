//! `Runtime` — the handle a host (TUI or test) holds. Spawns the driver and pump
//! tasks on the current tokio runtime and exposes a bounded, coalescing event
//! handle plus a bounded sync `action` sender.
//!
//! Must be constructed inside a tokio runtime context — it `tokio::spawn`s the
//! driver and pump. A non-async caller panics at `spawn`.

use rc_core::{AgentLoop, EventSink, Session};
use rc_session::SessionStore;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::action::UserAction;
use crate::driver::{driver_task, DriverCmd, DriverTask};
use crate::event::{self, AgentEvent, EventReceiver, EventSender};
use crate::prompter::{PendingAsks, RuntimePrompter};
use crate::pump::pump_task;
use crate::sink::{RuntimeSink, SessionWriter};

pub struct Runtime {
    events_rx: std::sync::Mutex<Option<EventReceiver>>,
    events_tx: EventSender,
    actions_tx: mpsc::Sender<UserAction>,
    driver: Option<JoinHandle<()>>,
    pump: Option<JoinHandle<()>>,
}

/// Cloneable control plane for host watchdogs. It deliberately exposes no
/// session or task ownership; a resource monitor can report pressure and ask
/// for graceful cancellation without being able to bypass runtime ordering.
#[derive(Clone)]
pub struct RuntimeControl {
    actions: mpsc::Sender<UserAction>,
    events: EventSender,
}

impl RuntimeControl {
    pub fn action(&self, action: UserAction) {
        let _ = self.actions.try_send(action);
    }

    pub fn notice(&self, message: impl Into<String>) {
        self.events.send(AgentEvent::Notice(message.into()));
    }
}

impl Runtime {
    /// Build the runtime and spawn its driver + pump on the current tokio
    /// runtime. Panics outside a runtime context.
    ///
    /// `store` is an optional session-persistence handle; when `Some`, the
    /// driver appends each completed turn to it (crash recovery, §9). `None`
    /// for headless/ephemeral runs.
    pub fn new(
        agent: std::sync::Arc<AgentLoop>,
        session: Session,
        store: Option<SessionStore>,
    ) -> Self {
        // There is exactly one host consumer. Streaming deltas are coalesced
        // into a bounded queue; terminal turn events are retained preferentially
        // when a completely stalled UI forces compaction.
        let (events_tx, events_rx) = event::channel();
        let (actions_tx, actions_rx) = mpsc::channel::<UserAction>(64);
        let (driver_tx, driver_rx) = mpsc::channel::<DriverCmd>(16);
        let (feedback_tx, feedback_rx) = mpsc::channel(4);

        let pending = std::sync::Arc::new(PendingAsks::new());
        let store = store.map(SessionWriter::new);
        let sink = std::sync::Arc::new(RuntimeSink::new(events_tx.clone(), store.clone()))
            as std::sync::Arc<dyn EventSink>;
        let prompter = RuntimePrompter::new(events_tx.clone(), pending.clone());
        let permission = agent.permission.clone();

        let driver = tokio::spawn(driver_task(
            DriverTask {
                agent,
                session,
                sink,
                prompter,
                events: events_tx.clone(),
                store,
                feedback: feedback_tx,
            },
            driver_rx,
        ));
        let pump = tokio::spawn(pump_task(
            actions_rx,
            driver_tx,
            events_tx.clone(),
            permission,
            pending,
            feedback_rx,
        ));

        Self {
            events_rx: std::sync::Mutex::new(Some(events_rx)),
            events_tx,
            actions_tx,
            driver: Some(driver),
            pump: Some(pump),
        }
    }

    /// Subscribe to the agent event stream. Call once; drain with sync
    /// [`EventStream::try_next`] (TUI) or async [`EventStream::recv`] (tests).
    pub fn subscribe(&self) -> EventStream {
        let rx = self
            .events_rx
            .lock()
            .expect("runtime event receiver lock poisoned")
            .take()
            .expect("Runtime::subscribe may only be called once");
        EventStream { rx }
    }

    pub fn control(&self) -> RuntimeControl {
        RuntimeControl {
            actions: self.actions_tx.clone(),
            events: self.events_tx.clone(),
        }
    }

    /// Push a user action (sync — safe from any thread/task).
    pub fn action(&self, action: UserAction) {
        self.try_action(action);
    }

    /// Try to push a user action, returning whether the runtime accepted it.
    /// Interactive hosts use this when they must only mutate local UI state
    /// after the matching action is safely in the runtime queue.
    pub fn try_action(&self, action: UserAction) -> bool {
        if self.actions_tx.try_send(action).is_err() {
            self.events_tx.send(AgentEvent::Notice(
                "runtime action queue is full; input was not accepted".into(),
            ));
            false
        } else {
            true
        }
    }

    /// Stop the runtime: dropping `actions_tx` makes the pump exit, which drops
    /// `driver_tx`, which makes the driver exit once its current turn finishes.
    pub async fn shutdown(mut self) {
        let _ = self.actions_tx.send(UserAction::Quit).await;
        self.join_until(Duration::from_secs(5)).await;
    }

    /// Graceful shutdown for the synchronous terminal host. The TUI itself
    /// runs on a blocking thread, so it cannot await Tokio joins directly. It
    /// still gives cancellation, driver cleanup, session flushes, and child
    /// reaping a bounded grace period before aborting a wedged task.
    pub fn shutdown_blocking(&mut self, timeout: Duration) {
        let _ = self.actions_tx.blocking_send(UserAction::Quit);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline
            && self
                .pump
                .iter()
                .chain(self.driver.iter())
                .any(|task| !task.is_finished())
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        finish_or_abort(&mut self.pump);
        finish_or_abort(&mut self.driver);
    }

    async fn join_until(&mut self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        join_task_until(&mut self.pump, deadline).await;
        join_task_until(&mut self.driver, deadline).await;
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.actions_tx.try_send(UserAction::Quit);
        // Explicit shutdown is the normal path. Drop is only an emergency
        // fallback, where aborting is preferable to silently detaching tasks
        // that retain a session and its background processes indefinitely.
        if let Some(task) = self.pump.take() {
            task.abort();
        }
        if let Some(task) = self.driver.take() {
            task.abort();
        }
    }
}

async fn join_task_until(task: &mut Option<JoinHandle<()>>, deadline: tokio::time::Instant) {
    let Some(mut handle) = task.take() else {
        return;
    };
    match tokio::time::timeout_at(deadline, &mut handle).await {
        Ok(_) => {}
        Err(_) => {
            handle.abort();
            let _ = handle.await;
        }
    }
}

fn finish_or_abort(task: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = task.take() {
        if !handle.is_finished() {
            handle.abort();
        }
    }
}

/// A handle onto the agent event stream. Wraps the single-consumer receiver so
/// hosts that don't run a tokio runtime (the TUI) can drain it synchronously via
/// [`EventStream::try_next`] without depending on tokio directly; async hosts
/// (tests) use [`EventStream::recv`].
pub struct EventStream {
    rx: EventReceiver,
}

impl EventStream {
    /// Non-blocking receive: the next event, or `None` when empty/closed. The
    /// `Result` wrapper remains API-compatible with the old broadcast stream;
    /// queue compaction is reported as an `AgentEvent::Notice`, not an error.
    pub fn try_next(&mut self) -> Option<Result<AgentEvent, u64>> {
        self.rx.try_recv().map(Ok)
    }

    /// Blocking async receive (for hosts running a tokio runtime).
    pub async fn recv(&mut self) -> Option<Result<AgentEvent, u64>> {
        self.rx.recv().await.map(Ok)
    }
}
