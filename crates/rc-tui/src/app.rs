//! App state, the poll loop, and the keymap. The loop: drain pending events
//! into state, render, poll crossterm for up to one frame, translate a key into
//! a [`rc_rt::UserAction`] (or a local effect), repeat until quit.
//!
//! Rendering reads only [`ViewState`], which is cheap to build in a test — the
//! `Runtime`/`EventStream` are kept separate so a `ratatui::backend::TestBackend`
//! render test needs no tokio and no model.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use rc_core::{AgentMode, AskResponse, ToolCall, ToolResultBody, Turn};
use rc_rt::{AgentEvent, EventStream, Runtime, UserAction};
use serde_json::Value;

use crate::complete::{self, Completion};
use crate::diff;
use crate::theme;
use crate::view::{self, CompletionMenu, PendingAsk, ViewState};
use crate::Term;

/// Poll cadence while a turn is in flight. `crossterm::event::poll` only wakes
/// on terminal input, so the loop drains the streaming channel once per poll;
/// a short busy tick keeps token latency low (~8 ms ≈ 120 fps) and the
/// spinner smooth. The cost is a ~125 Hz idle-ish wake while busy, which is
/// negligible next to the work the loop is already doing.
const TICK_BUSY: Duration = Duration::from_millis(8);
/// Poll cadence while idle. Nothing is streaming, so there's no latency to
/// optimize — a slower tick saves CPU (and battery) while the user reads.
const TICK_IDLE: Duration = Duration::from_millis(33);

/// The transcript range occupied by one in-flight tool call. Tool details are
/// useful while the call is running, but become noise once its terminal event
/// arrives; at that point this whole range is replaced by one compact row.
#[derive(Debug, Clone)]
struct LiveToolBlock {
    start: usize,
    len: usize,
    tool: String,
    started: Instant,
}

pub(crate) struct App {
    runtime: Runtime,
    stream: EventStream,
    view: ViewState,
    /// The session cwd, used to resolve `@file` completions (M4c).
    cwd: PathBuf,
    quit: bool,
    /// In-flight tool blocks keyed by the runtime's stable call id. Keeping the
    /// transcript ranges here lets parallel calls finish in any order while
    /// each completed block still collapses in place.
    live_tools: HashMap<String, LiveToolBlock>,
    /// Set when `/menu` picks a session to resume or a directory to start in.
    /// The TUI can't act on it itself — a different session means a different
    /// cwd, tool set, and permission roots, all built in `rc-cli` — so it
    /// quits and hands this back for the host to rebuild against.
    outcome: Option<crate::menu::Outcome>,
}

impl App {
    pub(crate) fn new(
        runtime: Runtime,
        model_name: String,
        cwd: PathBuf,
        history: Vec<Turn>,
    ) -> Self {
        let stream = runtime.subscribe();
        let mut view = ViewState::new(model_name);
        restore_history(&mut view, &history);
        // The welcome card is rendered from `cwd` (and the model name) when the
        // transcript is empty — no transcript lines pushed here.
        view.cwd = cwd.display().to_string();
        // Recall persisted prompt history so Alt+↑/↓ reaches prior sessions,
        // not just this one. Best-effort: no file yet → empty, never blocks.
        if let Some(p) = sc_history_path() {
            view.prompt_history = load_history(&p);
        }
        Self {
            runtime,
            stream,
            view,
            cwd,
            quit: false,
            live_tools: HashMap::new(),
            outcome: None,
        }
    }
}

