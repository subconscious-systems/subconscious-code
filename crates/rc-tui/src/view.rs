//! Rendering: turns [`ViewState`] into ratatui widgets. The state types live
//! here so a `TestBackend` render test can build them with no tokio and no model.
//!
//! M4a renders plain text (simple `Wrap`, no markdown, no diff) and a
//! single-line composer. Markdown / word-level diff land in M4b.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use rc_core::{AgentMode, Usage};
use serde_json::Value;

use crate::complete::Completion;

/// A pending permission ask the user must answer before the turn proceeds.
#[derive(Clone)]
pub(crate) struct PendingAsk {
    pub id: u64,
    pub tool: String,
    pub input: Value,
    pub reason: String,
}

/// The open completion menu: the computed candidates + the selected row.
/// Kept in `ViewState` so a `TestBackend` render test can build it directly.
#[derive(Clone)]
pub(crate) struct CompletionMenu {
    pub completion: Completion,
    pub selected: usize,
}

/// The renderable subset of app state. Kept separate from the runtime handles so
/// [`draw`] (and a `TestBackend` render test) can run with no tokio and no model.
#[derive(Clone)]
pub(crate) struct ViewState {
    /// Completed transcript entries, parsed to styled lines once (incremental:
    /// parsed on flush, not re-parsed per frame — §12's O(n²) trap).
    pub transcript: Vec<Line<'static>>,
    /// In-progress assistant text (markdown-parsed and flushed on the next boundary).
    pub current_text: String,
    pub mode: AgentMode,
    pub last_usage: Option<Usage>,
    /// M8: size of the last context sent (chars, estimated tokens). Shown in the
    /// status bar so the operator can watch the context grow — Subconscious Code
    /// has no window to exceed, which only makes the number more interesting.
    pub last_context: Option<(usize, usize)>,
    pub busy: bool,
    pub pending_ask: Option<PendingAsk>,
    pub composer: String,
    /// The open autocomplete menu, if any (M4c). `None` when the composer has
    /// no `@`/`/` trigger at the caret or the user dismissed it.
    pub menu: Option<CompletionMenu>,
    pub model_name: String,
}

impl ViewState {
    pub(crate) fn new(model_name: String) -> Self {
        Self {
            transcript: Vec::new(),
            current_text: String::new(),
            mode: AgentMode::Default,
            last_usage: None,
            last_context: None,
            busy: false,
            pending_ask: None,
            composer: String::new(),
            menu: None,
            model_name,
        }
    }

    /// Move accumulated assistant text into the transcript, markdown-parsed once.
    pub(crate) fn flush_text(&mut self) {
        if self.current_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.current_text);
        self.transcript.extend(crate::markdown::parse_blocks(&text));
    }
}

pub(crate) fn draw(frame: &mut Frame, state: &ViewState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(4)])
        .split(area);
    draw_transcript(frame, state, chunks[0]);
    draw_status(frame, state, chunks[1]);
    if let Some(ask) = &state.pending_ask {
        draw_ask(frame, ask, chunks[2]);
    } else {
        draw_composer(frame, state, chunks[2]);
        // The completion menu floats above the composer, as a popup.
        if let Some(menu) = &state.menu {
            draw_menu(frame, menu, chunks[2]);
        }
    }
}

fn draw_transcript(frame: &mut Frame, state: &ViewState, area: Rect) {
    // Show the bottom of the transcript (latest lines) within the area height.
    let h = area.height as usize;
    let start = state.transcript.len().saturating_sub(h);
    let mut lines: Vec<Line<'static>> = state.transcript[start..].to_vec();
    // The in-progress text is re-parsed each frame (small/growing) — that's the
    // only per-frame parse; completed turns above are already cached.
    if !state.current_text.is_empty() {
        lines.extend(crate::markdown::parse_blocks(&state.current_text));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), area);
}

