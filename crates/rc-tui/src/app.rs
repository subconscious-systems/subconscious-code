//! App state, the poll loop, and the keymap. The loop: drain pending events
//! into state, render, poll crossterm for up to one frame, translate a key into
//! a [`rc_rt::UserAction`] (or a local effect), repeat until quit.
//!
//! Rendering reads only [`ViewState`], which is cheap to build in a test — the
//! `Runtime`/`EventStream` are kept separate so a `ratatui::backend::TestBackend`
//! render test needs no tokio and no model.

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rc_core::{AgentMode, AskResponse};
use rc_rt::{AgentEvent, EventStream, Runtime, UserAction};
use serde_json::Value;

use crate::Term;
use crate::view::{self, PendingAsk, ViewState};

/// One frame budget (~30fps) — short enough to feel responsive, long enough to
/// not busy-spin between crossterm polls.
const FRAME: Duration = Duration::from_millis(33);

pub(crate) struct App {
    runtime: Runtime,
    stream: EventStream,
    view: ViewState,
    quit: bool,
}

impl App {
    pub(crate) fn new(runtime: Runtime, model_name: String) -> Self {
        let stream = runtime.subscribe();
        let mut view = ViewState::new(model_name);
        view.transcript
            .push(format!("rc | model: {} | Shift+Tab cycles mode | Ctrl+C quits", view.model_name));
        Self { runtime, stream, view, quit: false }
    }
}

/// Run the TUI main loop. Returns when the user quits.
pub(crate) fn run(terminal: &mut Term, runtime: Runtime, model_name: String) -> anyhow::Result<()> {
    let mut app = App::new(runtime, model_name);

    loop {
        app.drain_events();
        terminal.draw(|f| view::draw(f, &app.view))?;
        if app.quit {
            break;
        }
        if event::poll(FRAME)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key);
                }
            }
        }
    }
    Ok(())
}

impl App {
    /// Pull every available event off the stream and fold it into the view state.
    fn drain_events(&mut self) {
        while let Some(ev) = self.stream.try_next() {
            match ev {
                Ok(e) => self.apply(e),
                Err(n) => self
                    .view
                    .transcript
                    .push(format!("[stream lagged {n}; oldest deltas lost]")),
            }
        }
    }

    fn apply(&mut self, ev: AgentEvent) {
        let v = &mut self.view;
        match ev {
            AgentEvent::Text(t) => v.current_text.push_str(&t),
            AgentEvent::Reasoning(r) => v.current_text.push_str(&r), // M4a: inline, no styling
            AgentEvent::ToolStart { call } => {
                v.flush_text();
                v.transcript
                    .push(format!("-> {} {}", call.name, summarize_args(&call.arguments)));
            }
            AgentEvent::ToolEnd { tool, result, .. } => {
                v.flush_text();
                v.transcript.push(format!("<- {}: {}", tool, truncate(&result.render(), 200)));
            }
            AgentEvent::PermissionAsk { id, tool, input, reason } => {
                v.flush_text();
                v.pending_ask = Some(PendingAsk { id, tool, input, reason });
            }
            AgentEvent::PermissionDecision { .. } => v.pending_ask = None,
            AgentEvent::Iter { .. } => {} // M4a: not surfaced in the transcript.
            AgentEvent::Usage(u) => v.last_usage = Some(u),
            AgentEvent::Outcome(_) => {
                v.flush_text();
                v.busy = false;
            }
            AgentEvent::Error(e) => {
                v.flush_text();
                v.transcript.push(format!("! {e}"));
                v.busy = false;
            }
            AgentEvent::ModeChanged(m) => v.mode = m,
            AgentEvent::Ready => v.busy = true,
            AgentEvent::Idle => {
                v.busy = false;
                v.flush_text();
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits, even mid-ask / mid-turn.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.runtime.action(UserAction::Quit);
            self.quit = true;
            return;
        }

        // While an ask is open, only the answer keys are live; Enter is a no-op.
        if let Some(ask) = self.view.pending_ask.take() {
            let response = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(AskResponse::Once),
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    Some(AskResponse::Session(suggested_rule(&ask.tool, &ask.input)))
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Some(AskResponse::Always(suggested_rule(&ask.tool, &ask.input)))
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(AskResponse::Deny("declined".into()))
                }
                _ => None,
            };
            match response {
                Some(r) => self
                    .runtime
                    .action(UserAction::PermissionAnswer { id: ask.id, response: r }),
                None => self.view.pending_ask = Some(ask), // ignored key: keep the ask open
            }
            return;
        }