/// Run the TUI main loop. Returns when the user quits, reporting whether
/// `/menu` asked the host to switch to another session (see
/// [`crate::menu::Outcome`]).
pub(crate) fn run(
    terminal: &mut Term,
    runtime: Runtime,
    model_name: String,
    cwd: PathBuf,
    initial_mode: AgentMode,
    history: Vec<Turn>,
) -> anyhow::Result<Option<crate::menu::Outcome>> {
    let mut app = App::new(runtime, model_name, cwd, history);
    // The mode the host resolved — a resumed session's own mode, or the
    // configured default. Set before the first draw so the status bar agrees
    // with the engine from frame one instead of claiming "default" until
    // something happens to change it.
    app.view.mode = initial_mode;
    loop {
        app.drain_events();
        terminal.draw(|f| view::draw(f, &mut app.view))?;
        if app.quit {
            break;
        }
        // Drain frequently while a turn streams, sparingly while idle. The
        // broadcast channel buffers anything that arrives between wakes, so a
        // longer idle tick loses no events — it only delays *display*, which
        // doesn't matter when there's nothing to display.
        let tick = if app.view.busy { TICK_BUSY } else { TICK_IDLE };
        if event::poll(tick)? {
            match event::read()? {
                // Keyboard drives the composer and the keymap. Only Press events
                // count — crossterm also emits Release on some terminals, and
                // acting on both would double every keystroke.
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                // Bracketed paste arrives as one payload, so embedded newlines
                // are editor content rather than a burst of submit keys.
                Event::Paste(text) => app.handle_paste(&text),
                // Mouse capture is on (see lib.rs), so wheel events arrive here.
                // Without this arm they were read and dropped, which is why the
                // trackpad/mouse "couldn't scroll up and down": the keyboard
                // scrollback worked, but the wheel did nothing. Scroll is
                // independent of the composer/menu/ask state, so it routes
                // straight to the scroll math without touching handle_key.
                Event::Mouse(ev) => app.handle_mouse(ev),
                _ => {}
            }
        }
    }
    Ok(app.outcome)
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
            AgentEvent::Text(t) => {
                v.finish_reasoning(Instant::now());
                v.current_text.push_str(&t);
                v.current_dirty = true;
                track_stream(v, &t);
            }
            AgentEvent::Reasoning(r) => {
                // Reasoning is the model's chain-of-thought, kept in its own
                // buffer so it renders distinctly (dim/italic under a header)
                // rather than blurring into the answer. It doesn't count toward
                // the answer's tokens/sec meter — that measures output, not
                // deliberation.
                v.note_reasoning(Instant::now());
                v.current_reasoning.push_str(&r);
                v.current_dirty = true;
            }
            AgentEvent::ToolStart { call } => {
                v.flush_text();
                // No model-thinking clock runs while the tool itself executes;
                // the next phase begins when the last parallel call finishes.
                v.reasoning_started = None;
                v.reasoning_elapsed = None;
                // A tool splits the turn's stream; reset the rate meter so the
                // next streaming segment measures itself, not the tool's wait.
                v.stream_chars = 0;
                v.stream_started = None;
                // Call ids are expected to be unique, but if a provider reuses
                // one, compact the stale block first instead of orphaning an
                // expanded block that can never receive its own ToolEnd.
                if self.live_tools.contains_key(&call.id) {
                    collapse_tool_call(
                        v,
                        &mut self.live_tools,
                        &call.id,
                        &call.name,
                        &ToolResultBody::Interrupted,
                        Instant::now(),
                    );
                    v.running = v.running.saturating_sub(1);
                }

                let started = Instant::now();
                let summary = summarize_args(&call.arguments);
                let start = v.transcript.len();
                v.transcript.push(tool_start_line(&call.name, &summary));
                // An Edit previews the change as a word-level diff of old -> new.
                if let Some((path, old, new)) = edit_args(&call.arguments) {
                    v.transcript
                        .push(Line::styled(format!("  ⤿ edit {path}"), dim_style()));
                    v.transcript.push(diff::word_diff_line(&old, &new));
                }
                self.live_tools.insert(
                    call.id,
                    LiveToolBlock {
                        start,
                        len: v.transcript.len() - start,
                        tool: call.name.clone(),
                        started,
                    },
                );
                // Track the in-flight call so the live tool-spinner has a name.
                // A batch may start several; keep the most recent and count
                // them so the spinner only clears when the last one lands.
                v.running = v.running.saturating_add(1);
                v.running_tool = Some(call.name);
            }
            AgentEvent::ToolEnd {
                call_id,
                tool,
                result,
            } => {
                v.flush_text();
                collapse_tool_call(
                    v,
                    &mut self.live_tools,
                    &call_id,
                    &tool,
                    &result,
                    Instant::now(),
                );
                v.running = v.running.saturating_sub(1);
                if v.running == 0 {
                    v.running_tool = None;
                    v.begin_reasoning_phase(Instant::now());
                }
            }
            AgentEvent::PermissionAsk {
                id,
                tool,
                input,
                reason,
            } => {
                v.flush_text();
                v.pending_ask = Some(PendingAsk {
                    id,
                    tool,
                    input,
                    reason,
                });
            }
            AgentEvent::PermissionDecision { .. } => v.pending_ask = None,
            AgentEvent::Iter { .. } => {}
            AgentEvent::Usage(u) => {
                // Replace the preflight estimate with the authoritative context
                // size returned for this request. Cache efficiency belongs to
                // that same prompt-token denominator, so display it as a rate.
                v.context_tokens = Some(u.prompt_tokens);
                v.context_tokens_estimated = false;
                v.cache_hit_rate = u.cache_hit_rate();
                v.last_usage = Some(u);
            }
            AgentEvent::Context {
                chars: _,
                est_tokens,
            } => {
                // Emitted synchronously before each model request. The later
                // Usage event replaces this estimate for the same request.
                v.context_tokens = Some(est_tokens as u64);
                v.context_tokens_estimated = true;
                v.cache_hit_rate = None;
            }
            AgentEvent::Outcome(_) => {
                v.flush_text();
                collapse_unfinished_tool_calls(v, &mut self.live_tools, Instant::now());
                end_turn(v);
            }
            AgentEvent::Error(e) => {
                v.flush_text();
                collapse_unfinished_tool_calls(v, &mut self.live_tools, Instant::now());
                v.transcript.extend(error_block(&e));
                end_turn(v);
            }
            AgentEvent::ModeChanged(m) => v.mode = m,
            AgentEvent::Notice(n) => {
                v.flush_text();
                v.transcript
                    .push(Line::styled(format!("· {n}"), dim_style()));
            }
            AgentEvent::Ready => {
                v.busy = true;
                // Mark the turn clock if submit didn't already (it sets it
                // optimistically so the indicator appears the instant Enter is
                // pressed). Keep the submit timestamp when present — the
                // elapsed readout then measures from the keystroke, so even a
                // slow submit→Ready gap shows live motion, not a frozen screen.
                if v.turn_started.is_none() {
                    v.turn_started = Some(Instant::now());
                }
                if v.reasoning_started.is_none() {
                    v.begin_reasoning_phase(v.turn_started.unwrap_or_else(Instant::now));
                }
            }
            AgentEvent::Idle => {
                v.flush_text();
                collapse_unfinished_tool_calls(v, &mut self.live_tools, Instant::now());
                end_turn(v);
            }
        }
    }

    /// Keys while the `/menu` modal is open.
    ///
    /// Two sub-modes: editing a settings field (a tiny line editor, where only
    /// Enter/Esc/typing matter) and navigating (arrows + Enter + Esc). Nothing
    /// here touches the composer or the runtime — selecting a session records
    /// an [`Outcome`](crate::menu::Outcome) and quits, because switching
    /// sessions has to be done by the host.
    fn handle_menu_key(&mut self, key: KeyEvent) {
        let sessions_dir = sessions_dir_for_menu();
        let cwd = self.cwd.clone();
        let Some(menu) = self.view.menu_overlay.as_mut() else {
            return;
        };

        // Editing a field: a minimal line editor.
        if let Some(buf) = menu.editing.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let value = buf.clone();
                    menu.commit(&value, &cwd);
                }
                KeyCode::Esc => {
                    menu.editing = None;
                    menu.status = None;
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Up => menu.move_selection(-1),
            KeyCode::Down => menu.move_selection(1),
            // ←/→ cycle a choice/bool setting in place; on other pages ← is
            // "back", which is the more useful binding there.
            KeyCode::Right if menu.page == crate::menu::MenuPage::Settings => {
                menu.cycle_current(1, &cwd)
            }
            KeyCode::Left if menu.page == crate::menu::MenuPage::Settings => {
                menu.cycle_current(-1, &cwd)
            }
            KeyCode::Left => {
                if !menu.back() {
                    self.view.menu_overlay = None;
                }
            }
            KeyCode::Esc => self.view.menu_overlay = None,
            KeyCode::Char('r') => menu.refresh(&sessions_dir, &cwd),
            // Only reachable outside the editor (the editing branch returns
            // above), so this can't swallow a `d` typed into a model name.
            KeyCode::Char('d') => menu.remove_current_model(&cwd),
            KeyCode::Char('q') => self.view.menu_overlay = None,
            KeyCode::Enter => self.activate_menu_row(),
            _ => {}
        }
    }

    /// Act on the selected menu row.
    fn activate_menu_row(&mut self) {
        let Some(menu) = self.view.menu_overlay.as_mut() else {
            return;
        };
        let Some(row) = menu.current_row() else {
            return;
        };
        match row {
            crate::menu::Row::Goto(page) => menu.goto(page),
            crate::menu::Row::Project(dir) => menu.goto(crate::menu::MenuPage::Sessions(dir)),
            crate::menu::Row::Field(_) => menu.begin_edit(),
            crate::menu::Row::Close => self.view.menu_overlay = None,
            // Both of these leave the TUI: the host rebuilds the agent for the
            // new session/directory and runs a fresh TUI over it.
            crate::menu::Row::Session(path) => {
                self.outcome = Some(crate::menu::Outcome::Resume(path));
                self.runtime.action(UserAction::Quit);
                self.quit = true;
            }
            crate::menu::Row::NewSession(dir) => {
                self.outcome = Some(crate::menu::Outcome::NewIn(dir));
                self.runtime.action(UserAction::Quit);
                self.quit = true;
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

        // `/menu` is modal: while it's open it consumes every key, so nothing
        // reaches the composer behind it.
        if self.view.menu_overlay.is_some() {
            self.handle_menu_key(key);
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
                Some(r) => self.runtime.action(UserAction::PermissionAnswer {
                    id: ask.id,
                    response: r,
                }),
                None => self.view.pending_ask = Some(ask), // ignored key: keep the ask open
            }
            return;
        }

        // While a completion menu is open, arrow/Tab/Esc drive the menu; other
        // keys fall through to the composer (so typing keeps filtering it).
        if let Some(menu) = self.view.menu.take() {
            match key.code {
                KeyCode::Tab => {
                    // Accept the selected candidate into the composer and keep
                    // the menu open for continued typing/filtering.
                    if let Some(new) =
                        complete::apply(&self.view.composer, &menu.completion, menu.selected)
                    {
                        self.view.clear_paste_markers();
                        self.view.composer = new;
                    }
                    self.refresh_menu();
                    return;
                }
                KeyCode::Enter => {
                    // Accept the selected candidate. For slash commands, the
                    // accepted text is a complete host-side command — fall
                    // through to the submit logic so one Enter both accepts and
                    // runs it (the old path reopened the menu on the exact match,
                    // so a single Enter re-accepted forever and the user had to
                    // press Esc then Enter). For file mentions, accept and close
                    // the menu; the next Enter submits, no Esc needed.
                    if let Some(new) =
                        complete::apply(&self.view.composer, &menu.completion, menu.selected)
                    {
                        self.view.clear_paste_markers();
                        self.view.composer = new;
                    }
                    if menu.completion.kind == complete::MenuKind::Slash {
                        // Fall through to the composer's Enter handler below, which
                        // will submit the now-complete slash command.
                    } else {
                        // File mention: menu dismissed; do not reopen on the
                        // completed mention. The next Enter submits.
                        return;
                    }
                }
                KeyCode::Esc => {
                    // Dismiss the menu without accepting; key consumed.
                    return;
                }
                KeyCode::Up => {
                    let selected = menu.selected.saturating_sub(1);
                    self.view.menu = Some(CompletionMenu {
                        completion: menu.completion,
                        selected,
                    });
                    return;
                }
                KeyCode::Down => {
                    let max = menu.completion.candidates.len().saturating_sub(1);
                    let selected = (menu.selected + 1).min(max);
                    self.view.menu = Some(CompletionMenu {
                        completion: menu.completion,
                        selected,
                    });
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
                    self.view.clear_paste_markers();
                    // Bare `exit` / `quit` (shell-style) quits the session like
                    // Ctrl+C instead of being sent to the model. Slash forms
                    // (`/exit`, `/quit`) are handled by `handle_slash` below.
                    if is_quit_word(&text) {
                        self.runtime.action(UserAction::Quit);
                        self.quit = true;
                    } else if let Some(action) = self.handle_slash(&text) {
                        // Slash commands are host-side actions (or prompt
                        // expansions), not raw prompts. An unrecognized `/…`
                        // falls through and is submitted to the model verbatim.
                        self.run_slash(action);
                    } else {
                        self.submit_prompt(text);
                    }
                    self.refresh_menu();
                    // A submit (or host-side command) is a "watch the result"
                    // moment — snap back to the bottom regardless of where the
                    // user had scrolled.
                    self.jump_to_bottom();
                }
            }
            KeyCode::Esc => match esc_action(&self.view) {
                // Esc is overloaded by state, and the ordering matters: a
                // drafted prompt must never be lost to a stray Esc. See
                // [`esc_action`] for the decision table.
                EscAction::Cancel => self.runtime.action(UserAction::Cancel),
                EscAction::RestoreDraft => {
                    // Browsing history → return to the live draft, not clear it.
                    self.view.history_pos = None;
                    self.view.clear_paste_markers();
                    self.view.composer = std::mem::take(&mut self.view.history_draft);
                    self.view.last_input = Some(Instant::now());
                    self.refresh_menu();
                }
                EscAction::Clear => {
                    self.view.composer.clear();
                    self.view.clear_paste_markers();
                    self.view.last_input = Some(Instant::now());
                    self.refresh_menu();
                }
                EscAction::Quit => {
                    self.runtime.action(UserAction::Quit);
                    self.quit = true;
                }
            },
            KeyCode::BackTab => {
                // Shift+Tab: cycle the permission mode
                // (Default -> AcceptEdits -> Plan -> Ask -> Auto -> Default).
                let next = cycle_mode(self.view.mode);
                self.view.mode = next; // optimistic; ModeChanged confirms
                self.runtime.action(UserAction::SetMode(next));
            }
            // Scrollback. Up/Down/page jump to the composer only when the
            // completion menu is open (handled above); with the menu closed they
            // scroll the transcript. New content arriving while scrolled up just
            // grows the buffer below the held view — the status bar reports it.
            KeyCode::PageUp => self.scroll_page_up(),
            KeyCode::PageDown => self.scroll_page_down(),
            // Alt+↑/↓ recall prompt history. Up/Down alone scroll the
            // transcript (the documented, verified scrollback behavior), so
            // history gets a distinct, non-conflicting modifier.
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => self.history_prev(),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => self.history_next(),
            KeyCode::Up => self.scroll_line_up(),
            KeyCode::Down => self.scroll_line_down(),
            KeyCode::Home => {
                self.view.follow = false;
                self.view.scroll_top = 0;
            }
            KeyCode::End => self.jump_to_bottom(),
            // Emacs-style line editing (caret stays at the end, so the `@`/`/`
            // completion engine's caret-at-end invariant still holds). Without
            // these arms Ctrl+letter would fall through to `Char(c)` and insert
            // the raw letter — a latent bug once raw mode passes them through.
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => match c {
                'w' | 'W' => self.delete_word(),
                't' | 'T' => self.toggle_latest_reasoning(),
                'u' | 'U' => {
                    self.view.composer.clear();
                    self.view.clear_paste_markers();
                    self.view.last_input = Some(Instant::now());
                    self.refresh_menu();
                }
                _ => {} // other Ctrl+letter combos: ignore, don't insert
            },
            // Alt+Backspace (mac) and Ctrl+W (emacs) both delete the last word.
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => self.delete_word(),
            KeyCode::Char(c) => {
                self.view.composer.push(c);
                self.view.last_input = Some(Instant::now());
                self.refresh_menu();
            }
            KeyCode::Backspace => {
                self.view.clear_paste_markers();
                self.view.composer.pop();
                self.view.last_input = Some(Instant::now());
                self.refresh_menu();
            }
            _ => {}
        }
    }

    /// Native bracketed paste is deliberately separate from key handling:
    /// newlines stay inside the composer and cannot trigger Enter's submit arm.
    fn handle_paste(&mut self, text: &str) {
        // Modal surfaces own their input. Never paste into the hidden composer
        // while a settings editor or permission decision is in front of it.
        if self.view.menu_overlay.is_some() || self.view.pending_ask.is_some() {
            return;
        }
        if self.view.append_paste(text) == 0 {
            return;
        }
        self.view.history_pos = None;
        self.view.last_input = Some(Instant::now());
        self.refresh_menu();
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
                self.view.menu = Some(CompletionMenu {
                    completion,
                    selected,
                });
            }
            None => self.view.menu = None,
        }
    }

    /// Total renderable transcript lines: cached turns plus the in-progress
    /// streaming text (parsed, since that's how it's drawn), plus the live
    /// "thinking"/"running <tool>" spinner line shown while a turn is in flight
    /// with no streaming text yet. Must match `view::draw_transcript`'s notion
    /// of the trailing lines or scroll math drifts off by one during a turn.
    fn total_lines(&self) -> usize {
        // Read the cached parse (refreshed each draw) rather than re-parsing the
        // streaming buffer here — scroll handlers fire on user key/wheel events,
        // and the cache is fresh from the last draw.
        let streaming =
            if !self.view.current_text.is_empty() || !self.view.current_reasoning.is_empty() {
                self.view.current_parsed.len()
            } else if self.view.busy {
                1
            } else {
                0
            };
        self.view.transcript.len() + streaming
    }

    /// The topmost line currently shown — the bottom of the transcript when
    /// following, else the user's held scroll position clamped to fit.
    fn current_top(&self, total: usize) -> usize {
        let h = self.view.area_height.max(1);
        if self.view.follow {
            total.saturating_sub(h)
        } else {
            self.view.scroll_top.min(total.saturating_sub(h))
        }
    }

    /// Pin the view to the bottom (auto-scroll on new content).
    fn jump_to_bottom(&mut self) {
        self.view.follow = true;
        self.view.scroll_top = 0;
    }

    fn scroll_line_up(&mut self) {
        let total = self.total_lines();
        let top = self.current_top(total);
        self.view.follow = false;
        self.view.scroll_top = top.saturating_sub(1);
    }

    fn scroll_line_down(&mut self) {
        if self.view.follow {
            return; // already watching the bottom
        }
        let total = self.total_lines();
        let h = self.view.area_height.max(1);
        let new_top = self.view.scroll_top + 1;
        if new_top + h >= total {
            self.jump_to_bottom();
        } else {
            self.view.scroll_top = new_top;
        }
    }

    fn scroll_page_up(&mut self) {
        let total = self.total_lines();
        let h = self.view.area_height.max(1);
        let top = self.current_top(total);
        self.view.follow = false;
        self.view.scroll_top = top.saturating_sub(h);
    }

    fn scroll_page_down(&mut self) {
        if self.view.follow {
            return;
        }
        let total = self.total_lines();
        let h = self.view.area_height.max(1);
        let new_top = self.view.scroll_top + h;
        if new_top + h >= total {
            self.jump_to_bottom();
        } else {
            self.view.scroll_top = new_top;
        }
    }

    /// Mouse wheel scroll. Each notch moves a few lines; reaching the bottom
    /// re-pins to follow, reaching the top holds at line 0. Single-pass (one
    /// `total_lines` computation) so a fast trackpad swipe doesn't re-parse the
    /// streaming markdown once per line.
    fn handle_mouse(&mut self, ev: MouseEvent) {
        const WHEEL_LINES: i32 = 3;
        match ev.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES),
            MouseEventKind::Down(MouseButton::Left) => self.toggle_expandable_at(ev.column, ev.row),
            // Drag/hold/right-click aren't tracked. Native text selection still
            // works in most terminals by holding Shift while dragging, the
            // standard mouse-capture tradeoff.
            _ => {}
        }
    }

    /// Move the held scroll position by `delta` lines (negative = up). Clamps
    /// to `[0, max_top]`; landing at `max_top` (the bottom) re-pins to follow so
    /// new content auto-scrolls again.
    fn scroll_by(&mut self, delta: i32) {
        let total = self.total_lines();
        let h = self.view.area_height.max(1);
        let max_top = total.saturating_sub(h);
        let cur = if self.view.follow {
            max_top
        } else {
            self.view.scroll_top.min(max_top)
        };
        let new_top = (cur as i32 + delta).clamp(0, max_top as i32) as usize;
        if new_top >= max_top {
            self.jump_to_bottom();
        } else {
            self.view.follow = false;
            self.view.scroll_top = new_top;
        }
    }

    /// Ctrl+W / Alt+Backspace: delete the last whitespace-delimited word from
    /// the end of the composer. Char-safe (works on `&str` boundaries, never
    /// mid-multibyte). Keeps the caret at the end, so completion still holds.
    fn delete_word(&mut self) {
        self.view.clear_paste_markers();
        delete_last_word(&mut self.view.composer);
        self.view.last_input = Some(Instant::now());
        self.refresh_menu();
    }

    /// Ctrl+T expands/collapses the newest completed `thought for N.NNs`
    /// block. Avoid splicing while a tool block is live because its in-place
    /// completion bookkeeping references transcript indices.
    fn toggle_latest_reasoning(&mut self) {
        if !self.live_tools.is_empty() {
            return;
        }
        let Some(toggle) = self.view.toggle_latest_reasoning() else {
            return;
        };
        if toggle.expanded {
            // Put the summary at the top so even a long reasoning body is
            // immediately visible rather than expanding above the viewport.
            self.view.follow = false;
            self.view.scroll_top = toggle.summary_index;
        } else {
            self.jump_to_bottom();
        }
    }

    /// A left click on a visible thought or completed-tool label toggles that
    /// exact retained block. Keep the clicked summary anchored so expanding a
    /// long body does not move the target out from under the user.
    fn toggle_expandable_at(&mut self, column: u16, row: u16) {
        if !self.live_tools.is_empty() {
            return;
        }
        let toggle = self
            .view
            .toggle_reasoning_at(column, row)
            .or_else(|| self.view.toggle_tool_at(column, row));
        let Some(toggle) = toggle else { return };
        self.view.follow = false;
        self.view.scroll_top = toggle.summary_index;
    }

    /// Alt+↑: step one entry toward the oldest prompt. Entering history from
    /// the live composer stashes the in-progress draft so Alt+↓ past the newest
    /// restores it. Clamps at the oldest (stays on the first entry).
    fn history_prev(&mut self) {
        let entering = self.view.history_pos.is_none();
        let draft = if entering {
            self.view.composer.as_str()
        } else {
            self.view.history_draft.as_str()
        };
        let (pos, text) = browse_history(
            &self.view.prompt_history,
            self.view.history_pos,
            draft,
            true,
        );
        if entering && pos.is_some() {
            self.view.history_draft = self.view.composer.clone();
        }
        self.view.history_pos = pos;
        self.view.clear_paste_markers();
        self.view.composer = text;
        self.view.last_input = Some(Instant::now());
        self.refresh_menu();
    }

    /// Alt+↓: step one entry toward the newest prompt, and past the newest
    /// return to the live draft. No-op when already on the live draft.
    fn history_next(&mut self) {
        if self.view.history_pos.is_none() {
            return;
        }
        let (pos, text) = browse_history(
            &self.view.prompt_history,
            self.view.history_pos,
            &self.view.history_draft,
            false,
        );
        self.view.history_pos = pos;
        self.view.clear_paste_markers();
        self.view.composer = text;
        self.view.last_input = Some(Instant::now());
        self.refresh_menu();
    }

    /// Submit `text` to the model as a normal user turn: echo the prompt line,
    /// mark the turn in flight (so the spinner shows the instant Enter is
    /// pressed, before the driver is Ready), and dispatch `UserAction::Submit`.
    /// Shared by the plain-prompt path and the prompt-expansion slash commands.
    fn submit_prompt(&mut self, text: String) {
        self.view.transcript.push(user_prompt_line(&text));
        // Record the prompt for Alt+↑/↓ recall (deduped, bash-style), and leave
        // history-browsing mode — a fresh submit always returns to the live
        // draft.
        push_history(&mut self.view.prompt_history, text.clone());
        self.view.history_pos = None;
        self.view.history_draft.clear();
        // Persist the prompt for cross-session recall. Best-effort — a failed
        // append never blocks the turn.
        if let Some(p) = sc_history_path() {
            append_history(&p, &text);
        }
        // Optimistically mark the turn in flight so the "thinking" indicator
        // appears the instant Enter is pressed — there's a real gap between
        // Submit and the driver's Ready during which the screen would
        // otherwise be still. The clock stays on the submit instant;
        // Idle/Outcome/Error clear it.
        let started = Instant::now();
        self.view.busy = true;
        self.view.turn_started = Some(started);
        self.view.begin_reasoning_phase(started);
        self.view.running = 0;
        self.view.running_tool = None;
        self.view.stream_chars = 0;
        self.view.stream_started = None;
        // The previous request's returned count/rate must not masquerade as
        // the new request. Its preflight estimate arrives through Context.
        self.view.context_tokens = None;
        self.view.context_tokens_estimated = false;
        self.view.cache_hit_rate = None;
        self.runtime.action(UserAction::Submit(text));
    }

    /// Push a styled info block: one accent heading line, then chrome body
    /// lines. Used by the introspection commands (/cost, /status, …) so they
    /// all read the same way in the transcript.
    fn push_info(&mut self, heading: &str, lines: &[String]) {
        let p = theme::palette();
        self.view
            .transcript
            .push(Line::styled(heading.to_string(), p.accent()));
        for l in lines {
            self.view
                .transcript
                .push(Line::styled(l.clone(), p.chrome()));
        }
    }

    /// Run a recognized slash command. Host-side actions mutate the view (and
    /// may dispatch a `UserAction`); prompt-expansion commands call
    /// `submit_prompt` with a canned instruction. Everything here is a
    /// best-effort, real implementation against the state `sc` already has —
    /// commands whose backends aren't wired yet degrade to an info note via
    /// `Note`, never a silent no-op.
    fn run_slash(&mut self, action: SlashAction) {
        let p = theme::palette();
        match action {
            SlashAction::Menu => {
                self.view.menu_overlay = Some(crate::menu::MenuState::new(
                    &sessions_dir_for_menu(),
                    &self.cwd,
                ));
            }
            SlashAction::Help => {
                self.view
                    .transcript
                    .push(Line::styled("commands".to_string(), p.accent()));
                let mut cmds: Vec<(&'static str, &'static str)> =
                    complete::slash_palette().to_vec();
                cmds.sort();
                for (name, desc) in cmds {
                    self.view.transcript.push(Line::from(vec![
                        Span::styled(format!("  {name:<16}"), p.code()),
                        Span::styled(desc.to_string(), p.chrome()),
                    ]));
                }
                self.view
                    .transcript
                    .push(Line::styled("keys".to_string(), p.accent()));
                let mk = |s: &str| Line::styled(s.to_string(), p.chrome());
                self.view
                    .transcript
                    .push(mk("  @<path>      mention a file (tab completes)"));
                self.view
                    .transcript
                    .push(mk("  Shift+Tab    cycle permission mode"));
                self.view
                    .transcript
                    .push(mk("  PgUp/PgDn    scroll the transcript"));
                self.view
                    .transcript
                    .push(mk("  Alt+↑/↓      recall prompt history"));
                self.view
                    .transcript
                    .push(mk("  Ctrl+T       expand/collapse latest thought"));
                self.view
                    .transcript
                    .push(mk("  Ctrl+W / U   delete word / clear the line"));
                self.view.transcript.push(mk(
                    "  Esc          interrupt a turn · clear a draft · quit when idle",
                ));
                self.view.transcript.push(mk("  Ctrl+C       quit"));
                self.view.transcript.push(mk(
                    "  exit / quit  quit (shell-style, typed in the composer)",
                ));
                self.view
                    .transcript
                    .push(mk("  SC_NO_ANIM=1 / NO_COLOR=1 disable motion / color"));
            }
            SlashAction::Clear => {
                self.view.clear_transcript();
                self.view.current_text.clear();
                self.live_tools.clear();
                // The welcome card reappears on its own once the transcript is
                // empty (see `draw_transcript`'s pre-turn guard).
                end_turn(&mut self.view);
            }
            SlashAction::Compact => {
                // Like /clear but signals intent: the context was compacted.
                let lines = self.view.transcript.len();
                self.view.clear_transcript();
                self.view.current_text.clear();
                self.live_tools.clear();
                end_turn(&mut self.view);
                self.view.transcript.push(Line::styled(
                    format!("context compacted · cleared {lines} transcript lines"),
                    p.chrome(),
                ));
            }
            SlashAction::CycleMode => {
                let next = cycle_mode(self.view.mode);
                self.view.mode = next; // optimistic; ModeChanged confirms
                self.runtime.action(UserAction::SetMode(next));
            }
            SlashAction::Rewind { steps } => {
                self.runtime.action(UserAction::Rewind { steps });
            }
            SlashAction::Cost => {
                let lines = match &self.view.last_usage {
                    Some(u) => {
                        let cache_rate = u
                            .cache_hit_rate()
                            .map(|rate| format!("{:.1}%", rate * 100.0))
                            .unwrap_or_else(|| "not reported".to_string());
                        vec![
                            format!("  prompt      {} tok", u.prompt_tokens),
                            format!("  completion  {} tok", u.completion_tokens),
                            format!("  total       {} tok", u.total_tokens),
                            format!("  cache hit   {cache_rate}"),
                        ]
                    }
                    None => vec!["  no usage reported yet".into()],
                };
                self.push_info("usage", &lines);
            }
            SlashAction::Context => {
                let lines = match self.view.context_tokens {
                    Some(tokens) if self.view.context_tokens_estimated => {
                        vec![format!("  tokens  ~{tokens} (preflight estimate)")]
                    }
                    Some(tokens) => vec![format!("  tokens  {tokens} (provider reported)")],
                    None => vec!["  no context token count yet".into()],
                };
                self.push_info("context", &lines);
            }
            SlashAction::Status => {
                let lines = vec![
                    format!("  model   {}", self.view.model_name),
                    format!("  mode    {:?}", self.view.mode),
                    format!("  cwd     {}", self.cwd.display()),
                    format!("  busy    {}", self.view.busy),
                    format!("  lines   {}", self.view.transcript.len()),
                ];
                self.push_info("status", &lines);
            }
            SlashAction::Model => {
                self.view.transcript.push(Line::styled(
                    format!("model: {}", self.view.model_name),
                    p.chrome(),
                ));
            }
            SlashAction::Permissions => {
                let lines = vec![
                    format!("  mode   {:?}", self.view.mode),
                    "  rules  per-tool allow/deny are set at startup;".into(),
                    "         /mode or Shift+Tab cycles the mode.".into(),
                ];
                self.push_info("permissions", &lines);
            }
            SlashAction::Doctor => {
                let no_color = std::env::var_os("NO_COLOR")
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                let lines = vec![
                    format!("  model      {}", self.view.model_name),
                    format!(
                        "  cwd        {} ({})",
                        self.cwd.display(),
                        if self.cwd.exists() {
                            "present"
                        } else {
                            "missing"
                        }
                    ),
                    format!("  term rows  {}", self.view.area_height),
                    format!(
                        "  color      {}",
                        if no_color { "off (NO_COLOR)" } else { "on" }
                    ),
                    format!(
                        "  motion     {}",
                        if theme::animations_enabled() {
                            "on"
                        } else {
                            "off (SC_NO_ANIM)"
                        }
                    ),
                ];
                self.push_info("doctor", &lines);
            }
            SlashAction::History => {
                self.view.transcript.push(Line::styled(
                    format!(
                        "history: {} transcript lines this session",
                        self.view.transcript.len()
                    ),
                    p.chrome(),
                ));
            }
            SlashAction::Export => {
                let path = self.cwd.join("sc-export.txt");
                let body: String = self
                    .view
                    .transcript
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                match std::fs::write(&path, body) {
                    Ok(_) => self.view.transcript.push(Line::styled(
                        format!("exported transcript → {}", path.display()),
                        p.chrome(),
                    )),
                    Err(e) => self
                        .view
                        .transcript
                        .push(Line::styled(format!("export failed: {e}"), p.chrome())),
                }
            }
            SlashAction::Quit => {
                self.runtime.action(UserAction::Quit);
                self.quit = true;
            }
            SlashAction::Note(kind) => self.run_note(kind),
            SlashAction::Prompt(text) => self.submit_prompt(text),
        }
    }

    /// Print a one-line info note for a capability that isn't fully wired into
    /// `sc` yet. The note tells the user what the command *would* do and where
    /// the real control lives, so it's never a silent no-op.
    fn run_note(&mut self, kind: NoteKind) {
        let p = theme::palette();
        let msg = match kind {
            NoteKind::Vim => "vim keybindings aren't wired yet; the composer is emacs-style (Ctrl+W, Ctrl+U, Alt+Backspace)",
            NoteKind::TerminalSetup => "for Shift+Enter, bind your terminal to send a CSI-u / `\\n` escape; see its keybind settings",
            NoteKind::Login | NoteKind::Logout => "auth is set via ANTHROPIC_API_KEY / the configured backend; no interactive login yet",
            NoteKind::Mcp => "no MCP servers connected (MCP support isn't wired into sc yet)",
            NoteKind::Memory => "memory lives in CLAUDE.md and ~/.claude/projects/.../memory/; sc doesn't auto-load project memory yet",
            NoteKind::AddDir => "extra working dirs are set at startup; per-session /add-dir isn't wired yet",
            NoteKind::Approval => "approval is governed by the permission mode — cycle it with /mode or Shift+Tab",
            NoteKind::Update => "update sc with: cargo install --path . (or your distribution's update flow)",
            NoteKind::Resume => "session resume isn't wired yet; conversations aren't persisted across runs",
        };
        self.view
            .transcript
            .push(Line::styled(format!("» {msg}"), p.chrome()));
    }

    /// If `text` is a recognized slash command, return its host-side action.
    /// Returns `None` for anything else (including `@`-prefixed text), so the
    /// text is submitted as a normal prompt. Aliases (e.g. `/h`, `/c`, `/q`)
    /// map onto the same canonical action — they're recognized here but not
    /// advertised in the completion palette, so the menu stays uncluttered.
    fn handle_slash(&self, text: &str) -> Option<SlashAction> {
        let t = text.trim();
        // `/rewind` alone, or `/rewind <n>` with an explicit step count.
        if let Some(rest) = t.strip_prefix("/rewind") {
            let rest = rest.trim();
            if rest.is_empty() {
                return Some(SlashAction::Rewind { steps: 1 });
            }
            let n = rest.parse::<usize>().ok()?;
            return (n > 0).then_some(SlashAction::Rewind { steps: n });
        }
        match t {
            // Conversation / context hygiene.
            "/clear" | "/c" | "/new" => Some(SlashAction::Clear),
            "/menu" | "/m" => Some(SlashAction::Menu),
            "/compact" | "/cc" => Some(SlashAction::Compact),
            "/context" => Some(SlashAction::Context),
            // Session / environment introspection.
            "/cost" | "/usage" => Some(SlashAction::Cost),
            "/status" | "/s" => Some(SlashAction::Status),
            "/model" => Some(SlashAction::Model),
            "/mode" => Some(SlashAction::CycleMode),
            "/permissions" => Some(SlashAction::Permissions),
            "/doctor" => Some(SlashAction::Doctor),
            "/history" => Some(SlashAction::History),
            "/export" => Some(SlashAction::Export),
            // Lifecycle.
            "/quit" | "/exit" | "/q" => Some(SlashAction::Quit),
            "/resume" => Some(SlashAction::Note(NoteKind::Resume)),
            "/update" => Some(SlashAction::Note(NoteKind::Update)),
            "/login" => Some(SlashAction::Note(NoteKind::Login)),
            "/logout" => Some(SlashAction::Note(NoteKind::Logout)),
            // Integrations / capabilities.
            "/mcp" => Some(SlashAction::Note(NoteKind::Mcp)),
            "/memory" => Some(SlashAction::Note(NoteKind::Memory)),
            "/add-dir" => Some(SlashAction::Note(NoteKind::AddDir)),
            "/vim" => Some(SlashAction::Note(NoteKind::Vim)),
            "/terminal-setup" => Some(SlashAction::Note(NoteKind::TerminalSetup)),
            "/approval" => Some(SlashAction::Note(NoteKind::Approval)),
            // Prompt-expansion commands — submit a canned instruction.
            "/review" => Some(SlashAction::Prompt(
                "Review the pending code changes on the current branch. Summarize the diff, flag risks and bugs, and suggest concrete fixes.".into(),
            )),
            "/pr" => Some(SlashAction::Prompt(
                "Create a pull request for the current branch: summarize the changes and generate a title and body.".into(),
            )),
            "/init" => Some(SlashAction::Prompt(
                "Initialize a CLAUDE.md file documenting this codebase: structure, build/test commands, and conventions.".into(),
            )),
            "/diff" => Some(SlashAction::Prompt(
                "Show the diff of pending changes using git.".into(),
            )),
            "/release-notes" => Some(SlashAction::Prompt(
                "Generate release notes from the recent git history.".into(),
            )),
            "/bug" => Some(SlashAction::Prompt(
                "Help me report a bug: summarize the current problem and any recent errors from this session.".into(),
            )),
            "/doc" => Some(SlashAction::Prompt(
                "Generate documentation for the relevant code.".into(),
            )),
            "/fix" => Some(SlashAction::Prompt(
                "Find and fix bugs in the relevant code.".into(),
            )),
            "/explain" => Some(SlashAction::Prompt(
                "Explain the relevant code.".into(),
            )),
            "/edit" => Some(SlashAction::Prompt(
                "Apply the requested edits to the code.".into(),
            )),
            "/codebase" => Some(SlashAction::Prompt(
                "Summarize the structure of this codebase for context.".into(),
            )),
            "/docs" => Some(SlashAction::Prompt(
                "Read and summarize the relevant documentation.".into(),
            )),
            // Reference.
            "/help" | "/h" | "/?" => Some(SlashAction::Help),
            _ => None,
        }
    }
}