fn draw_status(frame: &mut Frame, state: &ViewState, area: Rect) {
    let usage = state.last_usage.as_ref();
    let tokens = usage.map(|u| u.total_tokens).unwrap_or(0);
    let cached = usage.and_then(|u| u.cached_tokens()).unwrap_or(0);
    let cached_str = if cached > 0 { format!(" ({} cached)", cached) } else { String::new() };
    let activity = if state.busy { "working" } else { "idle" };
    // The context figure is the headline number for this agent, so it gets a
    // human-scaled rendering rather than a raw char count.
    let ctx_str = match state.last_context {
        Some((chars, est)) => format!(" | ctx: {} (~{} tok)", human_bytes(chars), human_count(est)),
        None => String::new(),
    };
    let line = format!(
        " {} | {} | tokens: {}{}{} | {}",
        state.model_name,
        mode_name(state.mode),
        tokens,
        cached_str,
        ctx_str,
        activity,
    );
    frame.render_widget(Paragraph::new(line).style(Style::new().fg(Color::Cyan)), area);
}

/// Render a char count as B/K/M, so a large context reads at a glance.
fn human_bytes(n: usize) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1_000.0),
        n => format!("{n}"),
    }
}

/// Render a token count as K/M.
fn human_count(n: usize) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1_000.0),
        n => format!("{n}"),
    }
}

fn draw_composer(frame: &mut Frame, state: &ViewState, area: Rect) {
    let prompt = format!("> {}█", state.composer);
    frame.render_widget(
        Paragraph::new(prompt).block(Block::default().borders(Borders::ALL).title("compose")),
        area,
    );
}

/// Max rows shown in the completion popup. The candidate list in
/// [`CompletionMenu`] may be longer; the menu window-clips to this many.
const MENU_ROWS: usize = 8;

