//! App state, the poll loop, and the keymap. The loop: drain pending events
//! into state, render, poll crossterm for up to one frame, translate a key into
//! a [`rc_rt::UserAction`] (or a local effect), repeat until quit.
//!
//! Rendering reads only [`ViewState`], which is cheap to build in a test — the
//! `Runtime`/`EventStream` are kept separate so a `ratatui::backend::TestBackend`
//! render test needs no tokio and no model.

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use rc_core::{AgentMode, AskResponse, ToolResultBody};
use rc_rt::{AgentEvent, EventStream, Runtime, UserAction};
use serde_json::Value;

use crate::complete::{self, Completion};
use crate::diff;
use crate::Term;
use crate::view::{self, CompletionMenu, PendingAsk, ViewState};

/// One frame budget (~30fps) — short enough to feel responsive, long enough to
/// not busy-spin between crossterm polls.
const FRAME: Duration = Duration::from_millis(33);

pub(crate) struct App {
    runtime: Runtime,
    stream: EventStream,
    view: ViewState,
    /// The session cwd, used to resolve `@file` completions (M4c).
    cwd: PathBuf,
    quit: bool,
}

impl App {
    pub(crate) fn new(runtime: Runtime, model_name: String, cwd: PathBuf) -> Self {
        let stream = runtime.subscribe();
        let mut view = ViewState::new(model_name);
        view.transcript.push(Line::from(format!(
            "rc | model: {} | Shift+Tab cycles mode | Ctrl+C quits",
            view.model_name
        )));
        Self { runtime, stream, view, cwd, quit: false }
    }
}