/// A host-side slash command (not submitted to the model). Each variant is a
/// recognized command from the union of Claude Code, Codex, and Cursor slash
/// palettes; aliases (e.g. `/h`, `/c`, `/q`) collapse onto the same variant in
/// `handle_slash`. `Note` covers the small "print an info line" commands that
/// don't have a richer action yet, and `Prompt` carries a canned instruction
/// that `run_slash` submits to the model as if the user had typed it.
#[derive(Debug, Clone)]
enum SlashAction {
    Help,
    /// Open the `/menu` modal (projects, sessions, settings).
    Menu,
    Clear,
    Compact,
    CycleMode,
    Rewind {
        steps: usize,
    },
    Cost,
    Context,
    Status,
    Model,
    Permissions,
    Quit,
    Export,
    Doctor,
    History,
    /// Print a short, static info line for a capability not fully wired yet.
    Note(NoteKind),
    /// Submit a canned instruction to the model (prompt-expansion commands).
    Prompt(String),
}

/// The "info note" commands — each prints a one-line status. They share a
/// handler (`run_note`) so adding a new one is one match arm + one line.
#[derive(Debug, Clone, Copy)]
enum NoteKind {
    Vim,
    TerminalSetup,
    Login,
    Logout,
    Mcp,
    Memory,
    AddDir,
    Approval,
    Update,
    Resume,
}

