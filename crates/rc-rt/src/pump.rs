//! The action pump: drains `UserAction`s from the host and dispatches them.
//!
//! - `Submit` starts a turn with a fresh cancel token the pump owns as a local
//!   (one task → no shared-slot race with a new turn).
//! - `Cancel` fires the token and denies any pending ask so the prompter
//!   unblocks and the turn winds down.
//! - `SetMode` swaps the engine mode atomically (immediate), tells the host via
//!   `ModeChanged`, and tells the driver to persist `session.mode` at the next
//!   idle.
//! - `PermissionAnswer` fulfills the pending ask's oneshot.
//! - `Quit` cancels any in-flight turn and exits (dropping `driver_tx` lets the
//!   driver exit once its current turn finishes).

use rc_core::PermissionChecker;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::action::UserAction;
use crate::driver::DriverCmd;
use crate::event::AgentEvent;
use crate::prompter::PendingAsks;

pub(crate) async fn pump_task(
    mut actions: mpsc::UnboundedReceiver<UserAction>,
    driver_tx: mpsc::UnboundedSender<DriverCmd>,
    events: broadcast::Sender<AgentEvent>,
    permission: std::sync::Arc<dyn PermissionChecker>,
    pending: std::sync::Arc<PendingAsks>,
) {
    let mut current_cancel: Option<CancellationToken> = None;
    while let Some(action) = actions.recv().await {
        match action {
            UserAction::Submit(prompt) => {
                let token = CancellationToken::new();
                current_cancel = Some(token.clone());
                let _ = driver_tx.send(DriverCmd::Run { prompt, cancel: token });
            }
            UserAction::Cancel => {
                if let Some(token) = current_cancel.take() {
                    token.cancel();
                }
                pending.drain_cancel();
            }
            UserAction::SetMode(mode) => {
                permission.set_mode(mode.into());
                let _ = events.send(AgentEvent::ModeChanged(mode));
                let _ = driver_tx.send(DriverCmd::SetMode(mode));
            }
            UserAction::PermissionAnswer { id, response } => {
                pending.resolve(id, response);
            }
            UserAction::Rewind { steps } => {
                let _ = driver_tx.send(DriverCmd::Rewind { steps });
            }
            UserAction::Quit => {
                if let Some(token) = current_cancel.take() {
                    token.cancel();
                }
                pending.drain_cancel();
                break;
            }
        }
    }
}