        match key.code {
            KeyCode::Enter => {
                if !self.view.composer.is_empty() {
                    let text = std::mem::take(&mut self.view.composer);
                    self.view.transcript.push(format!("> {text}"));
                    self.runtime.action(UserAction::Submit(text));
                }
            }
            KeyCode::Esc => {
                if self.view.busy {
                    self.runtime.action(UserAction::Cancel);
                } else {
                    self.runtime.action(UserAction::Quit);
                    self.quit = true;
                }
            }
            KeyCode::BackTab => {
                // Shift+Tab: cycle the permission mode
                // (Default -> AcceptEdits -> Plan -> BypassPermissions -> Default).
                let next = cycle_mode(self.view.mode);
                self.view.mode = next; // optimistic; ModeChanged confirms
                self.runtime.action(UserAction::SetMode(next));
            }
            KeyCode::Char(c) => self.view.composer.push(c),
            KeyCode::Backspace => {
                self.view.composer.pop();
            }
            _ => {}
        }
    }
}

fn cycle_mode(m: AgentMode) -> AgentMode {
    match m {
        AgentMode::Default => AgentMode::AcceptEdits,
        AgentMode::AcceptEdits => AgentMode::Plan,
        AgentMode::Plan => AgentMode::BypassPermissions,
        AgentMode::BypassPermissions => AgentMode::Default,
    }
}

/// A rough "don't ask again for this" rule, matching rc-cli's stdin prompter:
/// `Bash(<first-token>:*)` for Bash, the bare tool name otherwise.
fn suggested_rule(tool: &str, input: &Value) -> String {
    if tool == "Bash" {
        if let Some(cmd) = input.get("command").and_then(|val| val.as_str()) {
            let first = cmd.split_whitespace().next().unwrap_or("");
            if !first.is_empty() {
                return format!("Bash({first}:*)");
            }
        }
    }
    tool.to_string()
}

/// One-line summary of a tool call's arguments for the transcript.
fn summarize_args(args: &str) -> String {
    let v: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    if let Some(cmd) = v.get("command").and_then(|x| x.as_str()) {
        return truncate(cmd, 80);
    }
    if let Some(p) = v.get("file_path").or_else(|| v.get("path")).and_then(|x| x.as_str()) {
        return p.to_string();
    }
    if let Some(q) = v.get("pattern").and_then(|x| x.as_str()) {
        return format!("pattern={}", truncate(q, 60));
    }
    if let Some(m) = v.get("msg").and_then(|x| x.as_str()) {
        return m.to_string();
    }
    truncate(args, 80)
}

/// Char-safe truncation with an ellipsis.
pub(crate) fn truncate(s: &str, n: usize) -> String {
    let head: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{head}...")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_mode_rotates_through_all_four() {
        assert_eq!(cycle_mode(AgentMode::Default), AgentMode::AcceptEdits);
        assert_eq!(cycle_mode(AgentMode::AcceptEdits), AgentMode::Plan);
        assert_eq!(cycle_mode(AgentMode::Plan), AgentMode::BypassPermissions);
        assert_eq!(cycle_mode(AgentMode::BypassPermissions), AgentMode::Default);
    }

    #[test]
    fn suggested_rule_for_bash_uses_first_token() {
        assert_eq!(
            suggested_rule("Bash", &serde_json::json!({"command": "cargo test --lib"})),
            "Bash(cargo:*)"
        );
        assert_eq!(suggested_rule("Edit", &serde_json::json!({"file_path": "/tmp/x"})), "Edit");
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("héllo", 2), "hé..."); // 2 chars + ellipsis
    }
}