/// Two completions are "the same menu" if they're the same kind and would show
/// the same candidates — used to preserve the selection across keystrokes.
fn same_menu(a: &Completion, b: &Completion) -> bool {
    a.kind == b.kind && a.candidates == b.candidates
}

/// What `Esc` does in the current state. Factored out so the safety ordering
/// (never quit and lose a drafted prompt to a stray Esc) is testable with no
/// `Runtime`. Priority:
///
///   busy          → Cancel the in-flight turn
///   browsing hist → RestoreDraft (return to the live draft, not clear it)
///   draft present → Clear the composer (a second Esc, now empty, quits)
///   otherwise     → Quit
///
/// The menu and ask handlers `return` before the keymap reaches `Esc`, so this
/// only fires on a bare Esc with neither overlay open.
fn esc_action(state: &crate::view::ViewState) -> EscAction {
    if state.busy {
        EscAction::Cancel
    } else if state.history_pos.is_some() {
        EscAction::RestoreDraft
    } else if !state.composer.is_empty() {
        EscAction::Clear
    } else {
        EscAction::Quit
    }
}

/// The resolved effect of an `Esc` press. See [`esc_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscAction {
    Cancel,
    RestoreDraft,
    Clear,
    Quit,
}

/// Reset the per-turn animation state: not busy, clock stopped, no in-flight
/// tools. Called from every terminal turn event (Idle, Outcome, Error) and
/// from `/clear`. A free function over `&mut ViewState` (not a `&mut self`
/// method) so it can be called inside `apply`, where `&mut self.view` is
/// already borrowed.
fn end_turn(v: &mut crate::view::ViewState) {
    v.busy = false;
    v.turn_started = None;
    v.running = 0;
    v.running_tool = None;
    v.stream_chars = 0;
    v.stream_started = None;
    // Drop any stale in-progress parse so a fresh turn starts clean (also
    // covers `/clear`, which clears `current_text` but not the cache).
    v.current_parsed.clear();
    v.current_dirty = false;
    v.current_reasoning.clear();
    v.reasoning_started = None;
    v.reasoning_elapsed = None;
}

