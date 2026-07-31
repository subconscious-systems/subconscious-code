//! `Runtime` — the handle a host (TUI or test) holds. Spawns the driver and pump
//! tasks on the current tokio runtime and exposes a `broadcast` subscribe handle
//! plus a sync `action` sender.
//!
//! Must be constructed inside a tokio runtime context — it `tokio::spawn`s the
//! driver and pump. A non-async caller panics at `spawn`.

use rc_core::{AgentLoop, EventSink, Session};
use rc_session::SessionStore;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::action::UserAction;
use crate::driver::{driver_task, DriverCmd};
use crate::event::AgentEvent;
use crate::prompter::{PendingAsks, RuntimePrompter};
use crate::pump::pump_task;
use crate::sink::RuntimeSink;

/// Broadcast capacity. The single consumer drains every frame; on `Lagged(n)`
/// it logs and continues — high-level state (`Outcome`/`Idle`/`ModeChanged`) is
/// never lost, only the oldest `n` deltas.
const EVENT_CAPACITY: usize = 256;

pub struct Runtime {
    events_tx: broadcast::Sender<AgentEvent>,
    actions_tx: mpsc::UnboundedSender<UserAction>,
    _driver: JoinHandle<()>,
    _pump: JoinHandle<()>,
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
        let (events_tx, _) = broadcast::channel::<AgentEvent>(EVENT_CAPACITY);
        let (actions_tx, actions_rx) = mpsc::unbounded_channel::<UserAction>();
        let (driver_tx, driver_rx) = mpsc::unbounded_channel::<DriverCmd>();

        let pending = std::sync::Arc::new(PendingAsks::new());
        let sink =
            std::sync::Arc::new(RuntimeSink::new(events_tx.clone())) as std::sync::Arc<dyn EventSink>;
        let prompter = RuntimePrompter::new(events_tx.clone(), pending.clone());
        let permission = agent.permission.clone();

        let driver = tokio::spawn(driver_task(
            agent,
            session,
            driver_rx,
            sink,
            prompter,
            events_tx.clone(),
            store,
        ));
        let pump = tokio::spawn(pump_task(
            actions_rx,
            driver_tx,
            events_tx.clone(),
            permission,
            pending,
        ));

        Self { events_tx, actions_tx, _driver: driver, _pump: pump }
    }

    /// Subscribe to the agent event stream. Call once; drain with sync
    /// [`EventStream::try_next`] (TUI) or async [`EventStream::recv`] (tests).
    pub fn subscribe(&self) -> EventStream {
        EventStream { rx: self.events_tx.subscribe() }
    }

    /// Push a user action (sync — safe from any thread/task).
    pub fn action(&self, action: UserAction) {
        let _ = self.actions_tx.send(action);
    }

    /// Stop the runtime: dropping `actions_tx` makes the pump exit, which drops
    /// `driver_tx`, which makes the driver exit once its current turn finishes.
    pub fn shutdown(self) {
        drop(self.actions_tx);
    }
}

/// A handle onto the agent event stream. Wraps the `broadcast` receiver so hosts
/// that don't run a tokio runtime (the TUI) can drain it synchronously via
/// [`EventStream::try_next`] without depending on tokio directly; async hosts
/// (tests) use [`EventStream::recv`].
pub struct EventStream {
    rx: broadcast::Receiver<AgentEvent>,
}

impl EventStream {
    /// Non-blocking receive: the next event, or `None` if the stream closed.
    /// `Err(n)` means the host lagged `n` events (log and continue — high-level
    /// state like `Outcome`/`Idle`/`ModeChanged` is never lost).
    pub fn try_next(&mut self) -> Option<Result<AgentEvent, u64>> {
        match self.rx.try_recv() {
            Ok(ev) => Some(Ok(ev)),
            Err(broadcast::error::TryRecvError::Lagged(n)) => Some(Err(n)),
            // Empty (nothing right now) and Closed (runtime gone) both read as
            // "no event this tick" — the TUI polls again or stops on its own Quit.
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => None,
        }
    }

    /// Blocking async receive (for hosts running a tokio runtime).
    pub async fn recv(&mut self) -> Option<Result<AgentEvent, u64>> {
        match self.rx.recv().await {
            Ok(ev) => Some(Ok(ev)),
            Err(broadcast::error::RecvError::Lagged(n)) => Some(Err(n)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}