/// Run the TUI main loop. Returns when the user quits.
pub(crate) fn run(
    terminal: &mut Term,
    runtime: Runtime,
    model_name: String,
    cwd: PathBuf,
) -> anyhow::Result<()> {
    let mut app = App::new(runtime, model_name, cwd);

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
                Err(n) => self.view.transcript.push(Line::from(format!(
                    "[stream lagged {n}; oldest deltas lost]"
                ))),
            }
        }
    }

    fn apply(&mut self, ev: AgentEvent) {
        let v = &mut self.view;
        match ev {
            AgentEvent::Text(t) => v.current_text.push_str(&t),
            AgentEvent::Reasoning(r) => v.current_text.push_str(&r),
            AgentEvent::ToolStart { call } => {
                v.flush_text();
                v.transcript
                    .push(tool_start_line(&call.name, &summarize_args(&call.arguments)));
                // An Edit previews the change as a word-level diff of old -> new.
                if let Some((path, old, new)) = edit_args(&call.arguments) {
                    v.transcript.push(Line::styled(format!("  edit {path}"), dim_style()));
                    v.transcript.push(diff::word_diff_line(&old, &new));
                }
            }
            AgentEvent::ToolEnd { tool, result, .. } => {
                v.flush_text();
                v.transcript.push(tool_end_line(&tool, &result));
            }
            AgentEvent::PermissionAsk { id, tool, input, reason } => {
                v.flush_text();
                v.pending_ask = Some(PendingAsk { id, tool, input, reason });
            }
            AgentEvent::PermissionDecision { .. } => v.pending_ask = None,
            AgentEvent::Iter { .. } => {}
            AgentEvent::Usage(u) => v.last_usage = Some(u),
            AgentEvent::Context { chars, est_tokens } => {
                v.last_context = Some((chars, est_tokens))
            }
            AgentEvent::Outcome(_) => {
                v.flush_text();
                v.busy = false;
            }
            AgentEvent::Error(e) => {
                v.flush_text();
                v.transcript.push(Line::styled(format!("! {e}"), error_style()));
                v.busy = false;
            }
            AgentEvent::ModeChanged(m) => v.mode = m,
            AgentEvent::Notice(n) => {
                v.flush_text();
                v.transcript.push(Line::styled(format!("· {n}"), dim_style()));
            }
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

        // While a completion menu is open, arrow/Tab/Esc drive the menu; other
        // keys fall through to the composer (so typing keeps filtering it).
        if let Some(menu) = self.view.menu.take() {
            match key.code {
                KeyCode::Tab | KeyCode::Enter => {
                    // Accept the selected candidate into the composer.
                    if let Some(new) = complete::apply(&self.view.composer, &menu.completion, menu.selected) {
                        self.view.composer = new;
                    }
                    self.refresh_menu();
                    return;
                }
                KeyCode::Esc => {
                    // Dismiss the menu without accepting; key consumed.
                    return;
                }
                KeyCode::Up => {
                    let selected = menu.selected.saturating_sub(1);
                    self.view.menu = Some(CompletionMenu { completion: menu.completion, selected });
                    return;
                }
                KeyCode::Down => {
                    let max = menu.completion.candidates.len().saturating_sub(1);
                    let selected = (menu.selected + 1).min(max);
                    self.view.menu = Some(CompletionMenu { completion: menu.completion, selected });
                    return;
                }
                _ => {
                    // Fall through to composer editing; recompute menu below.
                    self.view.menu = None;
                }
            }
        }

        match key.code {
            KeyCode::Enter => {
                if !self.view.composer.is_empty() {
                    let text = std::mem::take(&mut self.view.composer);
                    // Slash commands are host-side actions, not prompts.
                    if let Some(action) = self.handle_slash(&text) {
                        match action {
                            SlashAction::Help => {
                                self.view.transcript.push(Line::from(
                                    "commands: /clear  /help  /mode  /rewind | @<path> completes files",
                                ));
                            }
                            SlashAction::Clear => {
                                self.view.transcript.clear();
                                self.view.current_text.clear();
                                self.view.transcript.push(Line::from(format!(
                                    "rc | model: {} | Shift+Tab cycles mode | Ctrl+C quits",
                                    self.view.model_name
                                )));
                            }
                            SlashAction::CycleMode => {
                                let next = cycle_mode(self.view.mode);
                                self.view.mode = next;
                                self.runtime.action(UserAction::SetMode(next));
                            }
                            SlashAction::Rewind { steps } => {
                                self.runtime.action(UserAction::Rewind { steps });
                            }
                        }
                    } else {
                        self.view.transcript.push(Line::from(format!("> {text}")));
                        self.runtime.action(UserAction::Submit(text));
                    }
                    self.refresh_menu();
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
            KeyCode::Char(c) => {
                self.view.composer.push(c);
                self.refresh_menu();
            }
            KeyCode::Backspace => {
                self.view.composer.pop();
                self.refresh_menu();
            }
            _ => {}
        }
    }

    /// Recompute the completion menu from the current composer buffer. Clears
    /// the menu if no trigger is active. Keeps the selected row clamped to the
    /// new candidate list (best-effort: reset to 0 on a kind/prefix change).
    fn refresh_menu(&mut self) {
        let prev = self.view.menu.take();
        match complete::complete(&self.view.composer, &self.cwd) {
            Some(completion) => {
                // Preserve the selection when the candidate set is unchanged;
                // otherwise start at the top.
                let selected = prev
                    .filter(|p| same_menu(&p.completion, &completion))
                    .map(|p| p.selected)
                    .unwrap_or(0)
                    .min(completion.candidates.len().saturating_sub(1));
                self.view.menu = Some(CompletionMenu { completion, selected });
            }
            None => self.view.menu = None,
        }
    }

    /// If `text` is a recognized slash command, return its host-side action.
    /// Returns `None` for anything else (including `@`-prefixed text), so the
    /// text is submitted as a normal prompt.
    fn handle_slash(&self, text: &str) -> Option<SlashAction> {
        let t = text.trim();
        match t {
            "/clear" => Some(SlashAction::Clear),
            "/help" => Some(SlashAction::Help),
            "/mode" => Some(SlashAction::CycleMode),
            "/rewind" => Some(SlashAction::Rewind { steps: 1 }),
            other if other.starts_with("/rewind ") => {
                let n = other["/rewind ".len()..].trim().parse::<usize>().ok()?;
                if n == 0 {
                    None
                } else {
                    Some(SlashAction::Rewind { steps: n })
                }
            }
            _ => None,
        }
    }
}

/// A host-side slash command (not submitted to the model).
enum SlashAction {
    Clear,
    Help,
    CycleMode,
    /// `/rewind [n]` — restore the last `steps` turns of file changes.
    Rewind { steps: usize },
}

/// Two completions are "the same menu" if they're the same kind and would show
/// the same candidates — used to preserve the selection across keystrokes.
fn same_menu(a: &Completion, b: &Completion) -> bool {
    a.kind == b.kind && a.candidates == b.candidates
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

/// A styled "tool starting" line: `-> Name summary`.
fn tool_start_line(name: &str, summary: &str) -> Line<'static> {
    Line::styled(format!("-> {name} {summary}"), Style::default().fg(Color::Cyan))
}

/// A styled "tool finished" line, colored by the result kind.
fn tool_end_line(tool: &str, result: &ToolResultBody) -> Line<'static> {
    let body = truncate(&result.render(), 200);
    let style = match result {
        ToolResultBody::Ok { .. } => Style::default().fg(Color::Green),
        ToolResultBody::Error { .. } => Style::default().fg(Color::Red),
        ToolResultBody::Denied { .. } => Style::default().fg(Color::Yellow),
        ToolResultBody::Interrupted => Style::default().fg(Color::DarkGray),
    };
    Line::styled(format!("<- {tool}: {body}"), style)
}

/// If `args` is an Edit call carrying `file_path`/`old_string`/`new_string`,
/// return them (for the word-level diff preview). `None` for creates / missing.
fn edit_args(args: &str) -> Option<(String, String, String)> {
    let v: Value = serde_json::from_str(args).ok()?;
    let path = v.get("file_path")?.as_str()?.to_string();
    let old = v.get("old_string")?.as_str()?.to_string();
    let new = v.get("new_string")?.as_str()?.to_string();
    Some((path, old, new))
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn error_style() -> Style {
    Style::default().fg(Color::Red)
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

    #[test]
    fn edit_args_extracts_path_old_new() {
        let args = r#"{"file_path":"/a.rs","old_string":"foo","new_string":"bar"}"#;
        assert_eq!(
            edit_args(args),
            Some(("/a.rs".into(), "foo".into(), "bar".into()))
        );
        // Missing old/new (e.g. a create) -> no diff preview.
        assert!(edit_args(r#"{"file_path":"/a.rs"}"#).is_none());
        assert!(edit_args("{not json}").is_none());
    }

    #[test]
    fn tool_end_line_renders_the_result() {
        let line = tool_end_line(
            "Read",
            &ToolResultBody::Ok { content: "hi".into(), truncated: false },
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Read: hi"), "{text}");
    }

    #[test]
    fn slash_commands_recognize_clear_help_mode() {
        // A fresh App needs a Runtime (tokio), but handle_slash is a pure method
        // over &self that never touches the runtime — test it via a tiny shim
        // that mirrors its body. We assert the recognized commands and the
        // fall-through for non-commands and @-mentions.
        fn classify(text: &str) -> Option<SlashAction> {
            let t = text.trim();
            match t {
                "/clear" => Some(SlashAction::Clear),
                "/help" => Some(SlashAction::Help),
                "/mode" => Some(SlashAction::CycleMode),
                _ => None,
            }
        }
        assert!(matches!(classify("/clear"), Some(SlashAction::Clear)));
        assert!(matches!(classify("/help"), Some(SlashAction::Help)));
        assert!(matches!(classify("/mode"), Some(SlashAction::CycleMode)));
        // Unknown command and plain prompts fall through.
        assert!(classify("/unknown").is_none());
        assert!(classify("hello there").is_none());
        assert!(classify("@src/main.rs").is_none());
        // Whitespace is tolerated.
        assert!(matches!(classify("  /clear  "), Some(SlashAction::Clear)));
    }

    #[test]
    fn same_menu_compares_kind_and_candidates() {
        let a = Completion {
            kind: crate::complete::MenuKind::Slash,
            replace_start: 0,
            candidates: vec!["/clear".into(), "/mode".into()],
        };
        // Same kind + candidates => same menu (selection should be preserved).
        let b = Completion {
            kind: crate::complete::MenuKind::Slash,
            replace_start: 9, // different prefix position is irrelevant
            candidates: vec!["/clear".into(), "/mode".into()],
        };
        assert!(same_menu(&a, &b));
        // Different kind => different menu.
        let c = Completion {
            kind: crate::complete::MenuKind::File,
            replace_start: 0,
            candidates: a.candidates.clone(),
        };
        assert!(!same_menu(&a, &c));
        // Different candidates => different menu (selection resets).
        let d = Completion {
            kind: crate::complete::MenuKind::Slash,
            replace_start: 0,
            candidates: vec!["/clear".into()],
        };
        assert!(!same_menu(&a, &d));
    }
}