/// Accumulate a streaming delta into the tokens/sec meter. The first delta of
/// a turn stamps the stream epoch so the rate measures pure streaming time,
/// not the pre-stream tool/TTFT wait that would dilute it.
fn track_stream(v: &mut crate::view::ViewState, delta: &str) {
    if v.stream_started.is_none() {
        v.stream_started = Some(Instant::now());
    }
    v.stream_chars = v.stream_chars.saturating_add(delta.chars().count() as u64);
}

/// Bare-word quit commands typed in the composer — shell-style: `exit` or
/// `quit` (optionally surrounded by whitespace) quits the session like Ctrl+C,
/// instead of being sent to the model. Whitespace is tolerated; anything else
/// (a real prompt mentioning "exit", `/exit`, "exits", …) falls through to the
/// model so the word isn't stolen from normal prompts.
fn is_quit_word(text: &str) -> bool {
    matches!(text.trim(), "exit" | "quit")
}

fn cycle_mode(m: AgentMode) -> AgentMode {
    match m {
        // Ordered by how much they let through: ask (confirm everything) sits
        // at the cautious end, auto (confirm nothing) at the permissive one.
        AgentMode::Ask => AgentMode::Default,
        AgentMode::Default => AgentMode::AcceptEdits,
        AgentMode::AcceptEdits => AgentMode::Plan,
        AgentMode::Plan => AgentMode::Auto,
        AgentMode::Auto => AgentMode::Ask,
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
    if let Some(p) = v
        .get("file_path")
        .or_else(|| v.get("path"))
        .and_then(|x| x.as_str())
    {
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

/// The echoed user turn mirrors the composer's neutral `> ` prompt. The orange
/// brand mark belongs exclusively to assistant output, so direction reads
/// immediately without painting both sides of the conversation the same.
fn user_prompt_line(text: &str) -> Line<'static> {
    let p = theme::palette();
    Line::from(vec![
        Span::styled("> ", p.chrome()),
        Span::raw(text.to_string()),
    ])
}

/// Rehydrate the visible transcript before a resumed TUI's first frame. The
/// runtime already receives these turns for model context; replaying them here
/// makes resume visual as well as semantic. Successful tools stay hidden (the
/// normal completed-state policy), while failed tools remain expandable.
fn restore_history(view: &mut ViewState, history: &[Turn]) {
    let mut calls: HashMap<String, ToolCall> = HashMap::new();

    for turn in history {
        match turn {
            Turn::User { content, .. } => view.transcript.push(user_prompt_line(content)),
            Turn::Assistant {
                text,
                reasoning,
                calls: announced,
                usage,
                ..
            } => {
                view.restore_assistant_turn(reasoning.as_deref(), text);
                for call in announced {
                    calls.insert(call.id.clone(), call.clone());
                }
                if let Some(usage) = usage {
                    view.context_tokens = Some(usage.prompt_tokens);
                    view.context_tokens_estimated = false;
                    view.cache_hit_rate = usage.cache_hit_rate();
                    view.last_usage = Some(usage.clone());
                }
            }
            Turn::ToolResult {
                call_id,
                tool,
                result,
                duration,
            } => {
                let call = calls.remove(call_id);
                if matches!(result, ToolResultBody::Ok { .. }) {
                    continue;
                }
                let name = call
                    .as_ref()
                    .map(|call| call.name.as_str())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(tool)
                    .to_string();
                let mut body = Vec::new();
                if let Some(call) = call {
                    body.push(tool_start_line(&name, &summarize_args(&call.arguments)));
                    if let Some((path, old, new)) = edit_args(&call.arguments) {
                        body.push(Line::styled(format!("  ⤿ edit {path}"), dim_style()));
                        body.push(diff::word_diff_line(&old, &new));
                    }
                }
                body.extend(tool_result_detail_lines(result));
                view.push_tool_block(
                    tool_summary_line(&name, result, *duration, false),
                    tool_summary_line(&name, result, *duration, true),
                    body,
                );
            }
            Turn::SystemNote { kind, text } => {
                // Mode changes are replayed into state by rc-session and do not
                // need a transcript row. User-visible notes remain visible.
                if !matches!(kind, rc_core::NoteKind::ModeChange) {
                    view.transcript
                        .push(Line::styled(format!("· {text}"), dim_style()));
                }
            }
            // A failed model request (transport error, non-2xx after retries,
            // context-length rejection, …). Persisted so the session record shows
            // the failure; replayed here as the same red `✗` block the live
            // `AgentEvent::Error` path renders.
            Turn::Error { message, .. } => {
                view.transcript.extend(error_block(message));
            }
            // The user cancelled the turn mid-flight (Esc). Shown as a dim note so
            // a `--continue` resume makes the interruption visible in the scrollback.
            Turn::Cancelled { .. } => {
                view.transcript
                    .push(Line::styled("· cancelled".to_string(), dim_style()));
            }
        }
    }
}

/// A styled "tool starting" line: `▸ Name  summary` — the glyph in chrome, the
/// tool name in the default foreground, the argument summary dimmed.
fn tool_start_line(name: &str, summary: &str) -> Line<'static> {
    let p = theme::palette();
    Line::from(vec![
        Span::styled("▸ ", p.chrome()),
        Span::styled(name.to_string(), p.body()),
        Span::styled(format!("  {summary}"), p.chrome()),
    ])
}

/// The tiny failed-tool row. Successful tools disappear at completion; errors,
/// denials, and interruptions retain this timed summary and expand on click.
fn tool_summary_line(
    tool: &str,
    result: &ToolResultBody,
    elapsed: Duration,
    expanded: bool,
) -> Line<'static> {
    let p = theme::palette();
    let arrow = if expanded { "▾ " } else { "▸ " };
    let name = if tool.trim().is_empty() || tool.eq_ignore_ascii_case("tool") {
        "tool call"
    } else {
        tool
    };
    let mut spans = vec![
        Span::styled(arrow.to_string(), p.accent()),
        Span::styled(name.to_string(), p.body()),
        Span::styled(format!(" · {:.2}s", elapsed.as_secs_f64()), p.chrome()),
    ];
    let status = match result {
        ToolResultBody::Ok { .. } => None,
        ToolResultBody::Error { .. } => Some((" · failed", Color::Red)),
        ToolResultBody::Denied { .. } => Some((" · denied", Color::Yellow)),
        ToolResultBody::Interrupted => Some((" · interrupted", Color::DarkGray)),
    };
    if let Some((label, color)) = status {
        spans.push(Span::styled(label.to_string(), p.semantic(color)));
    }
    Line::from(spans)
}

