//! App state, the poll loop, and the keymap. The loop: drain pending events
//! into state, render, poll crossterm for up to one frame, translate a key into
//! a [`rc_rt::UserAction`] (or a local effect), repeat until quit.
//!
//! Rendering reads only [`ViewState`], which is cheap to build in a test — the
//! `Runtime`/`EventStream` are kept separate so a `ratatui::backend::TestBackend`
//! render test needs no tokio and no model.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use rc_core::{AgentMode, Artifact, AskResponse, ToolCall, ToolResultBody, Turn};
use rc_rt::{AgentEvent, EventStream, Runtime, UserAction};
use serde_json::Value;

use crate::complete::{self, Completion};
use crate::diff;
use crate::theme;
use crate::view::Selection;
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
/// A normal human cannot type the first character of the next prompt this
/// quickly after Enter, but a terminal that has lost bracketed-paste framing
/// can. Hold plain submissions briefly so an Enter-delimited paste can be
/// reconstructed as one multiline prompt before anything reaches the runtime.
const PASTE_RESCUE_WINDOW: Duration = Duration::from_millis(20);

#[derive(Debug)]
struct PendingSubmit {
    text: String,
    staged: Instant,
}

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
    /// Plain prompts wait one tiny debounce window before dispatch. This is
    /// the fallback for terminals/multiplexers that turn a multiline paste
    /// into a burst of Char/Enter events instead of one bracketed Paste event.
    pending_submit: Option<PendingSubmit>,
    /// Prompts accepted by the runtime queue. They render only when `Ready`
    /// follows the previous turn's terminal boundary.
    queued_prompts: VecDeque<String>,
    /// Esc with a queued prompt waits for the current parallel tool batch to
    /// finish, then cancels the turn so the runtime starts that prompt.
    send_queued_after_tool: bool,
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
            pending_submit: None,
            queued_prompts: VecDeque::new(),
            send_queued_after_tool: false,
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
    mouse: bool,
) -> anyhow::Result<Option<crate::menu::Outcome>> {
    let mut app = App::new(runtime, model_name, cwd, history);
    // The host already emitted the capture sequence (or didn't); this keeps
    // the flag, the hint and Ctrl+O agreeing with the terminal's actual state.
    app.view.mouse_capture = mouse;
    app.view.location = describe_location(&app.cwd);
    // The mode the host resolved — a resumed session's own mode, or the
    // configured default. Set before the first draw so the status bar agrees
    // with the engine from frame one instead of claiming "default" until
    // something happens to change it.
    app.view.mode = initial_mode;
    loop {
        app.drain_events();
        app.flush_pending_submit_if_due();
        terminal.draw(|f| view::draw(f, &mut app.view))?;
        // After the draw, never before: `view::draw` is what harvests the
        // selected text out of the finished buffer.
        app.flush_copy();
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
    app.runtime.shutdown_blocking(Duration::from_secs(5));
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
        // A queued prompt belongs in transcript/history at its real turn
        // boundary, not when Tab is pressed in the middle of the prior answer.
        if matches!(&ev, AgentEvent::Ready) && !self.view.busy {
            if let Some(prompt) = self.queued_prompts.pop_front() {
                self.record_prompt(&prompt);
                self.begin_turn_display();
                self.view.queued_messages = self.queued_prompts.len();
                self.send_queued_after_tool = false;
                self.view.queued_after_tool = false;
            }
        }

        let mut cancel_after_tool = false;
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
                    if self.send_queued_after_tool {
                        self.send_queued_after_tool = false;
                        v.queued_after_tool = false;
                        cancel_after_tool = true;
                    }
                }
            }
            AgentEvent::Artifact {
                call_id: _,
                tool: _,
                artifact,
            } => {
                v.flush_text();
                show_artifact(v, &self.cwd, artifact);
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
            AgentEvent::Retry { retries } => {
                v.flush_text();
                v.transcript.push(retry_notice_line(retries));
            }
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
                finish_turn(v);
            }
            AgentEvent::Error(e) => {
                v.flush_text();
                collapse_unfinished_tool_calls(v, &mut self.live_tools, Instant::now());
                v.transcript.extend(error_block(&e));
                finish_turn(v);
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
                finish_turn(v);
            }
        }
        if cancel_after_tool {
            self.runtime.action(UserAction::Cancel);
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
                    // A saved API key can only take effect on a rebuilt
                    // client, so the commit asks to leave and come back.
                    if let Some(outcome) = menu.pending_outcome.take() {
                        self.leave_with(outcome);
                    }
                }
                KeyCode::Esc => {
                    menu.editing = None;
                    menu.editing_api_key = false;
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
                menu.cycle_current(1, &cwd);
                let outcome = menu.pending_outcome.take();
                if let Some(outcome) = outcome {
                    self.leave_with(outcome);
                }
            }
            KeyCode::Left if menu.page == crate::menu::MenuPage::Settings => {
                menu.cycle_current(-1, &cwd);
                let outcome = menu.pending_outcome.take();
                if let Some(outcome) = outcome {
                    self.leave_with(outcome);
                }
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
            crate::menu::Row::ChangeApiKey => menu.begin_api_key_edit(),
            crate::menu::Row::Close => self.view.menu_overlay = None,
            // Both of these leave the TUI: the host rebuilds the agent for the
            // new session/directory and runs a fresh TUI over it.
            crate::menu::Row::Session(path) => self.leave_with(crate::menu::Outcome::Resume(path)),
            crate::menu::Row::NewSession(dir) => self.leave_with(crate::menu::Outcome::NewIn(dir)),
        }
    }

    /// Leave the TUI with something for the host to do — switch sessions, or
    /// rebuild this one. Everything cwd- or key-scoped is constructed above
    /// this crate, so the only way to change it is to hand back an outcome and
    /// let `main` run a fresh TUI over the rebuilt agent.
    fn leave_with(&mut self, outcome: crate::menu::Outcome) {
        self.outcome = Some(outcome);
        self.runtime.action(UserAction::Quit);
        self.quit = true;
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Typing ends a selection: the highlight would otherwise sit over text
        // that has scrolled or changed underneath it. The copy already
        // happened on mouse-up, so nothing is lost.
        self.clear_selection();
        // Ctrl+C always quits, even mid-ask / mid-turn.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.runtime.action(UserAction::Quit);
            self.quit = true;
            return;
        }

        // If a terminal emitted an unbracketed multiline paste, the character
        // or Enter immediately following a staged submit belongs to the same
        // document. Restore the staged line to the composer before handling
        // this key. A slower key flushes the real submit first and begins an
        // ordinary draft for the next turn.
        let paste_continuation = key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
            && matches!(key.code, KeyCode::Char(_) | KeyCode::Enter);
        self.resolve_pending_submit(paste_continuation);

        // `/menu` is modal: while it's open it consumes every key, so nothing
        // reaches the composer behind it.
        if self.view.menu_overlay.is_some() {
            self.handle_menu_key(key);
            return;
        }

        // While an ask is open, only the answer keys are live; Enter is a no-op.
        if let Some(ask) = self.view.pending_ask.take() {
            if key.code == KeyCode::Esc {
                match esc_action(&self.view) {
                    EscAction::QueueAfterTool => {
                        self.arm_queued_after_tool();
                        // Denial completes this tool call, whose ToolEnd is the
                        // handoff boundary that starts the queued prompt.
                        self.runtime.action(UserAction::PermissionAnswer {
                            id: ask.id,
                            response: AskResponse::Deny("declined".into()),
                        });
                    }
                    EscAction::Cancel => self.cancel_active_turn(),
                    _ => self.view.pending_ask = Some(ask),
                }
                return;
            }
            let response = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(AskResponse::Once),
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    Some(AskResponse::Session(suggested_rule(&ask.tool, &ask.input)))
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Some(AskResponse::Always(suggested_rule(&ask.tool, &ask.input)))
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
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
                if self.view.busy {
                    if !self.view.composer.is_empty() {
                        self.view.transcript.push(Line::styled(
                            "· turn still running; draft kept in the composer",
                            dim_style(),
                        ));
                        self.jump_to_bottom();
                    }
                    return;
                }
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
                        self.pending_submit = Some(PendingSubmit {
                            text,
                            staged: Instant::now(),
                        });
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
                EscAction::QueueAfterTool => self.arm_queued_after_tool(),
                EscAction::Cancel => self.cancel_active_turn(),
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
            KeyCode::Tab if self.view.busy => self.queue_composer(),
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
                'o' | 'O' => self.toggle_mouse_capture(),
                'u' | 'U' => self.clear_composer_line(),
                _ => {} // other Ctrl+letter combos: ignore, don't insert
            },
            // Cmd+Backspace is the macOS "delete to start of line" gesture. Only
            // terminals that report the Super modifier (kitty protocol: Ghostty,
            // WezTerm, kitty; iTerm2 when configured) deliver it — Terminal.app
            // swallows Cmd entirely, so Ctrl+U remains the portable spelling.
            KeyCode::Backspace
                if key
                    .modifiers
                    .intersects(KeyModifiers::SUPER | KeyModifiers::META) =>
            {
                self.clear_composer_line()
            }
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
        // A native paste can also be the continuation of a staged first line
        // if the terminal only partially preserved bracketed-paste framing.
        self.resolve_pending_submit(true);

        // Modal editors own their input (notably the API-key field). A menu
        // with no editor cannot accept the payload, so close it and preserve
        // the text in the composer instead of dropping it behind the overlay.
        if let Some(menu) = self.view.menu_overlay.as_mut() {
            if menu.paste(text) {
                return;
            }
            self.view.menu_overlay = None;
            self.view.transcript.push(Line::styled(
                "· paste saved in the composer; menu closed",
                dim_style(),
            ));
        }
        if self.view.pending_ask.is_some() {
            self.view.transcript.push(Line::styled(
                "· paste saved in the composer; answer the permission prompt to continue",
                dim_style(),
            ));
        }
        if self.view.append_paste(text) == 0 {
            return;
        }
        self.view.history_pos = None;
        self.view.last_input = Some(Instant::now());
        self.refresh_menu();
    }

    /// Resolve the tiny pre-submit debounce. `continuation` means the caller
    /// is handling input that can belong to the same unbracketed paste; within
    /// the rescue window the staged line is restored with its newline. Any
    /// other input (or elapsed window) dispatches the staged prompt normally.
    fn resolve_pending_submit(&mut self, continuation: bool) {
        let Some(prompt) = resolve_staged_submit(
            &mut self.pending_submit,
            &mut self.view.composer,
            Instant::now(),
            continuation,
        ) else {
            return;
        };
        self.submit_prompt(prompt);
    }

    fn flush_pending_submit_if_due(&mut self) {
        let due = self
            .pending_submit
            .as_ref()
            .is_some_and(|pending| pending.staged.elapsed() >= PASTE_RESCUE_WINDOW);
        if due {
            self.resolve_pending_submit(false);
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
            // Scrolling moves the text out from under a highlight, so the
            // selection can't survive it.
            MouseEventKind::ScrollUp => {
                self.clear_selection();
                self.scroll_by(-WHEEL_LINES)
            }
            MouseEventKind::ScrollDown => {
                self.clear_selection();
                self.scroll_by(WHEEL_LINES)
            }
            // A press no longer acts immediately: not until the button comes
            // up do we know whether this was a click (toggle a block) or a
            // drag (select text).
            MouseEventKind::Down(MouseButton::Left) => {
                self.view.copy_notice = None;
                self.view.selection = Some(Selection {
                    anchor: (ev.column, ev.row),
                    head: (ev.column, ev.row),
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(sel) = self.view.selection.as_mut() {
                    sel.head = (ev.column, ev.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => match self.view.selection {
                // Dragged: copy on release, with no extra keystroke — the
                // selection *is* the copy gesture.
                Some(sel) if !sel.is_empty() => self.view.copy_pending = true,
                // Never moved: a plain click, which keeps its old meaning.
                _ => {
                    self.clear_selection();
                    self.toggle_expandable_at(ev.column, ev.row);
                }
            },
            _ => {}
        }
    }

    /// Drop any selection and the text harvested for it.
    fn clear_selection(&mut self) {
        self.view.selection = None;
        self.view.selection_text = None;
    }

    /// Copy a finished drag to the clipboard. Called right after a draw,
    /// because the text is read out of the rendered buffer and only exists
    /// once the frame has been painted.
    fn flush_copy(&mut self) {
        if !self.view.copy_pending {
            return;
        }
        self.view.copy_pending = false;
        let Some(text) = self.view.selection_text.clone() else {
            return;
        };
        if copy_to_clipboard(&text).is_ok() {
            let n = text.chars().count();
            self.view.copy_notice = Some((format!("copied {n} chars"), Instant::now()));
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

    /// Wipe the composer line: Ctrl+U, or Cmd+Backspace where the terminal
    /// reports it.
    fn clear_composer_line(&mut self) {
        self.view.composer.clear();
        self.view.clear_paste_markers();
        self.view.last_input = Some(Instant::now());
        self.refresh_menu();
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

    /// A left click on a visible thought or completed-tool label toggles that
    /// exact retained block. Thoughts preserve the viewport's existing top so
    /// their summary stays on the same screen row and the inserted body grows
    /// downward. Completed tools keep their existing top-anchor behavior.
    fn toggle_expandable_at(&mut self, column: u16, row: u16) {
        if !self.live_tools.is_empty() {
            return;
        }
        let top_before = self.current_top(self.total_lines());
        if toggle_reasoning_preserving_top(&mut self.view, column, row, top_before) {
            return;
        }
        let Some(toggle) = self.view.toggle_tool_at(column, row) else {
            return;
        };
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

    /// Echo a prompt at its actual turn boundary and retain it for history.
    fn record_prompt(&mut self, text: &str) {
        self.view.transcript.push(user_prompt_line(text));
        // Record the prompt for Alt+↑/↓ recall (deduped, bash-style), and leave
        // history-browsing mode — a fresh submit always returns to the live
        // draft.
        push_history(&mut self.view.prompt_history, text.to_owned());
        self.view.history_pos = None;
        self.view.history_draft.clear();
        // Persist the prompt for cross-session recall. Best-effort — a failed
        // append never blocks the turn.
        if let Some(p) = sc_history_path() {
            append_history(&p, text);
        }
    }

    /// Optimistically mark a turn in flight so the spinner appears without
    /// waiting for the driver's `Ready` event.
    fn begin_turn_display(&mut self) {
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
    }

    /// Submit `text` to the model as a normal user turn: echo the prompt line,
    /// mark the turn in flight, and dispatch `UserAction::Submit`.
    fn submit_prompt(&mut self, text: String) {
        if !self.runtime.try_action(UserAction::Submit(text.clone())) {
            return;
        }
        self.record_prompt(&text);
        self.begin_turn_display();
    }

    /// Queue the current draft behind the running turn. The runtime owns the
    /// execution queue; this mirror exists only to render each prompt when its
    /// turn starts rather than interleaving it with an active answer.
    fn queue_composer(&mut self) {
        if self.view.composer.is_empty() {
            return;
        }
        let text = self.view.composer.clone();
        if !self.runtime.try_action(UserAction::Queue(text.clone())) {
            return;
        }
        self.view.composer.clear();
        self.view.clear_paste_markers();
        self.view.history_pos = None;
        self.view.history_draft.clear();
        self.queued_prompts.push_back(text);
        self.view.queued_messages = self.queued_prompts.len();
        let count = self.view.queued_messages;
        let noun = if count == 1 { "message" } else { "messages" };
        self.view.transcript.push(Line::styled(
            format!("· queued {count} {noun} — Esc sends after the next tool call"),
            dim_style(),
        ));
        self.refresh_menu();
        self.jump_to_bottom();
    }

    fn arm_queued_after_tool(&mut self) {
        self.send_queued_after_tool = true;
        self.view.queued_after_tool = true;
        self.view.transcript.push(Line::styled(
            "· queued message will send after the current tool call".to_string(),
            dim_style(),
        ));
        self.jump_to_bottom();
    }

    fn cancel_active_turn(&mut self) {
        self.send_queued_after_tool = false;
        self.view.queued_after_tool = false;
        self.runtime.action(UserAction::Cancel);
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
    /// `submit_prompt` with a canned instruction. Every advertised command has
    /// a concrete action here; the registry test enforces that contract.
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
                    .push(mk("  Tab          queue a draft during an active turn"));
                self.view
                    .transcript
                    .push(mk("  PgUp/PgDn    scroll the transcript"));
                self.view
                    .transcript
                    .push(mk("  Alt+↑/↓      recall prompt history"));
                self.view.transcript.push(mk(
                    "  drag         select text with your terminal, then copy",
                ));
                self.view.transcript.push(mk(
                    "  Ctrl+O       toggle wheel scrolling / native selection",
                ));
                self.view
                    .transcript
                    .push(mk("  Ctrl+W / U   delete word / clear the line"));
                self.view.transcript.push(mk(
                    "  Esc          stop · send queued after tool · clear · quit",
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
                reset_turn(&mut self.view);
            }
            SlashAction::Compact => {
                // Clear the rendered history immediately; the runtime appends
                // a durable summary marker and projects future requests from
                // that marker forward.
                self.view.clear_transcript();
                self.view.current_text.clear();
                self.live_tools.clear();
                reset_turn(&mut self.view);
                self.runtime.action(UserAction::Compact);
            }
            SlashAction::ShowGoal => self.runtime.action(UserAction::ShowGoal),
            SlashAction::SetGoal(goal) => {
                self.runtime.action(UserAction::SetGoal(Some(goal)));
            }
            SlashAction::ClearGoal => self.runtime.action(UserAction::SetGoal(None)),
            SlashAction::Loop(direction) => self.submit_prompt(loop_prompt(direction.as_deref())),
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
            SlashAction::SelectMode => self.toggle_mouse_capture(),
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
            SlashAction::Resume => {
                let mut menu = crate::menu::MenuState::new(&sessions_dir_for_menu(), &self.cwd);
                let page = if menu.project(&self.cwd).is_some() {
                    crate::menu::MenuPage::Sessions(self.cwd.clone())
                } else {
                    crate::menu::MenuPage::Projects
                };
                menu.goto(page);
                self.view.menu_overlay = Some(menu);
            }
            SlashAction::Login => {
                let mut menu = crate::menu::MenuState::new(&sessions_dir_for_menu(), &self.cwd);
                menu.begin_api_key_edit();
                self.view.menu_overlay = Some(menu);
            }
            SlashAction::Memory => self.push_info("memory", &memory_status_lines(&self.cwd)),
            SlashAction::TerminalSetup => self.push_info(
                "terminal setup",
                &[
                    "  bind Shift+Enter to send a newline / CSI-u sequence".into(),
                    "  sc accepts bracketed paste and ordinary Enter submits".into(),
                ],
            ),
            SlashAction::Prompt(text) => self.submit_prompt(text),
        }
    }

    /// Hand the mouse back to the terminal, or take it again.
    ///
    /// Mouse capture is what makes the wheel scroll the transcript, but it
    /// also means the terminal never sees a drag — so there is no way to
    /// select text, and copy/paste out of `sc` is impossible. Neither state is
    /// right all the time, so it is a toggle rather than a default.
    fn toggle_mouse_capture(&mut self) {
        self.view.mouse_capture = !self.view.mouse_capture;
        let mut out = std::io::stdout();
        // Best-effort: unsupported terminals simply ignore the sequence.
        let _ = if self.view.mouse_capture {
            execute!(out, EnableMouseCapture)
        } else {
            execute!(out, DisableMouseCapture)
        };
    }

    /// If `text` is a recognized slash command, return its host-side action.
    /// Returns `None` for anything else (including `@`-prefixed text), so the
    /// text is submitted as a normal prompt. Aliases (e.g. `/h`, `/c`, `/q`)
    /// map onto the same canonical action — they're recognized here but not
    /// advertised in the completion palette, so the menu stays uncluttered.
    fn handle_slash(&self, text: &str) -> Option<SlashAction> {
        classify_slash(text)
    }
}

/// Parse the command independently of `App` so completion/help can be checked
/// against the real dispatcher in tests. This is the single command-routing
/// table; no test shim is allowed to duplicate it.
fn classify_slash(text: &str) -> Option<SlashAction> {
    let t = text.trim();
    if t == "/goal" {
        return Some(SlashAction::ShowGoal);
    }
    if let Some(goal) = t.strip_prefix("/goal ") {
        let goal = goal.trim();
        if goal.is_empty() {
            return Some(SlashAction::ShowGoal);
        }
        if goal.eq_ignore_ascii_case("clear") {
            return Some(SlashAction::ClearGoal);
        }
        return Some(SlashAction::SetGoal(goal.to_string()));
    }
    if t == "/loop" {
        return Some(SlashAction::Loop(None));
    }
    if let Some(direction) = t.strip_prefix("/loop ") {
        let direction = direction.trim();
        return Some(SlashAction::Loop(
            (!direction.is_empty()).then(|| direction.to_string()),
        ));
    }
    if let Some(rest) = t.strip_prefix("/rewind") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Some(SlashAction::Rewind { steps: 1 });
        }
        let n = rest.parse::<usize>().ok()?;
        return (n > 0).then_some(SlashAction::Rewind { steps: n });
    }
    match t {
        "/clear" | "/c" | "/new" => Some(SlashAction::Clear),
        "/menu" | "/m" => Some(SlashAction::Menu),
        "/compact" | "/cc" => Some(SlashAction::Compact),
        "/context" => Some(SlashAction::Context),
        "/cost" | "/usage" => Some(SlashAction::Cost),
        "/status" | "/s" => Some(SlashAction::Status),
        "/model" => Some(SlashAction::Model),
        "/mode" => Some(SlashAction::CycleMode),
        "/permissions" | "/approval" => Some(SlashAction::Permissions),
        "/select" | "/mouse" => Some(SlashAction::SelectMode),
        "/doctor" => Some(SlashAction::Doctor),
        "/history" => Some(SlashAction::History),
        "/export" => Some(SlashAction::Export),
        "/quit" | "/exit" | "/q" => Some(SlashAction::Quit),
        "/resume" => Some(SlashAction::Resume),
        "/login" => Some(SlashAction::Login),
        "/memory" => Some(SlashAction::Memory),
        "/terminal-setup" => Some(SlashAction::TerminalSetup),
        "/review" => Some(SlashAction::Prompt(
            "Review the pending code changes on the current branch. Summarize the diff, flag risks and bugs, and suggest concrete fixes.".into(),
        )),
        "/pr" => Some(SlashAction::Prompt(
            "Create a pull request for the current branch: summarize the changes and generate a title and body.".into(),
        )),
        "/init" => Some(SlashAction::Prompt(
            "Initialize an AGENTS.md file documenting this codebase: structure, build/test commands, and conventions.".into(),
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
        "/docs" => Some(SlashAction::Prompt(
            "Read and summarize the relevant documentation.".into(),
        )),
        "/fix" => Some(SlashAction::Prompt(
            "Find and fix bugs in the relevant code.".into(),
        )),
        "/explain" => Some(SlashAction::Prompt("Explain the relevant code.".into())),
        "/edit" => Some(SlashAction::Prompt(
            "Apply the requested edits to the code.".into(),
        )),
        "/codebase" => Some(SlashAction::Prompt(
            "Summarize the structure of this codebase for context.".into(),
        )),
        "/help" | "/h" | "/?" => Some(SlashAction::Help),
        _ => None,
    }
}

fn loop_prompt(direction: Option<&str>) -> String {
    let mut prompt = "Continue working autonomously toward the active session goal. Inspect the current state, take the next useful actions, verify the result, and do not stop at a progress update while safe in-scope work remains.".to_string();
    if let Some(direction) = direction {
        prompt.push_str(" Additional direction: ");
        prompt.push_str(direction);
    }
    prompt
}

/// Report the exact memory-file locations used by `rc-ctx::Memory::load_chain`.
/// A file counts as loaded only when it is readable and non-blank, matching the
/// assembler rather than merely checking whether a path exists.
fn memory_status_lines(cwd: &Path) -> Vec<String> {
    let mut candidates = Vec::with_capacity(3);
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".sc").join("AGENTS.md"));
    }
    candidates.push(cwd.join(".sc").join("AGENTS.md"));
    candidates.push(cwd.join("AGENTS.md"));

    let mut lines: Vec<String> = candidates
        .into_iter()
        .filter(|path| {
            std::fs::read_to_string(path).is_ok_and(|contents| !contents.trim().is_empty())
        })
        .map(|path| format!("  loaded  {}", path.display()))
        .collect();
    if lines.is_empty() {
        lines.push("  no non-empty AGENTS.md files loaded".into());
    }
    lines
}

/// Toggle a clicked reasoning block while freezing the logical viewport top
/// from the frame the user clicked. The summary and every line above it keep
/// their screen position; only content at and below the expansion moves.
fn toggle_reasoning_preserving_top(
    view: &mut ViewState,
    column: u16,
    row: u16,
    top_before: usize,
) -> bool {
    if view.toggle_reasoning_at(column, row).is_none() {
        return false;
    }
    view.follow = false;
    view.scroll_top = top_before;
    true
}

/// A host-side slash command (not submitted to the model). Every canonical
/// command is advertised by `complete::slash_palette` and has a concrete
/// action here; aliases collapse onto the same variant in `classify_slash`.
/// `Prompt` carries a canned instruction submitted as a normal user turn.
#[derive(Debug, Clone)]
enum SlashAction {
    Help,
    /// Open the `/menu` modal (projects, sessions, settings).
    Menu,
    Clear,
    Compact,
    ShowGoal,
    SetGoal(String),
    ClearGoal,
    Loop(Option<String>),
    CycleMode,
    Rewind {
        steps: usize,
    },
    Cost,
    Context,
    Status,
    Model,
    Permissions,
    /// Toggle mouse capture so the terminal can select text (`/select`).
    SelectMode,
    Quit,
    Export,
    Doctor,
    History,
    Resume,
    Login,
    Memory,
    TerminalSetup,
    /// Submit a canned instruction to the model (prompt-expansion commands).
    Prompt(String),
}

/// Two completions are "the same menu" if they're the same kind and would show
/// the same candidates — used to preserve the selection across keystrokes.
fn same_menu(a: &Completion, b: &Completion) -> bool {
    a.kind == b.kind && a.candidates == b.candidates
}

/// Fold one staged submit back into the live composer when the next input
/// proves it was an unbracketed paste continuation. Otherwise return the
/// prompt for immediate dispatch. Kept runtime-free for regression tests.
fn resolve_staged_submit(
    pending: &mut Option<PendingSubmit>,
    composer: &mut String,
    now: Instant,
    continuation: bool,
) -> Option<String> {
    let staged = pending.take()?;
    if continuation && now.saturating_duration_since(staged.staged) <= PASTE_RESCUE_WINDOW {
        let suffix = std::mem::take(composer);
        *composer = staged.text;
        composer.push('\n');
        composer.push_str(&suffix);
        None
    } else {
        Some(staged.text)
    }
}

/// What `Esc` does in the current state. Factored out so the safety ordering
/// (never quit and lose a drafted prompt to a stray Esc) is testable with no
/// `Runtime`. Priority:
///
///   busy + queue  → QueueAfterTool (a second Esc cancels immediately)
///   busy          → Cancel the in-flight turn
///   browsing hist → RestoreDraft (return to the live draft, not clear it)
///   draft present → Clear the composer (a second Esc, now empty, quits)
///   otherwise     → Quit
///
/// The menu and ask handlers `return` before the keymap reaches `Esc`, so this
/// only fires on a bare Esc with neither overlay open.
fn esc_action(state: &crate::view::ViewState) -> EscAction {
    if state.busy && state.queued_messages > 0 && !state.queued_after_tool {
        EscAction::QueueAfterTool
    } else if state.busy {
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
    QueueAfterTool,
    Cancel,
    RestoreDraft,
    Clear,
    Quit,
}

/// Finish a real agent turn, preserving its submit-to-terminal-event wall time
/// as a divider in transcript history. Providers can emit both Outcome and
/// Idle; taking `turn_started` makes the divider exactly-once.
fn finish_turn(v: &mut crate::view::ViewState) {
    finish_turn_at(v, Instant::now());
}

fn finish_turn_at(v: &mut crate::view::ViewState, finished: Instant) {
    let elapsed = v
        .turn_started
        .take()
        .map(|started| finished.saturating_duration_since(started));
    let changed = v
        .turn_file_changes
        .values()
        .map(|(before, after)| diff::line_stats(before.as_deref(), after.as_deref()))
        .fold(diff::DiffStats::default(), |mut total, stats| {
            total.added = total.added.saturating_add(stats.added);
            total.removed = total.removed.saturating_add(stats.removed);
            total
        });
    reset_turn(v);

    if let Some(elapsed) = elapsed {
        if v.transcript.last().is_some_and(|line| line.width() > 0) {
            v.transcript.push(Line::default());
        }
        v.transcript.push(view::turn_divider_line(
            turn_duration_label(elapsed),
            changed.added,
            changed.removed,
        ));
    }
}

/// Reset per-turn animation state without writing transcript history. `/clear`
/// and `/compact` use this path because they intentionally remove the turn.
fn reset_turn(v: &mut crate::view::ViewState) {
    v.busy = false;
    v.turn_started = None;
    v.turn_file_changes.clear();
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

fn turn_duration_label(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(60) {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        let total = elapsed.as_secs();
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

fn show_artifact(v: &mut ViewState, cwd: &Path, artifact: Artifact) {
    match artifact {
        Artifact::FileChange {
            path,
            before,
            after,
        } => {
            let shown = path
                .strip_prefix(cwd)
                .unwrap_or(path.as_path())
                .display()
                .to_string();
            let (lines, _) = diff::file_diff_lines(&shown, before.as_deref(), after.as_deref());
            v.transcript.extend(lines);
            v.turn_file_changes
                .entry(path)
                .and_modify(|(_, final_after)| *final_after = after.clone())
                .or_insert((before, after));
        }
    }
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

/// A successful response can still have spent seconds behind transient
/// 429/5xx or connection failures. Keep that recovery in the transcript so it
/// does not masquerade as unexplained model thinking time.
fn retry_notice_line(retries: u32) -> Line<'static> {
    let noun = if retries == 1 { "retry" } else { "retries" };
    Line::styled(
        format!("↻ model request recovered after {retries} {noun}"),
        dim_style(),
    )
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

/// The echoed user turn keeps the composer's `>` direction marker inside a
/// padded grey bubble. The orange brand mark remains exclusive to assistant
/// output, so the two sides of the conversation separate at a glance.
fn user_prompt_line(text: &str) -> Line<'static> {
    Line::styled(format!("  > {text}  "), theme::palette().user_prompt())
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

/// The status bar's location label: the directory leaf, plus the git branch
/// when `cwd` is inside a repository — `subconscious-code (main)`.
///
/// Read straight from `.git/HEAD` rather than by shelling out to git: this
/// runs at startup on the UI path, and a process spawn there is both slower
/// and one more thing to fail. A detached HEAD has no branch name, so only the
/// directory shows.
fn describe_location(cwd: &Path) -> String {
    let leaf = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string());
    match git_branch(cwd) {
        // Long branch names are the norm on a feature branch and would crowd
        // the mode and context readouts out of the line.
        Some(branch) => format!("{leaf} ({})", truncate(&branch, 24)),
        None => leaf,
    }
}

/// The checked-out branch of the repository containing `dir`, if any. Walks up
/// looking for `.git`, which is a file (not a directory) inside a worktree or
/// submodule — hence the `exists` check rather than `is_dir`.
fn git_branch(dir: &Path) -> Option<String> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let git = d.join(".git");
        if git.exists() {
            let head = if git.is_dir() {
                std::fs::read_to_string(git.join("HEAD")).ok()?
            } else {
                // A worktree/submodule `.git` file points at the real gitdir.
                let text = std::fs::read_to_string(&git).ok()?;
                let path = text.trim().strip_prefix("gitdir:")?.trim();
                std::fs::read_to_string(Path::new(path).join("HEAD")).ok()?
            };
            return head
                .trim()
                .strip_prefix("ref: refs/heads/")
                .map(|b| b.to_string());
        }
        cur = d.parent();
    }
    None
}

/// Put `text` on the *user's* clipboard with OSC 52.
///
/// Not a clipboard crate on purpose: `sc` is routinely run over SSH, where the
/// process has no access to the clipboard the user is actually pasting into —
/// a local clipboard API would copy into the void on the remote host. OSC 52
/// travels back down the same terminal connection and the terminal emulator
/// does the copying, so it works identically local and remote.
///
/// Inside tmux the sequence has to be wrapped in a DCS passthrough (and its
/// ESCs doubled) or tmux swallows it. Terminals that refuse OSC 52 for
/// security drop it silently — there is no reply to wait for — which is why
/// select mode (Ctrl+O) stays as the fallback.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let osc = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let seq = match std::env::var_os("TMUX") {
        Some(_) => format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b")),
        None => osc,
    };
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Standard-alphabet base64 with padding (RFC 4648).
///
/// Hand-rolled because OSC 52 is the only thing in the workspace that needs
/// base64, and twenty lines beats a dependency in the build graph.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = chunk[0] as u32;
        let b2 = *chunk.get(1).unwrap_or(&0) as u32;
        let b3 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        // A 1- or 2-byte tail pads rather than encoding bits that aren't there.
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
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
    fn finished_turn_gets_one_timed_divider_and_spacing() {
        let started = Instant::now();
        let mut view = ViewState::new("m".into());
        view.busy = true;
        view.turn_started = Some(started);
        view.transcript.push(Line::from("answer"));
        view.turn_file_changes.insert(
            PathBuf::from("src/main.rs"),
            (
                Some(b"one\ntwo\n".to_vec().into()),
                Some(b"one\nthree\nfour\n".to_vec().into()),
            ),
        );

        finish_turn_at(&mut view, started + Duration::from_secs(75));

        assert_eq!(view.transcript.len(), 3, "answer, space, divider");
        assert_eq!(view.transcript[1].width(), 0, "blank turn separation");
        let divider: String = view.transcript[2]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(divider.contains("worked for 1m 15s"), "{divider}");
        assert!(divider.contains("3 lines changed (+2 -1)"), "{divider}");
        assert!(view.turn_file_changes.is_empty());
        assert!(!view.busy);

        finish_turn_at(&mut view, started + Duration::from_secs(76));
        assert_eq!(
            view.transcript.len(),
            3,
            "Outcome + Idle cannot duplicate it"
        );
        assert_eq!(turn_duration_label(Duration::from_millis(1400)), "1.4s");
    }

    #[test]
    fn file_artifacts_render_live_and_accumulate_a_net_turn_diff() {
        let mut view = ViewState::new("m".into());
        let path = PathBuf::from("/repo/src/main.rs");
        show_artifact(
            &mut view,
            Path::new("/repo"),
            Artifact::FileChange {
                path: path.clone(),
                before: Some(b"one\ntwo\n".to_vec().into()),
                after: Some(b"one\nthree\n".to_vec().into()),
            },
        );
        show_artifact(
            &mut view,
            Path::new("/repo"),
            Artifact::FileChange {
                path: path.clone(),
                before: Some(b"one\nthree\n".to_vec().into()),
                after: Some(b"one\nfour\nfive\n".to_vec().into()),
            },
        );

        let rendered = view
            .transcript
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("src/main.rs"), "{rendered}");
        assert!(rendered.contains("two"), "{rendered}");
        assert!(rendered.contains("three"), "{rendered}");
        assert!(rendered.contains("four"), "{rendered}");

        let (before, after) = view.turn_file_changes.get(&path).unwrap();
        assert_eq!(before.as_deref(), Some(b"one\ntwo\n".as_slice()));
        assert_eq!(after.as_deref(), Some(b"one\nfour\nfive\n".as_slice()));
        let stats = diff::line_stats(before.as_deref(), after.as_deref());
        assert_eq!(
            stats,
            diff::DiffStats {
                added: 2,
                removed: 1
            }
        );
    }

    #[test]
    fn successful_model_retries_have_a_visible_transcript_notice() {
        let one: String = retry_notice_line(1)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let two: String = retry_notice_line(2)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(one, "↻ model request recovered after 1 retry");
        assert_eq!(two, "↻ model request recovered after 2 retries");
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

    /// RFC 4648 test vectors, including every padding case — a wrong tail is
    /// the classic hand-rolled-base64 bug, and OSC 52 fails silently, so a
    /// broken encoder would look like "copy just doesn't work".
    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // Non-ASCII goes through as its UTF-8 bytes.
        assert_eq!(base64("é".as_bytes()), "w6k=");
    }

    /// A selection reads the same whichever way it was dragged.
    #[test]
    fn selection_orders_row_major_in_both_directions() {
        let down = Selection {
            anchor: (3, 1),
            head: (7, 4),
        };
        assert_eq!(down.ordered(), ((3, 1), (7, 4)));

        let up = Selection {
            anchor: (7, 4),
            head: (3, 1),
        };
        assert_eq!(
            up.ordered(),
            ((3, 1), (7, 4)),
            "dragging up is the same span"
        );

        // Same row, dragged leftward.
        let left = Selection {
            anchor: (9, 2),
            head: (2, 2),
        };
        assert_eq!(left.ordered(), ((2, 2), (9, 2)));
    }

    /// A press that never moves is a click, not a selection — that distinction
    /// is what lets a click keep toggling reasoning/tool blocks.
    #[test]
    fn a_press_without_movement_is_not_a_selection() {
        let click = Selection {
            anchor: (4, 4),
            head: (4, 4),
        };
        assert!(click.is_empty());
        assert!(!Selection {
            anchor: (4, 4),
            head: (5, 4)
        }
        .is_empty());
    }

    #[test]
    fn clicking_a_thought_expands_down_without_moving_its_screen_row() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        fn render(view_state: &mut ViewState) -> String {
            let backend = TestBackend::new(60, 18);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| view::draw(frame, view_state))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let width = buffer.area.width as usize;
            let mut screen = String::new();
            for (index, cell) in buffer.content().iter().enumerate() {
                screen.push_str(cell.symbol());
                if (index + 1) % width == 0 {
                    screen.push('\n');
                }
            }
            screen
        }

        let mut view_state = ViewState::new("m".into());
        view_state.transcript.extend([
            Line::from("older one"),
            Line::from("older two"),
            Line::from("older three"),
        ]);
        let started = Instant::now();
        view_state.begin_reasoning_phase(started);
        view_state.current_reasoning = "private line one\nprivate line two".into();
        view_state.current_text = "answer".into();
        view_state.finish_reasoning(started + Duration::from_millis(240));
        view_state.flush_text();
        view_state.follow = false;
        view_state.scroll_top = 0;

        let before = render(&mut view_state);
        let row_before = before
            .lines()
            .position(|line| line.contains("thought for 0.24s"))
            .expect("thought is visible before the click");
        assert!(toggle_reasoning_preserving_top(
            &mut view_state,
            0,
            row_before as u16,
            0,
        ));

        let after = render(&mut view_state);
        let row_after = after
            .lines()
            .position(|line| line.contains("thought for 0.24s"))
            .expect("thought remains visible after expansion");
        assert_eq!(row_after, row_before, "the clicked row stays stationary");
        assert!(
            after
                .lines()
                .skip(row_after + 1)
                .any(|line| line.contains("private line one")),
            "the retained body grows below the summary: {after}"
        );
        assert!(!view_state.follow, "expansion holds the viewport");
        assert_eq!(view_state.scroll_top, 0, "the prior top is preserved");
    }

    /// The location label pairs the directory leaf with the branch, and keeps
    /// a long feature-branch name from crowding out the rest of the line.
    #[test]
    fn location_label_pairs_directory_with_branch() {
        // This repo is a git checkout, so the label carries a branch.
        let here = std::env::current_dir().unwrap();
        let label = describe_location(&here);
        assert!(
            label.starts_with(
                here.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap()
                    .as_str()
            ),
            "leads with the directory: {label}"
        );
        // Whatever the branch, the label stays short enough to share the line.
        assert!(label.len() <= 64, "bounded length: {label}");

        // A directory with no repository above it is just the directory.
        assert_eq!(describe_location(Path::new("/")), "/");
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
                trace: None,
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
    fn user_prompt_echo_is_a_grey_box_without_the_brand_logo() {
        let line = user_prompt_line("hello");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "  > hello  ");
        assert!(
            !text.contains(theme::DEFAULT_LOGO),
            "logo is reserved for output: {text}"
        );
        let style = line.style;
        assert!(
            style.bg == Some(Color::DarkGray)
                || style
                    .add_modifier
                    .contains(ratatui::style::Modifier::REVERSED),
            "grey background or monochrome enclosure: {style:?}"
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
    fn every_advertised_slash_command_reaches_the_real_dispatcher() {
        let mut seen = std::collections::HashSet::new();
        for (command, description) in complete::slash_palette() {
            assert!(seen.insert(*command), "duplicate command: {command}");
            assert!(!description.trim().is_empty(), "missing help: {command}");
            assert!(
                classify_slash(command).is_some(),
                "advertised command has no dispatcher action: {command}"
            );
        }

        assert!(matches!(
            classify_slash("/rewind 3"),
            Some(SlashAction::Rewind { steps: 3 })
        ));
        assert!(matches!(
            classify_slash("/goal ship the release"),
            Some(SlashAction::SetGoal(goal)) if goal == "ship the release"
        ));
        assert!(matches!(
            classify_slash("/goal clear"),
            Some(SlashAction::ClearGoal)
        ));
        assert!(matches!(
            classify_slash("/loop focus on tests"),
            Some(SlashAction::Loop(Some(direction))) if direction == "focus on tests"
        ));
        let loop_text = loop_prompt(Some("focus on tests"));
        assert!(loop_text.contains("active session goal"));
        assert!(loop_text.contains("Additional direction: focus on tests"));
        assert!(classify_slash("/rewind 0").is_none());
        assert!(classify_slash("/unknown").is_none());
        assert!(classify_slash("hello there").is_none());
        assert!(classify_slash("@src/main.rs").is_none());
        assert!(matches!(
            classify_slash("  /clear  "),
            Some(SlashAction::Clear)
        ));
    }

    #[test]
    fn memory_command_reports_the_files_the_assembler_can_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "project instructions\n").unwrap();
        std::fs::create_dir(dir.path().join(".sc")).unwrap();
        std::fs::write(dir.path().join(".sc/AGENTS.md"), "   \n").unwrap();

        let lines = memory_status_lines(dir.path());
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&dir.path().join("AGENTS.md").display().to_string())),
            "non-empty project memory is reported: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains(&dir.path().join(".sc/AGENTS.md").display().to_string())),
            "blank memory is not loaded: {lines:?}"
        );
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
    fn rapid_enter_stream_is_reassembled_as_one_multiline_prompt() {
        let start = Instant::now();
        let mut composer = String::new();
        let mut pending = Some(PendingSubmit {
            text: "first line".into(),
            staged: start,
        });

        // The first character after Enter arrives immediately: restore the
        // staged line plus its newline, then continue collecting the paste.
        assert!(resolve_staged_submit(
            &mut pending,
            &mut composer,
            start + Duration::from_millis(1),
            true,
        )
        .is_none());
        composer.push_str("second line");
        pending = Some(PendingSubmit {
            text: std::mem::take(&mut composer),
            staged: start + Duration::from_millis(2),
        });

        // A second rapid Enter/line is folded into the same pending prompt.
        assert!(resolve_staged_submit(
            &mut pending,
            &mut composer,
            start + Duration::from_millis(3),
            true,
        )
        .is_none());
        composer.push_str("third line");
        pending = Some(PendingSubmit {
            text: std::mem::take(&mut composer),
            staged: start + Duration::from_millis(4),
        });

        let prompt = resolve_staged_submit(
            &mut pending,
            &mut composer,
            start + PASTE_RESCUE_WINDOW + Duration::from_millis(5),
            false,
        )
        .unwrap();
        assert_eq!(prompt, "first line\nsecond line\nthird line");
        assert!(composer.is_empty());
        assert!(pending.is_none());
    }

    #[test]
    fn human_speed_input_dispatches_staged_prompt_before_new_draft() {
        let start = Instant::now();
        let mut composer = String::new();
        let mut pending = Some(PendingSubmit {
            text: "send this".into(),
            staged: start,
        });

        let prompt = resolve_staged_submit(
            &mut pending,
            &mut composer,
            start + PASTE_RESCUE_WINDOW + Duration::from_millis(1),
            true,
        )
        .unwrap();
        assert_eq!(prompt, "send this");
        assert!(composer.is_empty());
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

        // Busy with no queue cancels the turn, even with a draft present.
        s.busy = true;
        assert_eq!(esc_action(&s), EscAction::Cancel);

        // A queued follow-up makes the first Esc wait for the current tool
        // boundary; once armed, a second Esc still cancels immediately.
        s.queued_messages = 1;
        assert_eq!(esc_action(&s), EscAction::QueueAfterTool);
        s.queued_after_tool = true;
        assert_eq!(esc_action(&s), EscAction::Cancel);
        s.queued_messages = 0;
        s.queued_after_tool = false;
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