/// Render the completion popup as an overlay growing upward from the top of
/// the composer `area`. The selected row is highlighted; the menu is at most
/// `MENU_ROWS` tall and never overflows the screen.
fn draw_menu(frame: &mut Frame, menu: &CompletionMenu, composer_area: Rect) {
    let candidates = &menu.completion.candidates;
    if candidates.is_empty() {
        return;
    }
    let count = candidates.len().min(MENU_ROWS);
    // The menu sits just above the composer's top border. Height = content rows
    // + 2 border rows (top + bottom). Clamp to the available space above.
    let max_h = composer_area.y as usize;
    let h = (count + 2).min(max_h.max(1)) as u16;
    let y = composer_area.y.saturating_sub(h);
    let width = composer_area.width.min(40);
    let x = composer_area.x;
    let area = Rect { x, y, width, height: h };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(count);
    let title = match menu.completion.kind {
        crate::complete::MenuKind::File => "files",
        crate::complete::MenuKind::Slash => "commands",
    };
    let start = menu.selected.min(candidates.len().saturating_sub(1));
    for (i, cand) in candidates.iter().take(count).enumerate() {
        let marker = if i == start { "▶ " } else { "  " };
        let line = Line::from(format!("{marker}{cand}"));
        if i == start {
            lines.push(line.style(Style::new().fg(Color::Black).bg(Color::Cyan)));
        } else {
            lines.push(line.style(Style::new().fg(Color::Yellow)));
        }
    }
    // Render the popup with a clear background so it doesn't bleed the transcript.
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_ask(frame: &mut Frame, ask: &PendingAsk, area: Rect) {
    let line = format!(
        " ? {} needs permission: {}\n  [y]once  [s]ession  [a]lways  [n]o",
        ask.tool, ask.reason,
    );
    frame.render_widget(
        Paragraph::new(line)
            .style(Style::new().fg(Color::Yellow))
            .block(Block::default().borders(Borders::ALL).title("permission")),
        area,
    );
}

fn mode_name(m: AgentMode) -> &'static str {
    match m {
        AgentMode::Default => "default",
        AgentMode::AcceptEdits => "accept-edits",
        AgentMode::Plan => "plan",
        AgentMode::BypassPermissions => "bypass",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render `state` to a 60x10 TestBackend and return the joined cell symbols.
    fn rendered(state: &ViewState) -> String {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, state)).unwrap();
        let buf = terminal.backend().buffer();
        let width = buf.area.width as usize;
        let mut out = String::new();
        for (i, cell) in buf.content().iter().enumerate() {
            out.push_str(cell.symbol());
            if (i + 1) % width == 0 {
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn status_line_shows_model_mode_and_activity() {
        let mut state = ViewState::new("mock-model".into());
        state.mode = AgentMode::Plan;
        state.busy = true;
        let screen = rendered(&state);
        assert!(screen.contains("mock-model"), "model name: {screen}");
        assert!(screen.contains("plan"), "mode: {screen}");
        assert!(screen.contains("working"), "busy state: {screen}");
    }

    /// The context figure is the headline number for this agent, so it has to
    /// actually reach the status bar — and at a readable scale.
    #[test]
    fn status_line_shows_context_size() {
        let mut state = ViewState::new("m".into());
        state.last_context = Some((12_071_555, 2_748_220));
        let screen = rendered(&state);
        assert!(screen.contains("ctx: 12.1M"), "context chars: {screen}");
        assert!(screen.contains("2.7M tok"), "estimated tokens: {screen}");
    }

    /// No context yet (before the first request) means no stale figure shown.
    #[test]
    fn status_line_omits_context_before_the_first_request() {
        let state = ViewState::new("m".into());
        assert!(!rendered(&state).contains("ctx:"));
    }

    #[test]
    fn human_scales_read_at_a_glance() {
        assert_eq!(human_bytes(999), "999");
        assert_eq!(human_bytes(12_071_555), "12.1M");
        assert_eq!(human_bytes(16_384), "16.4K");
        assert_eq!(human_count(2_748_220), "2.7M");
    }

    #[test]
    fn ask_prompt_replaces_the_composer() {
        let mut state = ViewState::new("m".into());
        state.pending_ask = Some(PendingAsk {
            id: 1,
            tool: "Edit".into(),
            input: Value::Null,
            reason: "Edit requires confirmation".into(),
        });
        let screen = rendered(&state);
        assert!(screen.contains("permission"), "ask block titled: {screen}");
        assert!(screen.contains("Edit requires confirmation"), "reason: {screen}");
        assert!(screen.contains("[y]once"), "answer keys: {screen}");
    }

    #[test]
    fn transcript_and_streaming_text_render() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("-> Read README.md"));
        state.transcript.push(Line::from("<- Read: # rc"));
        state.current_text = "streaming answer".into();
        let screen = rendered(&state);
        assert!(screen.contains("-> Read README.md"), "tool start line: {screen}");
        assert!(screen.contains("streaming answer"), "in-progress text: {screen}");
    }

    #[test]
    fn markdown_and_diff_render_in_the_transcript() {
        // Completed markdown is parsed once on flush; an Edit previews a word diff.
        let mut state = ViewState::new("m".into());
        state.transcript.extend(crate::markdown::parse_blocks("# Heading\n\nsome **bold** text"));
        state.transcript.push(crate::diff::word_diff_line("old word", "new word"));
        let screen = rendered(&state);
        assert!(screen.contains("Heading"), "heading: {screen}");
        assert!(screen.contains("bold"), "bold: {screen}");
        // The inline word diff interleaves the deleted ("old") and inserted
        // ("new") tokens, then the shared " word" -> "oldnew word".
        assert!(screen.contains("oldnew"), "diff interleaves old+new: {screen}");
        assert!(screen.contains("word"), "diff keeps shared word: {screen}");
    }

    #[test]
    fn completion_menu_renders_candidates_and_selection() {
        // A slash menu with two candidates; the first is selected (▶, highlighted).
        let mut state = ViewState::new("m".into());
        state.composer = "/c".into();
        state.menu = Some(CompletionMenu {
            completion: Completion {
                kind: crate::complete::MenuKind::Slash,
                replace_start: 0,
                candidates: vec!["/clear".into(), "/mode".into()],
            },
            selected: 0,
        });
        let screen = rendered(&state);
        assert!(screen.contains("commands"), "menu title: {screen}");
        assert!(screen.contains("/clear"), "first candidate: {screen}");
        assert!(screen.contains("/mode"), "second candidate: {screen}");
        assert!(screen.contains("▶"), "selection marker: {screen}");
    }

    #[test]
    fn completion_menu_empty_candidates_renders_nothing() {
        // An empty candidate list draws no popup (defensive; complete() returns
        // None in that case, but the view must not panic if state is stale).
        let mut state = ViewState::new("m".into());
        state.composer = "@zzz".into();
        state.menu = Some(CompletionMenu {
            completion: Completion {
                kind: crate::complete::MenuKind::File,
                replace_start: 0,
                candidates: Vec::new(),
            },
            selected: 0,
        });
        let screen = rendered(&state);
        // No "files"/"commands" title block should appear.
        assert!(!screen.contains("files"), "no menu for empty candidates: {screen}");
        assert!(!screen.contains("commands"), "no menu title: {screen}");
    }
}