/// Full result rows shown only when a completed tool is expanded. Nothing is
/// preview-truncated here: the retained body is the entire result delivered to
/// the TUI (including the runtime's own truncation sentinel, when present).
fn tool_result_detail_lines(result: &ToolResultBody) -> Vec<Line<'static>> {
    let p = theme::palette();
    let (glyph, label, color, body, truncated) = match result {
        ToolResultBody::Ok { content, truncated } => {
            ('✓', "output", Color::Green, content.as_ref(), *truncated)
        }
        ToolResultBody::Error { message, .. } => {
            ('✗', "error", Color::Red, message.as_str(), false)
        }
        ToolResultBody::Denied { reason } => ('⊘', "denied", Color::Yellow, reason.as_str(), false),
        ToolResultBody::Interrupted => ('–', "interrupted", Color::DarkGray, "", false),
    };
    let mut lines = vec![Line::styled(
        format!("  {glyph} {label}"),
        p.semantic(color),
    )];
    if !body.is_empty() {
        lines.extend(
            body.split('\n')
                .map(|line| Line::styled(format!("  │ {line}"), p.chrome())),
        );
    }
    if truncated {
        lines.push(Line::styled("  │ [output truncated]", p.chrome()));
    }
    lines
}

/// Replace one completed call's live start/preview range with its timed row,
/// retaining that range plus the full result behind click-to-expand. Parallel
/// calls may finish in any order, so later live ranges are shifted in place.
fn collapse_tool_call(
    view: &mut ViewState,
    live_tools: &mut HashMap<String, LiveToolBlock>,
    call_id: &str,
    fallback_tool: &str,
    result: &ToolResultBody,
    finished: Instant,
) {
    let succeeded = matches!(result, ToolResultBody::Ok { .. });
    let Some(block) = live_tools.remove(call_id) else {
        // A lagged event stream can drop ToolStart while retaining ToolEnd.
        // A successful end needs no trace; a failed end remains inspectable.
        if succeeded {
            return;
        }
        let elapsed = Duration::ZERO;
        view.push_tool_block(
            tool_summary_line(fallback_tool, result, elapsed, false),
            tool_summary_line(fallback_tool, result, elapsed, true),
            tool_result_detail_lines(result),
        );
        return;
    };

    let end = block.start.saturating_add(block.len);
    if block.len == 0 || end > view.transcript.len() {
        // Defensive fallback for corrupted bookkeeping: never panic the TUI.
        if !succeeded {
            let elapsed = finished.saturating_duration_since(block.started);
            view.push_tool_block(
                tool_summary_line(&block.tool, result, elapsed, false),
                tool_summary_line(&block.tool, result, elapsed, true),
                tool_result_detail_lines(result),
            );
        }
        return;
    }

    // A successful call has served its purpose. Remove its start line and any
    // live Edit preview instead of leaving even a compact transcript row.
    if succeeded {
        let removed = view.remove_transcript_range(block.start, block.len);
        if removed > 0 {
            for other in live_tools.values_mut() {
                if other.start >= end {
                    other.start = other.start.saturating_sub(removed);
                }
            }
        }
        return;
    }

    let elapsed = finished.saturating_duration_since(block.started);
    let collapsed = tool_summary_line(&block.tool, result, elapsed, false);
    let expanded = tool_summary_line(&block.tool, result, elapsed, true);
    let mut body = view.transcript[block.start..end].to_vec();
    body.extend(tool_result_detail_lines(result));
    let removed = view.replace_with_tool_block(block.start, block.len, collapsed, expanded, body);
    if removed > 0 {
        for other in live_tools.values_mut() {
            if other.start >= end {
                other.start = other.start.saturating_sub(removed);
            }
        }
    }
}

/// A terminal turn should never leave expanded live tool details behind. The
/// runtime normally emits ToolEnd for every ToolStart; this cleanup covers a
/// lagged stream or abrupt error by compacting any survivors as interrupted.
fn collapse_unfinished_tool_calls(
    view: &mut ViewState,
    live_tools: &mut HashMap<String, LiveToolBlock>,
    finished: Instant,
) {
    let mut ids: Vec<String> = live_tools.keys().cloned().collect();
    ids.sort_by_key(|id| std::cmp::Reverse(live_tools.get(id).map_or(0, |block| block.start)));
    for id in ids {
        collapse_tool_call(
            view,
            live_tools,
            &id,
            "tool",
            &ToolResultBody::Interrupted,
            finished,
        );
    }
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
    theme::palette().chrome()
}

