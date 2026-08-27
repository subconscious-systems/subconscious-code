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
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::action::UserAction;
use crate::driver::{DriverCmd, DriverFeedback};
use crate::event::{AgentEvent, EventSender};
use crate::prompter::PendingAsks;

pub(crate) async fn pump_task(
    mut actions: mpsc::Receiver<UserAction>,
    driver_tx: mpsc::Sender<DriverCmd>,
    events: EventSender,
    permission: std::sync::Arc<dyn PermissionChecker>,
    pending: std::sync::Arc<PendingAsks>,
    mut feedback: mpsc::Receiver<DriverFeedback>,
) {
    let mut active: Option<(u64, CancellationToken)> = None;
    let mut next_turn_id = 0u64;
    loop {
        let action = tokio::select! {
            biased;
            feedback = feedback.recv(), if active.is_some() => {
                if let Some(DriverFeedback::TurnFinished { turn_id }) = feedback {
                    if active.as_ref().is_some_and(|(active_id, _)| *active_id == turn_id) {
                        active = None;
                    }
                }
                continue;
            }
            action = actions.recv() => action,
        };
        let Some(action) = action else { break };
        match action {
            UserAction::Submit(prompt) => {
                if active.is_some() {
                    events.send(AgentEvent::Notice(
                        "a turn is already running; the duplicate submission was ignored".into(),
                    ));
                    continue;
                }
                next_turn_id = next_turn_id.wrapping_add(1);
                let token = CancellationToken::new();
                active = Some((next_turn_id, token.clone()));
                if driver_tx
                    .send(DriverCmd::Run {
                        turn_id: next_turn_id,
                        prompt,
                        cancel: token,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            UserAction::Cancel => {
                if let Some((_, token)) = &active {
                    token.cancel();
                }
                pending.drain_cancel();
            }
            UserAction::SetMode(mode) => {
                permission.set_mode(mode.into());
                events.send(AgentEvent::ModeChanged(mode));
                if driver_tx.send(DriverCmd::SetMode(mode)).await.is_err() {
                    break;
                }
            }
            UserAction::PermissionAnswer { id, response } => {
                pending.resolve(id, response);
            }
            UserAction::Rewind { steps } => {
                if active.is_none() {
                    if driver_tx.send(DriverCmd::Rewind { steps }).await.is_err() {
                        break;
                    }
                } else {
                    events.send(AgentEvent::Notice(
                        "finish or cancel the active turn before rewinding".into(),
                    ));
                }
            }
            UserAction::Compact => {
                if active.is_none() {
                    if driver_tx.send(DriverCmd::Compact).await.is_err() {
                        break;
                    }
                } else {
                    events.send(AgentEvent::Notice(
                        "finish or cancel the active turn before compacting".into(),
                    ));
                }
            }
            UserAction::SetGoal(goal) => {
                if active.is_none() {
                    if driver_tx.send(DriverCmd::SetGoal(goal)).await.is_err() {
                        break;
                    }
                } else {
                    events.send(AgentEvent::Notice(
                        "finish or cancel the active turn before changing its goal".into(),
                    ));
                }
            }
            UserAction::ShowGoal => {
                if driver_tx.send(DriverCmd::ShowGoal).await.is_err() {
                    break;
                }
            }
            UserAction::Quit => {
                if let Some((_, token)) = active.take() {
                    token.cancel();
                }
                pending.drain_cancel();
                break;
            }
        }
    }
}