/// A scannable model-error block: a red `✗` glyph + the message, mirroring the
/// tool-error treatment so failures read uniformly. Short single-line errors
/// ride inline on the header; longer/multi-line messages get a dim `│`-guttered
/// body capped at a few lines, so a verbose provider error is summarized, not
/// dumped into the transcript.
fn error_block(e: &str) -> Vec<Line<'static>> {
    let p = theme::palette();
    let red = p.semantic(Color::Red);
    let mut lines = Vec::new();
    if !e.contains('\n') && e.chars().count() <= 120 {
        lines.push(Line::from(vec![
            Span::styled("✗ ".to_string(), red),
            Span::styled(e.to_string(), p.chrome()),
        ]));
    } else {
        lines.push(Line::styled("✗ error".to_string(), red));
        let mut more = false;
        for (i, l) in e.lines().enumerate() {
            if i >= 5 {
                more = true;
                break;
            }
            lines.push(Line::styled(format!("│ {}", truncate(l, 120)), p.chrome()));
        }
        if more {
            lines.push(Line::styled("│ …".to_string(), p.chrome()));
        }
    }
    lines
}

/// Delete the last whitespace-delimited word from the end of `s` (Ctrl+W /
/// Alt+Backspace). Char-safe — works on `&str` boundaries, never mid-multibyte.
fn delete_last_word(s: &mut String) {
    let trimmed = s.trim_end();
    // Keep up to the separator before the last word; a single word clears all.
    let cut: usize = trimmed.rfind(char::is_whitespace).unwrap_or_default();
    s.truncate(cut);
}

/// Push `entry` onto the prompt history, collapsing a consecutive duplicate
/// (so re-submitting the same prompt N times doesn't flood the recall buffer)
/// and skipping blank/whitespace-only entries. Capped at [`MAX_HISTORY`] by
/// dropping the oldest entries.
fn push_history(history: &mut Vec<String>, entry: String) {
    if entry.trim().is_empty() {
        return;
    }
    if history.last().is_some_and(|h| h == &entry) {
        return;
    }
    history.push(entry);
    if history.len() > MAX_HISTORY {
        let drop = history.len() - MAX_HISTORY;
        history.drain(..drop);
    }
}

/// The most prompts retained (in memory and on disk). Large enough for a long
/// working session's recall, small enough to keep the file and the recaller
/// bounded.
const MAX_HISTORY: usize = 2000;

/// The prompt-history file: `~/.sc/history.txt` — the same `~/.sc` config dir
/// `sc` already uses. `None` when `$HOME` is unset.
fn sc_history_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".sc").join("history.txt"))
}

/// `~/.sc/sessions` — where `/menu` reads the project/session listing from.
/// Mirrors `rc-cli`'s `sessions_dir()`; a missing `HOME` yields a path that
/// simply lists empty rather than failing the menu open.
fn sessions_dir_for_menu() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".sc")
        .join("sessions")
}

/// Best-effort load of prompt history from `path` (one prompt per line),
/// capped to the most recent [`MAX_HISTORY`] entries. Missing/unreadable →
/// empty. Consecutive duplicates collapse (matching [`push_history`]), so a
/// hand-edited or appended file still recalls cleanly.
fn load_history(path: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if out.last().is_some_and(|h| h == line) {
            continue;
        }
        out.push(line.to_string());
    }
    if out.len() > MAX_HISTORY {
        out.drain(..out.len() - MAX_HISTORY);
    }
    out
}

/// Best-effort append of one prompt to the history file, creating `~/.sc` if
/// it doesn't exist. Failures are ignored — history is a convenience, never a
/// load on the turn.
fn append_history(path: &Path, line: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Pure decision for Alt+↑/↓ prompt-history recall, factored out so it's
/// testable without a `Runtime`. `pos` is the current history index (`None` =
/// the live draft). `draft` is the in-progress text stashed when history was
/// entered, restored on return to live. `older = true` steps toward the oldest
/// entry. Returns the next `(pos, composer_text)`.
fn browse_history(
    history: &[String],
    pos: Option<usize>,
    draft: &str,
    older: bool,
) -> (Option<usize>, String) {
    if history.is_empty() {
        return (pos, draft.to_string());
    }
    let len = history.len();
    let (next, text) = match (pos, older) {
        // Entering history from the live draft → jump to the newest (last).
        (None, true) => (Some(len - 1), history[len - 1].clone()),
        // Already live and pressing newer → stay on the draft (no-op).
        (None, false) => (None, draft.to_string()),
        // Browsing older; saturating sub clamps at the first entry.
        (Some(i), true) => {
            let j = i.saturating_sub(1);
            (Some(j), history[j].clone())
        }
        // Browsing newer; past the newest → back to the stashed draft.
        (Some(i), false) => {
            if i + 1 >= len {
                (None, draft.to_string())
            } else {
                (Some(i + 1), history[i + 1].clone())
            }
        }
    };
    (next, text)
}

/// Char-safe truncation with an ellipsis. Bounded by the output size, not the
/// input's — a 12 MB single-line tool result costs the first `n` chars, not a
/// full scan.
pub(crate) fn truncate(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for (count, c) in s.chars().enumerate() {
        if count >= n {
            truncated = true;
            break;
        }
        out.push(c);
    }
    if truncated {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_word_recognizes_exit_and_quit_only() {
        // Shell-style bare words quit; anything else falls through to the model.
        assert!(is_quit_word("exit"));
        assert!(is_quit_word("quit"));
        assert!(is_quit_word("  exit  "));
        assert!(!is_quit_word("exit code"), "phrase -> model");
        assert!(!is_quit_word("exit?"), "with punctuation -> model");
        assert!(!is_quit_word("exits"), "plural -> model");
        assert!(!is_quit_word("how do I exit"), "sentence -> model");
        assert!(!is_quit_word("/exit"), "slash form handled by handle_slash");
        assert!(!is_quit_word(""), "empty -> no-op");
    }

    #[test]
    fn cycle_mode_rotates_through_every_mode() {
        // Ordered least-permissive to most, wrapping: every mode is reachable
        // by pressing Shift+Tab, and none is stranded off the cycle.
        assert_eq!(cycle_mode(AgentMode::Ask), AgentMode::Default);
        assert_eq!(cycle_mode(AgentMode::Default), AgentMode::AcceptEdits);
        assert_eq!(cycle_mode(AgentMode::AcceptEdits), AgentMode::Plan);
        assert_eq!(cycle_mode(AgentMode::Plan), AgentMode::Auto);
        assert_eq!(cycle_mode(AgentMode::Auto), AgentMode::Ask);

        // Cycling from any mode returns to it in exactly one lap.
        let mut m = AgentMode::Default;
        for _ in 0..5 {
            m = cycle_mode(m);
        }
        assert_eq!(m, AgentMode::Default, "five steps is a full lap");
    }

    #[test]
    fn suggested_rule_for_bash_uses_first_token() {
        assert_eq!(
            suggested_rule("Bash", &serde_json::json!({"command": "cargo test --lib"})),
            "Bash(cargo:*)"
        );
        assert_eq!(
            suggested_rule("Edit", &serde_json::json!({"file_path": "/tmp/x"})),
            "Edit"
        );
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("héllo", 2), "hé..."); // 2 chars + ellipsis
    }

    #[test]
    fn resumed_history_restores_visible_turns_metrics_and_collapsed_details() {
        let turns = vec![
            Turn::User {
                content: "earlier prompt".into(),
                ts: std::time::SystemTime::UNIX_EPOCH,
            },
            Turn::Assistant {
                text: "earlier answer".into(),
                reasoning: Some("private restored thought".into()),
                calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "Bash".into(),
                    arguments: r#"{"command":"false"}"#.into(),
                }],
                usage: Some(rc_core::Usage {
                    prompt_tokens: 321,
                    completion_tokens: 9,
                    total_tokens: 330,
                    prompt_tokens_details: None,
                }),
                cost: None,
            },
            Turn::ToolResult {
                call_id: "call-1".into(),
                tool: "Bash".into(),
                result: ToolResultBody::Error {
                    message: "exit 1".into(),
                    retryable: false,
                },
                duration: Duration::from_millis(390),
            },
        ];
        let mut view = ViewState::new("m".into());
        restore_history(&mut view, &turns);
        let collapsed: String = view
            .transcript
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();

        assert!(collapsed.contains("earlier prompt"));
        assert!(collapsed.contains("earlier answer"));
        assert!(collapsed.contains("thought"));
        assert!(!collapsed.contains("private restored thought"));
        assert!(collapsed.contains("Bash · 0.39s · failed"));
        assert_eq!(view.context_tokens, Some(321));
        assert!(!view.context_tokens_estimated);

        view.toggle_latest_reasoning()
            .expect("restored thought remains expandable");
        let expanded: String = view
            .transcript
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(expanded.contains("private restored thought"));
    }

    #[test]
    fn delete_last_word_drops_trailing_word() {
        let mut s = String::from("fix the bug");
        delete_last_word(&mut s);
        assert_eq!(s, "fix the");
        delete_last_word(&mut s);
        assert_eq!(s, "fix");
        delete_last_word(&mut s);
        assert_eq!(s, "");
        // A single word with no separator clears entirely.
        let mut s = String::from("solo");
        delete_last_word(&mut s);
        assert_eq!(s, "");
    }

    #[test]
    fn delete_last_word_is_char_safe() {
        // Multibyte boundary: deleting must not split a char.
        let mut s = String::from("café au lait");
        delete_last_word(&mut s);
        assert_eq!(s, "café au");
        delete_last_word(&mut s);
        assert_eq!(s, "café");
        delete_last_word(&mut s);
        assert_eq!(s, "");
    }

    #[test]
    fn delete_last_word_trims_trailing_whitespace() {
        let mut s = String::from("foo bar   ");
        delete_last_word(&mut s);
        assert_eq!(s, "foo");
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
    fn user_prompt_echo_is_neutral_and_has_no_brand_logo() {
        let line = user_prompt_line("hello");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "> hello");
        assert!(
            !text.contains(theme::DEFAULT_LOGO),
            "logo is reserved for output: {text}"
        );
    }

    #[test]
    fn successful_tool_disappears_when_it_finishes() {
        let started = Instant::now();
        let mut view = ViewState::new("m".into());
        view.transcript = vec![
            Line::from("before"),
            Line::from("▸ Read  README.md"),
            Line::from("after"),
        ];
        let mut live_tools = HashMap::from([(
            "read-1".into(),
            LiveToolBlock {
                start: 1,
                len: 1,
                tool: "Read".into(),
                started,
            },
        )]);

        collapse_tool_call(
            &mut view,
            &mut live_tools,
            "read-1",
            "Read",
            &ToolResultBody::Ok {
                content: "line one\nline two\nline three".into(),
                truncated: false,
            },
            started + Duration::from_millis(390),
        );

        let lines: Vec<String> = view
            .transcript
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(lines, ["before", "after"]);
        assert!(live_tools.is_empty());
    }

    #[test]
    fn collapsed_failure_keeps_only_compact_status() {
        let line = tool_summary_line(
            "Bash",
            &ToolResultBody::Error {
                message: "tests failed\nfull stack trace".into(),
                retryable: false,
            },
            Duration::from_millis(120),
            false,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "▸ Bash · 0.12s · failed");
        assert!(
            !text.contains("cargo test"),
            "arguments stay hidden: {text}"
        );
        assert!(
            !text.contains("tests failed"),
            "error stays behind expansion: {text}"
        );
    }

    #[test]
    fn failed_tool_remains_as_a_timed_expandable_row() {
        let started = Instant::now();
        let mut view = ViewState::new("m".into());
        view.transcript = vec![Line::from("▸ Bash  cargo test")];
        let mut live_tools = HashMap::from([(
            "bash-1".into(),
            LiveToolBlock {
                start: 0,
                len: 1,
                tool: "Bash".into(),
                started,
            },
        )]);

        collapse_tool_call(
            &mut view,
            &mut live_tools,
            "bash-1",
            "Bash",
            &ToolResultBody::Error {
                message: "tests failed".into(),
                retryable: false,
            },
            started + Duration::from_millis(390),
        );

        let text: String = view.transcript[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(view.transcript.len(), 1);
        assert_eq!(text, "▸ Bash · 0.39s · failed");
    }

    #[test]
    fn tool_summary_status_by_kind() {
        let text = |result: &ToolResultBody| -> String {
            tool_summary_line("T", result, Duration::ZERO, false)
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert_eq!(
            text(&ToolResultBody::Ok {
                content: "".into(),
                truncated: false
            }),
            "▸ T · 0.00s"
        );
        assert!(text(&ToolResultBody::Error {
            message: "boom".into(),
            retryable: false
        })
        .ends_with("failed"));
        assert!(text(&ToolResultBody::Denied {
            reason: "no".into()
        })
        .ends_with("denied"));
        assert!(text(&ToolResultBody::Interrupted).ends_with("interrupted"));
    }

    #[test]
    fn expanded_tool_result_keeps_every_output_line() {
        let lines = tool_result_detail_lines(&ToolResultBody::Ok {
            content: "one\ntwo\nthree\nfour".into(),
            truncated: false,
        });
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            text,
            ["  ✓ output", "  │ one", "  │ two", "  │ three", "  │ four"]
        );
    }

    #[test]
    fn successful_parallel_tools_disappear_and_reindex_survivors() {
        let started = Instant::now();
        let mut view = ViewState::new("m".into());
        view.transcript = vec![
            Line::from("before"),
            Line::from("▸ Edit file.rs"),
            Line::from("  edit preview"),
            Line::from("▸ Bash cargo test"),
            Line::from("after"),
        ];
        let mut live_tools = HashMap::from([
            (
                "edit-1".into(),
                LiveToolBlock {
                    start: 1,
                    len: 2,
                    tool: "Edit".into(),
                    started,
                },
            ),
            (
                "bash-1".into(),
                LiveToolBlock {
                    start: 3,
                    len: 1,
                    tool: "Bash".into(),
                    started,
                },
            ),
        ]);

        collapse_tool_call(
            &mut view,
            &mut live_tools,
            "edit-1",
            "Edit",
            &ToolResultBody::Ok {
                content: "done".into(),
                truncated: false,
            },
            started + Duration::from_millis(390),
        );
        assert_eq!(
            view.transcript.len(),
            3,
            "the successful edit start and preview both disappear"
        );
        assert_eq!(
            live_tools["bash-1"].start, 1,
            "later parallel block is reindexed"
        );

        collapse_tool_call(
            &mut view,
            &mut live_tools,
            "bash-1",
            "Bash",
            &ToolResultBody::Ok {
                content: "all green".into(),
                truncated: false,
            },
            started + Duration::from_millis(420),
        );
        assert!(live_tools.is_empty());
        let lines: Vec<String> = view
            .transcript
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(lines, ["before", "after"]);
    }

    #[test]
    fn error_block_inlines_short_message() {
        let lines = error_block("model rate-limited");
        assert_eq!(lines.len(), 1, "short error inlines: {lines:?}");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("✗"), "red error glyph: {text}");
        assert!(
            text.contains("model rate-limited"),
            "includes the message: {text}"
        );
    }

    #[test]
    fn error_block_gutters_a_long_message() {
        let e = "line one\nline two\nline three\nline four\nline five\nline six\nline seven";
        let lines = error_block(e);
        // Header + 5 body lines + ellipsis.
        assert_eq!(lines.len(), 7, "header + 5 lines + …: {lines:?}");
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            header.contains("✗ error"),
            "header is the glyph + error: {header}"
        );
        assert!(
            lines[6].spans.iter().any(|s| s.content.contains("…")),
            "trailing ellipsis: {lines:?}"
        );
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

    #[test]
    fn esc_action_never_loses_a_draft() {
        // The safety ordering: a drafted prompt is never lost to a stray Esc.
        let mut s = ViewState::new("m".into());

        // Idle + empty → quit.
        assert_eq!(esc_action(&s), EscAction::Quit);

        // Idle + draft → clear (not quit).
        s.composer = "half-typed prompt".into();
        assert_eq!(esc_action(&s), EscAction::Clear);

        // Busy always cancels the turn, even with a draft present.
        s.busy = true;
        assert_eq!(esc_action(&s), EscAction::Cancel);
        s.busy = false;

        // Browsing history → restore the stashed draft, not clear.
        s.history_pos = Some(0);
        s.history_draft = "my draft".into();
        s.composer = "an old prompt".into();
        assert_eq!(esc_action(&s), EscAction::RestoreDraft);

        // Busy wins over history browsing too.
        s.busy = true;
        assert_eq!(esc_action(&s), EscAction::Cancel);
    }

    #[test]
    fn push_history_dedups_consecutive_and_skips_blank() {
        let mut h = Vec::new();
        push_history(&mut h, "a".into());
        push_history(&mut h, "b".into());
        push_history(&mut h, "b".into()); // consecutive duplicate → dropped
        push_history(&mut h, "  ".into()); // blank → dropped
        push_history(&mut h, "a".into()); // non-consecutive repeat → kept
        assert_eq!(h, vec!["a", "b", "a"]);
    }

    #[test]
    fn browse_history_walks_and_restores_draft() {
        let hist = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];
        // From the live draft, older → jump to the newest (last) entry.
        let (pos, text) = browse_history(&hist, None, "draft", true);
        assert_eq!(pos, Some(2));
        assert_eq!(text, "third");
        // Older again → the middle entry.
        let (pos, text) = browse_history(&hist, pos, "draft", true);
        assert_eq!(pos, Some(1));
        assert_eq!(text, "second");
        // Older clamps at the oldest (stays on the first).
        let (pos, text) = browse_history(&hist, Some(0), "draft", true);
        assert_eq!(pos, Some(0));
        assert_eq!(text, "first");
        // Newer from the middle → the newest.
        let (pos, text) = browse_history(&hist, Some(1), "draft", false);
        assert_eq!(pos, Some(2));
        assert_eq!(text, "third");
        // Newer past the newest → back to the stashed live draft.
        let (pos, text) = browse_history(&hist, Some(2), "my draft", false);
        assert_eq!(pos, None);
        assert_eq!(text, "my draft");
        // Newer while already live is a no-op.
        let (pos, text) = browse_history(&hist, None, "draft", false);
        assert_eq!(pos, None);
        assert_eq!(text, "draft");
        // Empty history is a no-op (can't browse what isn't there).
        let empty: Vec<String> = Vec::new();
        let (pos, text) = browse_history(&empty, None, "draft", true);
        assert_eq!(pos, None);
        assert_eq!(text, "draft");
    }

    #[test]
    fn history_round_trips_through_disk() {
        // Unique path per test process so parallel runs don't collide.
        let path = std::env::temp_dir().join(format!("sc-history-test-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path); // clean slate

        append_history(&path, "first prompt");
        append_history(&path, "second prompt");
        append_history(&path, "second prompt"); // consecutive dup written to disk
        let loaded = load_history(&path);
        // load collapses consecutive dups → recall is clean.
        assert_eq!(loaded, vec!["first prompt", "second prompt"]);

        // A missing file loads as empty (never errors).
        let missing = std::env::temp_dir().join("sc-history-nonexistent-xyz.txt");
        let _ = std::fs::remove_file(&missing);
        assert!(load_history(&missing).is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
