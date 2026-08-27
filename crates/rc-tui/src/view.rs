//! Rendering: turns [`ViewState`] into ratatui widgets. The state types live
//! here so a `TestBackend` render test can build them with no tokio and no model.
//!
//! M4a renders plain text (simple `Wrap`, no markdown, no diff) and a
//! single-line composer. Markdown / word-level diff land in M4b.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use rc_core::{AgentMode, Usage};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::complete::Completion;
use crate::menu::{ago, MenuPage, MenuState, Row};
use crate::theme;

type FileSnapshots = (Option<std::sync::Arc<[u8]>>, Option<std::sync::Arc<[u8]>>);

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

/// Byte range of a multiline paste inside the real composer buffer. Rendering
/// replaces the range with one compact label; submission still uses the exact
/// underlying text, including every newline.
#[derive(Clone)]
struct PasteMarker {
    start: usize,
    end: usize,
    lines: usize,
}

/// A completed reasoning segment retained behind its collapsed transcript row.
/// `summary_index` tracks the row as earlier blocks expand/collapse; `body`
/// keeps the full streamed reasoning available without showing it by default.
#[derive(Clone)]
struct ReasoningBlock {
    summary_index: usize,
    body: Vec<Line<'static>>,
    /// Live turns have a measured duration. Older session files predate that
    /// presentation metadata, so resumed thoughts remain expandable but use
    /// the honest untimed label `thought` rather than inventing `0.00s`.
    elapsed: Option<Duration>,
    expanded: bool,
}

/// A completed tool call retained behind its compact timed transcript row.
/// `body` contains the original call, any edit preview, and the complete
/// result, so expansion restores the whole interaction rather than a summary.
#[derive(Clone)]
struct ToolBlock {
    summary_index: usize,
    body: Vec<Line<'static>>,
    collapsed_line: Line<'static>,
    expanded_line: Line<'static>,
    expanded: bool,
}

/// Result of toggling a completed reasoning or tool block. The app uses this
/// to anchor the expanded block in the viewport.
pub(crate) struct TranscriptToggle {
    pub summary_index: usize,
    #[cfg(test)]
    pub expanded: bool,
}

/// Screen-space target for one rendered `thought for N.NNs` row. Rebuilt on
/// every draw so mouse hit-testing follows transcript scrolling and wrapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReasoningHitbox {
    block_index: usize,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl ReasoningHitbox {
    fn contains(self, column: u16, row: u16) -> bool {
        column >= self.x
            && column < self.x.saturating_add(self.width)
            && row >= self.y
            && row < self.y.saturating_add(self.height)
    }
}

/// Screen-space target for one rendered completed-tool summary. Like thought
/// hitboxes, this is rebuilt every frame so scrolling and wrapping stay exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ToolHitbox {
    block_index: usize,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl ToolHitbox {
    fn contains(self, column: u16, row: u16) -> bool {
        column >= self.x
            && column < self.x.saturating_add(self.width)
            && row >= self.y
            && row < self.y.saturating_add(self.height)
    }
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
    /// In-progress reasoning (the model's chain-of-thought), kept separate from
    /// [`current_text`](Self::current_text) so it can render distinctly (dim +
    /// italic, under a header) instead of blurring into the answer. Flushed
    /// alongside the text on the next boundary.
    pub current_reasoning: String,
    /// Cached markdown parse of [`current_text`](Self::current_text), refreshed
    /// only when [`current_dirty`](Self::current_dirty) is set (once per token
    /// delta), then reused across frames. Without this, `parse_blocks` ran on
    /// the whole growing buffer every frame — and again in `scroll_indicator`
    /// — which is O(n) per frame and janks on long streaming replies.
    pub current_parsed: Vec<Line<'static>>,
    /// [`current_parsed`](Self::current_parsed) is stale and needs a re-parse on
    /// the next draw. Set by each `Text`/`Reasoning` delta; cleared after refresh.
    pub current_dirty: bool,
    /// Start of the current model-thinking phase. The app arms this when a turn
    /// is submitted (and again after a tool batch), so the displayed duration
    /// includes time-to-first-reasoning rather than only token emission time.
    pub reasoning_started: Option<Instant>,
    /// Frozen duration once answer text begins. Without this, a reasoning block
    /// would incorrectly include the time spent streaming the final answer.
    pub reasoning_elapsed: Option<Duration>,
    /// Completed reasoning bodies, normally hidden behind clickable
    /// `thought for N.NNs` rows and restored in place when selected.
    reasoning_blocks: Vec<ReasoningBlock>,
    /// Click targets for visible reasoning summaries, refreshed by
    /// `draw_transcript` after its exact wrap/scroll calculation.
    reasoning_hitboxes: Vec<ReasoningHitbox>,
    /// Completed tool bodies and click targets follow the same retained-block
    /// model as reasoning, but each tool owns distinct collapsed/expanded rows.
    tool_blocks: Vec<ToolBlock>,
    tool_hitboxes: Vec<ToolHitbox>,
    pub mode: AgentMode,
    pub last_usage: Option<Usage>,
    /// Token size of the request context currently shown in the status bar.
    /// The preflight estimate is installed immediately before a request, then
    /// replaced by the provider-returned `prompt_tokens` for that same request.
    pub context_tokens: Option<u64>,
    /// Whether [`context_tokens`](Self::context_tokens) is still the preflight
    /// estimate. A returned usage event flips this off.
    pub context_tokens_estimated: bool,
    /// Provider-reported cached/prompt ratio for the current completed request.
    /// Cleared when the next preflight estimate arrives to avoid stale rates.
    pub cache_hit_rate: Option<f64>,
    pub busy: bool,
    pub pending_ask: Option<PendingAsk>,
    pub composer: String,
    /// Multiline regions displayed as `[pasted N lines]` instead of expanding
    /// the composer. Markers are cleared when destructive editing begins.
    paste_markers: Vec<PasteMarker>,
    /// Prompt history for Alt+↑/Alt+↓ recall — the submitted prompts of this
    /// session, newest last. Consecutive duplicates collapse (bash-style).
    pub prompt_history: Vec<String>,
    /// Current position in [`Self::prompt_history`] for recall. `None` = the
    /// composer holds the live draft (not browsing); `Some(i)` = showing
    /// `prompt_history[i]`.
    pub history_pos: Option<usize>,
    /// The in-progress draft stashed when the user entered history, restored on
    /// the way back past the newest entry.
    pub history_draft: String,
    /// The open autocomplete menu, if any (M4c). `None` when the composer has
    /// no `@`/`/` trigger at the caret or the user dismissed it.
    pub menu: Option<CompletionMenu>,
    pub model_name: String,
    /// The session cwd, shown in the welcome card. Set once in `App::new`.
    pub cwd: String,
    /// Scrollback: `true` pins the view to the bottom (auto-scrolls as new
    /// content arrives). Scrolling up sets it `false` so the view holds steady
    /// while the conversation grows below it; submitting, `/clear`, `End`, or
    /// paging down to the bottom set it `true` again.
    pub follow: bool,
    /// When not [`Self::follow`], the index of the topmost transcript line shown.
    pub scroll_top: usize,
    /// Last transcript area height, recorded each draw so the keymap can page by
    /// a real screenful rather than a guessed constant.
    pub area_height: usize,
    /// When the current turn started — the epoch for the "thinking" timer and
    /// the spinner phase. `None` while idle. Set optimistically on submit so
    /// the indicator appears instantly, refreshed on `Ready`.
    pub turn_started: Option<Instant>,
    /// Net file mutations in the active turn. The first `before` snapshot and
    /// latest `after` snapshot are retained per path, so editing the same line
    /// twice counts once in the final turn divider rather than twice.
    pub turn_file_changes: HashMap<PathBuf, FileSnapshots>,
    /// Number of tool calls in flight (a batch may run several at once). The
    /// live tool-spinner line shows while this is > 0.
    pub running: u32,
    /// The most-recently started in-flight tool's name, for the live spinner.
    /// Cleared when `running` returns to 0.
    pub running_tool: Option<String>,
    /// Chars of assistant text received since the current turn's stream began,
    /// for the live tokens/sec meter. Reset on turn end / submit.
    pub stream_chars: u64,
    /// When the current turn's stream began (first text/reasoning delta), the
    /// epoch for the tokens/sec rate so it isn't diluted by pre-stream tool time.
    /// `None` until the first delta lands (a tool-only turn never sets it).
    pub stream_started: Option<Instant>,
    /// When the user last touched the composer. The caret stays solid for a
    /// beat after a keystroke, then begins to blink — matching native cursor
    /// behavior and avoiding a permanent flicker while typing.
    pub last_input: Option<Instant>,
    /// Process start — the caret blink's epoch (it needs a continuously
    /// advancing clock independent of any single turn).
    pub process_started: Instant,
    /// The `/menu` overlay, when open. A full-screen modal: while this is
    /// `Some` it owns both the frame and the keymap, so there's no half-state
    /// where a keystroke lands in the composer hidden behind it.
    pub menu_overlay: Option<crate::menu::MenuState>,
    /// The live mouse selection, in *screen* cells. `None` when nothing is
    /// selected. Screen coordinates rather than transcript offsets because
    /// what the user is selecting is what they can see — after wrapping,
    /// markdown styling and collapsing, the rendered buffer is the only place
    /// that text exists in the shape they are pointing at.
    pub selection: Option<Selection>,
    /// The selected text, harvested from the render buffer during [`draw`].
    pub selection_text: Option<String>,
    /// Set on mouse-up: the run loop copies [`Self::selection_text`] to the
    /// clipboard after the next draw (the text only exists once drawn).
    pub copy_pending: bool,
    /// A short-lived "copied N chars" confirmation and when it was shown.
    pub copy_notice: Option<(String, Instant)>,
    /// Where the session is, for the status bar: the working directory's leaf
    /// and the git branch when there is one (`subconscious-code (main)`).
    /// Resolved once at startup — it is the fact that scrolls off the top of a
    /// long transcript and never comes back, so the persistent chrome owns it.
    pub location: String,
    /// Whether mouse capture is on. While it is, the app receives every drag
    /// and the terminal never sees one, so text cannot be selected — the one
    /// thing a terminal is otherwise always good for. Off hands the mouse back
    /// for selection and copy, at the cost of wheel scrolling. Toggled with
    /// Ctrl+O (`/select`).
    pub mouse_capture: bool,
}

/// A drag selection, anchored where the button went down and headed wherever
/// the pointer is now. Ordering is not normalized here — a selection dragged
/// upward or leftward is as valid as one dragged down, so the range is sorted
/// at use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// Where the drag began (column, row).
    pub anchor: (u16, u16),
    /// Where the pointer is now.
    pub head: (u16, u16),
}

impl Selection {
    /// The selection as (start, end) in reading order, whichever way it was
    /// dragged.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        // Compare row-major: a point on an earlier row always precedes one on
        // a later row, whatever the columns are.
        let a = (self.anchor.1, self.anchor.0);
        let b = (self.head.1, self.head.0);
        if a <= b {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// True when the pointer never left the cell it went down on — a click,
    /// not a drag.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

impl ViewState {
    pub(crate) fn new(model_name: String) -> Self {
        Self {
            transcript: Vec::new(),
            current_text: String::new(),
            current_reasoning: String::new(),
            current_parsed: Vec::new(),
            current_dirty: false,
            reasoning_started: None,
            reasoning_elapsed: None,
            reasoning_blocks: Vec::new(),
            reasoning_hitboxes: Vec::new(),
            tool_blocks: Vec::new(),
            tool_hitboxes: Vec::new(),
            mode: AgentMode::Default,
            last_usage: None,
            context_tokens: None,
            context_tokens_estimated: false,
            cache_hit_rate: None,
            busy: false,
            pending_ask: None,
            composer: String::new(),
            paste_markers: Vec::new(),
            prompt_history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            menu: None,
            model_name,
            cwd: String::new(),
            follow: true,
            scroll_top: 0,
            area_height: 0,
            turn_started: None,
            turn_file_changes: HashMap::new(),
            running: 0,
            running_tool: None,
            stream_chars: 0,
            stream_started: None,
            last_input: None,
            process_started: Instant::now(),
            menu_overlay: None,
            location: String::new(),
            mouse_capture: false,
            selection: None,
            selection_text: None,
            copy_pending: false,
            copy_notice: None,
        }
    }

    /// Whether the pre-conversation welcome card is showing right now (the
    /// logo splash, `cwd`, key hints) rather than the transcript: nothing in
    /// the transcript, nothing streaming, no turn in flight, no pending ask.
    fn welcome_card_visible(&self) -> bool {
        self.transcript.is_empty()
            && self.current_text.is_empty()
            && self.current_reasoning.is_empty()
            && !self.busy
            && self.pending_ask.is_none()
    }

    /// Append one native bracketed-paste payload without interpreting embedded
    /// newlines as submit keys. Returns the logical line count for tests/status.
    pub(crate) fn append_paste(&mut self, raw: &str) -> usize {
        let text = raw.replace("\r\n", "\n").replace('\r', "\n");
        if text.is_empty() {
            return 0;
        }
        let lines = text.split('\n').count();
        let start = self.composer.len();
        self.composer.push_str(&text);
        if lines > 1 {
            self.paste_markers.push(PasteMarker {
                start,
                end: self.composer.len(),
                lines,
            });
        }
        lines
    }

    /// Once the user destructively edits or replaces the buffer, reveal the
    /// actual text instead of attempting to maintain stale byte ranges.
    pub(crate) fn clear_paste_markers(&mut self) {
        self.paste_markers.clear();
    }

    /// Arm the timer for a model-thinking phase. Called at submit/Ready and
    /// after a tool batch completes, before the next model response arrives.
    pub(crate) fn begin_reasoning_phase(&mut self, now: Instant) {
        self.reasoning_started = Some(now);
        self.reasoning_elapsed = None;
    }

    /// Record that reasoning is actually streaming. Usually the phase was
    /// already armed at submit; this fallback handles restored/synthetic event
    /// streams that begin directly with a Reasoning event.
    pub(crate) fn note_reasoning(&mut self, now: Instant) {
        if self.reasoning_started.is_none() {
            self.reasoning_started = Some(now);
        }
    }

    /// Freeze reasoning time when answer text starts. The final answer may
    /// stream for minutes; that time must not inflate `thought for N.NNs`.
    pub(crate) fn finish_reasoning(&mut self, now: Instant) {
        if self.current_reasoning.is_empty() || self.reasoning_elapsed.is_some() {
            return;
        }
        self.reasoning_elapsed = Some(
            self.reasoning_started
                .map(|started| now.saturating_duration_since(started))
                .unwrap_or_default(),
        );
    }

    /// Expand/collapse the most recent completed reasoning block in place.
    /// Completed bodies are retained in `reasoning_blocks`, so collapsing is a
    /// presentation choice rather than destructive compaction.
    #[cfg(test)]
    pub(crate) fn toggle_latest_reasoning(&mut self) -> Option<TranscriptToggle> {
        let block_index = self.reasoning_blocks.len().checked_sub(1)?;
        self.toggle_reasoning(block_index)
    }

    /// Toggle the reasoning summary under a mouse coordinate from the most
    /// recent draw. Only the label's visible cells are active.
    pub(crate) fn toggle_reasoning_at(
        &mut self,
        column: u16,
        row: u16,
    ) -> Option<TranscriptToggle> {
        let block_index = self
            .reasoning_hitboxes
            .iter()
            .find(|hitbox| hitbox.contains(column, row))?
            .block_index;
        self.toggle_reasoning(block_index)
    }

    fn toggle_reasoning(&mut self, block_index: usize) -> Option<TranscriptToggle> {
        let block = self.reasoning_blocks.get(block_index)?;
        let summary_index = block.summary_index;
        if summary_index >= self.transcript.len() {
            return None;
        }
        let expanded = !block.expanded;
        let elapsed = block.elapsed;
        let body_len = block.body.len();
        let body = expanded.then(|| block.body.clone());

        self.transcript[summary_index] = reasoning_summary_line(elapsed, expanded);
        if let Some(body) = body {
            self.transcript
                .splice(summary_index + 1..summary_index + 1, body);
        } else {
            let end = (summary_index + 1 + body_len).min(self.transcript.len());
            self.transcript.drain(summary_index + 1..end);
        }
        self.reasoning_blocks[block_index].expanded = expanded;
        // Geometry changed; discard the old frame's targets until the next
        // draw rebuilds them from the new transcript layout.
        self.reasoning_hitboxes.clear();
        self.tool_hitboxes.clear();

        // Every following retained block, regardless of type, moves with this
        // splice. Keeping one shared index adjustment prevents a thought above
        // a tool (or vice versa) from leaving the later click target stale.
        if expanded {
            self.shift_summary_indices_at_or_after(summary_index + 1, body_len as isize);
        } else {
            self.shift_summary_indices_at_or_after(
                summary_index + 1 + body_len,
                -(body_len as isize),
            );
        }

        Some(TranscriptToggle {
            summary_index,
            #[cfg(test)]
            expanded,
        })
    }

    /// Replace a completed live tool range with one compact row and retain the
    /// supplied full body for click-to-expand. Returns how many transcript rows
    /// were removed so the app can reindex still-running parallel calls.
    pub(crate) fn replace_with_tool_block(
        &mut self,
        start: usize,
        len: usize,
        collapsed_line: Line<'static>,
        expanded_line: Line<'static>,
        body: Vec<Line<'static>>,
    ) -> usize {
        let end = start + len;
        self.transcript
            .splice(start..end, std::iter::once(collapsed_line.clone()));
        let removed = len.saturating_sub(1);
        if removed > 0 {
            self.shift_summary_indices_at_or_after(end, -(removed as isize));
        }
        self.tool_blocks.push(ToolBlock {
            summary_index: start,
            body,
            collapsed_line,
            expanded_line,
            expanded: false,
        });
        self.reasoning_hitboxes.clear();
        self.tool_hitboxes.clear();
        removed
    }

    /// Remove a successful live tool call from the transcript completely.
    /// Retained thoughts and failed-tool summaries after it shift upward so
    /// their click targets continue to point at the correct logical rows.
    pub(crate) fn remove_transcript_range(&mut self, start: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let end = start + len;
        self.transcript.drain(start..end);
        self.shift_summary_indices_at_or_after(end, -(len as isize));
        self.reasoning_hitboxes.clear();
        self.tool_hitboxes.clear();
        len
    }

    /// Retain a tool whose start event was missing (for example after stream
    /// lag) at the end of the transcript, still expandable like a normal call.
    pub(crate) fn push_tool_block(
        &mut self,
        collapsed_line: Line<'static>,
        expanded_line: Line<'static>,
        body: Vec<Line<'static>>,
    ) {
        let summary_index = self.transcript.len();
        self.transcript.push(collapsed_line.clone());
        self.tool_blocks.push(ToolBlock {
            summary_index,
            body,
            collapsed_line,
            expanded_line,
            expanded: false,
        });
        self.reasoning_hitboxes.clear();
        self.tool_hitboxes.clear();
    }

    /// Toggle the completed tool summary under an exact mouse coordinate.
    pub(crate) fn toggle_tool_at(&mut self, column: u16, row: u16) -> Option<TranscriptToggle> {
        let block_index = self
            .tool_hitboxes
            .iter()
            .find(|hitbox| hitbox.contains(column, row))?
            .block_index;
        self.toggle_tool(block_index)
    }

    fn toggle_tool(&mut self, block_index: usize) -> Option<TranscriptToggle> {
        let block = self.tool_blocks.get(block_index)?;
        let summary_index = block.summary_index;
        if summary_index >= self.transcript.len() {
            return None;
        }
        let expanded = !block.expanded;
        let body_len = block.body.len();
        let summary = if expanded {
            block.expanded_line.clone()
        } else {
            block.collapsed_line.clone()
        };
        let body = expanded.then(|| block.body.clone());

        self.transcript[summary_index] = summary;
        if let Some(body) = body {
            self.transcript
                .splice(summary_index + 1..summary_index + 1, body);
        } else {
            let end = (summary_index + 1 + body_len).min(self.transcript.len());
            self.transcript.drain(summary_index + 1..end);
        }
        self.tool_blocks[block_index].expanded = expanded;
        self.reasoning_hitboxes.clear();
        self.tool_hitboxes.clear();
        if expanded {
            self.shift_summary_indices_at_or_after(summary_index + 1, body_len as isize);
        } else {
            self.shift_summary_indices_at_or_after(
                summary_index + 1 + body_len,
                -(body_len as isize),
            );
        }

        Some(TranscriptToggle {
            summary_index,
            #[cfg(test)]
            expanded,
        })
    }

    fn shift_summary_indices_at_or_after(&mut self, threshold: usize, delta: isize) {
        let shift = |index: &mut usize| {
            if *index < threshold {
                return;
            }
            *index = if delta >= 0 {
                index.saturating_add(delta as usize)
            } else {
                index.saturating_sub(delta.unsigned_abs())
            };
        };
        for block in &mut self.reasoning_blocks {
            shift(&mut block.summary_index);
        }
        for block in &mut self.tool_blocks {
            shift(&mut block.summary_index);
        }
    }

    /// Clear completed transcript state together. The reasoning bodies live
    /// outside `transcript`, so `/clear` and `/compact` must clear both.
    pub(crate) fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.reasoning_blocks.clear();
        self.reasoning_hitboxes.clear();
        self.tool_blocks.clear();
        self.tool_hitboxes.clear();
    }

    /// Move accumulated assistant text and reasoning into the transcript: the
    /// answer is parsed as markdown, while reasoning becomes a collapsed,
    /// expandable `thought for N.NNs` row. Both streaming buffers clear.
    pub(crate) fn flush_text(&mut self) {
        self.flush_text_at(Instant::now());
    }

    fn flush_text_at(&mut self, now: Instant) {
        if self.current_text.is_empty() && self.current_reasoning.is_empty() {
            return;
        }
        self.finish_reasoning(now);
        let elapsed = self.reasoning_elapsed.take().unwrap_or_default();
        self.reasoning_started = None;
        let text = std::mem::take(&mut self.current_text);
        let reasoning = std::mem::take(&mut self.current_reasoning);
        if !reasoning.is_empty() {
            let summary_index = self.transcript.len();
            self.transcript
                .push(reasoning_summary_line(Some(elapsed), false));
            self.reasoning_blocks.push(ReasoningBlock {
                summary_index,
                body: reasoning_body_lines(&reasoning),
                elapsed: Some(elapsed),
                expanded: false,
            });
            if !text.is_empty() {
                self.transcript.push(Line::default());
            }
        }
        if !text.is_empty() {
            self.transcript.extend(parse_assistant_output(&text));
        }
        // The in-progress cache is now empty too.
        self.current_parsed.clear();
        self.current_dirty = false;
    }

    /// Rebuild one completed assistant turn from persisted history. Reasoning
    /// stays collapsed and clickable; old JSONL records do not carry its wall
    /// time, so their summary is intentionally untimed.
    pub(crate) fn restore_assistant_turn(&mut self, reasoning: Option<&str>, text: &str) {
        if let Some(reasoning) = reasoning.filter(|r| !r.is_empty()) {
            let summary_index = self.transcript.len();
            self.transcript.push(reasoning_summary_line(None, false));
            self.reasoning_blocks.push(ReasoningBlock {
                summary_index,
                body: reasoning_body_lines(reasoning),
                elapsed: None,
                expanded: false,
            });
            if !text.is_empty() {
                self.transcript.push(Line::default());
            }
        }
        if !text.is_empty() {
            self.transcript.extend(parse_assistant_output(text));
        }
    }
}

/// Build the trailing live lines for an in-progress assistant turn. Reasoning
/// content is never rendered while it streams: one compact activity row takes
/// its place, followed by any answer text. The full body remains buffered and
/// becomes available only through the completed timed row after flush.
fn parse_live(
    reasoning: &str,
    text: &str,
    reasoning_elapsed: Option<Duration>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if !reasoning.is_empty() {
        out.push(match reasoning_elapsed {
            Some(elapsed) => reasoning_summary_line(Some(elapsed), false),
            None => live_reasoning_line(),
        });
        if !text.is_empty() {
            out.push(Line::default());
        }
    }
    if !text.is_empty() {
        out.extend(parse_assistant_output(text));
    }
    out
}

/// Parse assistant markdown into a consistent two-cell response gutter. The
/// first visible row carries the one-cell `logo.svg` reduction plus a space;
/// every later visible row gets two spaces, so headings, prose, tables and code
/// all align with the content after the mark. Leading/structural blank rows stay
/// blank rather than acquiring invisible padding.
fn parse_assistant_output(text: &str) -> Vec<Line<'static>> {
    let mut lines = crate::markdown::parse_blocks(text);
    if lines.is_empty() {
        return lines;
    }
    let first_visible = lines
        .iter()
        .position(|line| {
            line.spans
                .iter()
                .any(|span| !span.content.trim().is_empty())
        })
        .unwrap_or(0);
    for (index, line) in lines.iter_mut().enumerate().skip(first_visible) {
        if line.width() == 0 {
            continue;
        }
        let gutter = if index == first_visible {
            Span::styled(
                format!("{} ", theme::DEFAULT_LOGO),
                theme::palette().accent(),
            )
        } else {
            Span::raw("  ")
        };
        line.spans.insert(0, gutter);
    }
    lines
}

/// Compact completed-reasoning row. Two decimal places are always shown so a
/// short phase reads `0.07s`, not a lossy whole-second approximation.
fn reasoning_summary_line(elapsed: Option<Duration>, expanded: bool) -> Line<'static> {
    let p = theme::palette();
    let arrow = if expanded { "▾ " } else { "▸ " };
    let label = match elapsed {
        Some(elapsed) => format!("thought for {:.2}s", elapsed.as_secs_f64()),
        None => "thought".to_string(),
    };
    Line::from(vec![
        Span::styled(arrow.to_string(), p.accent()),
        Span::styled(label, p.chrome()),
    ])
}

/// The only transcript representation of reasoning while it is still
/// streaming. It is deliberately content-free and cannot expand until the
/// phase completes and its precise duration is frozen.
fn live_reasoning_line() -> Line<'static> {
    Line::styled("Marathoning", theme::palette().chrome())
}

/// The live reasoning placeholder is a compact animated label. No reasoning
/// text, logo, or multi-row art is materialized.
fn animated_live_reasoning_line(now: Instant, started: Option<Instant>) -> Line<'static> {
    Line::from(marathoning_spans(now, started))
}

fn reasoning_body_lines(reasoning: &str) -> Vec<Line<'static>> {
    let style = theme::palette().reasoning();
    reasoning
        .split('\n')
        .map(|line| Line::styled(format!("  {line}"), style))
        .collect()
}

pub(crate) fn draw(frame: &mut Frame, state: &mut ViewState) {
    // Elapsed-time animations (spinner, timer, caret blink) use `now` against
    // their own epochs, so they advance at wall-clock rate whatever the render
    // loop's frequency happens to be — no per-frame accumulator needed.
    let now = Instant::now();

    // The transcript and composer own the full terminal width. Turn dividers
    // deliberately reach both edges; status chrome can keep the quiet outer
    // margin on very wide screens.
    let frame_area = frame.area();
    let area = content_area(frame_area);
    // `/menu` is a modal: it takes the whole frame and the whole keymap, so
    // nothing behind it can be typed into by accident.
    if let Some(menu) = &state.menu_overlay {
        state.reasoning_hitboxes.clear();
        state.tool_hitboxes.clear();
        frame.render_widget(Clear, area);
        draw_menu_overlay(frame, menu, area, now);
        // A modal owns the screen; a selection made behind it would highlight
        // cells that are no longer there.
        state.selection = None;
        state.selection_text = None;
        return;
    }
    // The composer wraps long prompts instead of clipping them: its box grows
    // with the typed text up to MAX_COMPOSER_ROWS inner rows, then scrolls
    // internally so the caret (always at the end) stays pinned to the bottom.
    // Short input is a single row → the familiar three-row box. A pending
    // permission ask needs two lines, so it keeps a fixed four-row box.
    let bottom = if state.pending_ask.is_some() {
        Constraint::Length(4)
    } else {
        let inner_w = frame_area.width.saturating_sub(2).max(1);
        let rows = composer_rows(&composer_display_text(state), inner_w).max(1) as u16;
        Constraint::Length(rows.min(MAX_COMPOSER_ROWS) + 2)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        // A one-row blank gap between the transcript and the status/composer
        // chrome, so the last line of output has breathing room above the box
        // instead of butting directly against the status bar.
        .constraints([
            Constraint::Min(1),    // transcript
            Constraint::Length(1), // blank breathing room
            Constraint::Length(1), // status bar
            bottom,                // composer / ask
            Constraint::Length(1), // model footer
        ])
        .split(frame_area);
    draw_transcript(frame, state, chunks[0], now);
    frame.render_widget(Clear, chunks[1]); // keep the gap blank across frames
    draw_status(frame, state, content_area(chunks[2]), now);
    if let Some(ask) = &state.pending_ask {
        draw_ask(frame, ask, chunks[3]);
    } else {
        draw_composer(frame, state, chunks[3], now);
        // The completion menu floats above the composer, as a popup.
        if let Some(menu) = &state.menu {
            draw_menu(frame, menu, chunks[3]);
        }
    }
    draw_model_footer(frame, state, chunks[4]);
    // Last, over everything: the selection highlight is applied to the
    // finished buffer, and the text is read back out of it. Doing this after
    // every widget has drawn is what makes it work uniformly across the
    // transcript, the status bar and the composer without any of them knowing
    // that selection exists.
    apply_selection(frame, state);
}

/// Reverse-video the selected cells and harvest their text.
///
/// The rendered buffer is the source of truth: by this point wrapping,
/// markdown styling, and tool/reasoning collapsing have all happened, so the
/// glyphs on screen are the only faithful representation of what the user
/// dragged across. Reading them back means "copy" always matches what the eye
/// selected, with no second, divergent path through the transcript model.
fn apply_selection(frame: &mut Frame, state: &mut ViewState) {
    let Some(selection) = state.selection else {
        state.selection_text = None;
        return;
    };
    if selection.is_empty() {
        state.selection_text = None;
        return;
    }
    let area = frame.area();
    let (start, end) = selection.ordered();
    let buf = frame.buffer_mut();
    let mut text = String::new();

    for row in start.1..=end.1.min(area.height.saturating_sub(1)) {
        // Linear (flow) selection, not a rectangle: the first row runs from
        // the anchor to the edge, whole rows follow, and the last stops at the
        // pointer — the same shape a text editor or browser gives you, which
        // is what makes multi-line copy read correctly.
        let from = if row == start.1 { start.0 } else { 0 };
        let to = if row == end.1 {
            end.0
        } else {
            area.width.saturating_sub(1)
        };
        let mut line = String::new();
        for col in from..=to.min(area.width.saturating_sub(1)) {
            let cell = &mut buf[(col, row)];
            line.push_str(cell.symbol());
            cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
        }
        // Trailing padding is an artifact of the terminal grid, never content.
        text.push_str(line.trim_end());
        if row < end.1 {
            text.push('\n');
        }
    }
    state.selection_text = (!text.trim().is_empty()).then_some(text);
}

fn draw_transcript(frame: &mut Frame, state: &mut ViewState, area: Rect, now: Instant) {
    let h = area.height as usize;
    let w = area.width;
    state.area_height = h;
    state.reasoning_hitboxes.clear();
    state.tool_hitboxes.clear();

    // Welcome card before the first turn: the brand logo on the left, the
    // model + cwd + key hints on the right, all in one bordered box. It lives
    // only in the pre-conversation state — the moment a prompt is submitted
    // (or anything streams, or a turn goes busy) the transcript takes over and
    // the card is gone. `/clear` brings it back by emptying the transcript.
    if state.welcome_card_visible() {
        frame.render_widget(Clear, area);
        draw_welcome_card(frame, area);
        return;
    }

    // Refresh the cached parse of the in-progress turn at most once per token
    // delta, then reuse it across frames. `current_dirty` is set by each
    // `Text`/`Reasoning` event; without this guard, parsing ran on the whole
    // growing buffer every frame (and again in `scroll_indicator`) — O(n) per
    // frame, the source of jank on long streaming replies. The second arm
    // (`stream non-empty but cache empty`) catches paths that set the buffers
    // without marking them dirty (notably render tests) — a non-empty buffer
    // always parses to ≥1 line, so an empty cache while a buffer exists is
    // stale. The cache holds the reasoned-then-answered tail ([`parse_live`]).
    let has_stream = !state.current_text.is_empty() || !state.current_reasoning.is_empty();
    if state.current_dirty || (has_stream && state.current_parsed.is_empty()) {
        if !has_stream {
            state.current_parsed.clear();
        } else {
            state.current_parsed = parse_live(
                &state.current_reasoning,
                &state.current_text,
                state.reasoning_elapsed,
            );
        }
        state.current_dirty = false;
    }

    // The trailing lines below the cached transcript. Completed turns above
    // are already parsed; only this tail is live. It is one of three things,
    // in priority order:
    //   1. the in-progress assistant turn (reasoning + answer) — served from
    //      the cache above, so a frame with no new token costs nothing;
    //   2. a live animated line while a turn is in progress but producing no
    //      output yet — a "thinking" spinner while waiting for the first token
    //      (TTFT), or a "running <tool>" spinner while a tool executes. This
    //      is what makes a slow model or a long Bash call feel alive instead
    //      of hung;
    //   3. nothing, while idle.
    //
    // We don't materialize the whole tail into a Vec (that would clone every
    // streaming line each frame — O(n) on long replies). The count is enough to
    // compute the visible window; the loop below clones only the rows in it.
    let live_lines: Vec<Line<'static>> = if !has_stream && state.busy {
        if state.running > 0 {
            tool_running_lines(state, now)
        } else {
            thinking_lines(state, now)
        }
    } else {
        Vec::new()
    };
    let stream_len = if has_stream {
        state.current_parsed.len()
    } else {
        live_lines.len()
    };
    let tr_len = state.transcript.len();
    let total = tr_len + stream_len;

    // The visible window: pinned to the bottom when following, else the user's
    // held position. Only the visible rows are cloned, not the whole transcript
    // (huge at scale) — same property as the pre-scroll code.
    let start = if state.follow {
        total.saturating_sub(h)
    } else {
        state.scroll_top.min(total.saturating_sub(h))
    };
    let end = (start + h).min(total);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(end - start);
    for i in start..end {
        if i < tr_len {
            let line = &state.transcript[i];
            lines.push(full_width_turn_divider(line, w).unwrap_or_else(|| line.clone()));
        } else if has_stream {
            let stream_index = i - tr_len;
            if stream_index == 0
                && !state.current_reasoning.is_empty()
                && state.reasoning_elapsed.is_none()
            {
                // Replace only the cached content-free placeholder. This tiny
                // line can animate every frame without re-parsing a growing
                // markdown answer on every frame.
                lines.push(animated_live_reasoning_line(
                    now,
                    state.reasoning_started.or(state.turn_started),
                ));
            } else {
                lines.push(state.current_parsed[stream_index].clone());
            }
        } else {
            // The live loader block (thinking/tool). It's the only streaming
            // content, so its rows follow the transcript directly.
            lines.push(live_lines[i - tr_len].clone());
        }
    }
    // Box-drawn Markdown tables have a useful natural width, but allowing a
    // wider table to pass through Paragraph's ordinary word wrapper tears the
    // border apart: each logical row wraps independently, so separators no
    // longer line up. Keep the full box whenever it fits. On narrower screens,
    // constrain its columns and wrap text inside each cell while rebuilding the
    // box at the exact viewport width.
    // Preserve the originating transcript index when one logical table/prose
    // row becomes several pre-wrapped physical rows. Hit-testing below needs
    // that mapping; `start + offset` is no longer valid after responsive
    // expansion.
    let mut line_sources: Vec<usize> = (start..end).collect();
    if w > 0 {
        let mut fitted = Vec::new();
        let mut fitted_sources = Vec::new();
        for (source, line) in line_sources.into_iter().zip(lines) {
            let rows = responsive_content_lines(line, w);
            fitted_sources.extend(std::iter::repeat_n(source, rows.len()));
            fitted.extend(rows);
        }
        lines = fitted;
        line_sources = fitted_sources;
    }
    // Capture wrapped-row geometry before `lines` moves into the Paragraph.
    // Each tuple is (retained block, physical row, wrapped height, label width).
    let mut reasoning_rows: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut tool_rows: Vec<(usize, usize, usize, usize)> = Vec::new();
    if w > 0 {
        let mut physical_row = 0usize;
        for (offset, line) in lines.iter().enumerate() {
            let row_count = Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(w)
                .max(1);
            let global_index = line_sources[offset];
            if global_index < tr_len {
                if let Some(block_index) = state
                    .reasoning_blocks
                    .iter()
                    .position(|block| block.summary_index == global_index)
                {
                    reasoning_rows.push((block_index, physical_row, row_count, line.width()));
                }
                if let Some(block_index) = state
                    .tool_blocks
                    .iter()
                    .position(|block| block.summary_index == global_index)
                {
                    tool_rows.push((block_index, physical_row, row_count, line.width()));
                }
            }
            physical_row = physical_row.saturating_add(row_count);
        }
    }
    // Blank the area first. `Paragraph` only writes the cells its (wrapped)
    // text covers, and ratatui double-buffers — it diffs against the previous
    // frame rather than clearing — so a cell the new frame doesn't touch keeps
    // last frame's glyph. When a long dim tool-preview line wraps to N rows and
    // a one-line scroll changes the layout, the tail of the previous word
    // isn't overwritten and stays put: "portions of some words scrolling, the
    // rest stuck." Clearing gives each frame a clean slate.
    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    // Wrapping can make the slice taller than the area: a long line spans
    // several visual rows, so `h` logical lines can occupy more than `h` visual
    // rows. `Paragraph` renders top-down and clips the bottom, so without help
    // the *newest* line — the last in the slice, right above the composer — is
    // the one that gets cut off. When following, scroll the excess rows off the
    // top so the content's bottom pins to the area's bottom. `line_count` runs
    // ratatui's own `WordWrapper`, so the count matches the render exactly.
    // When held (scrolled up), leave the slice top-aligned — the user put
    // `scroll_top` at the top on purpose.
    let wrapped_rows = paragraph.line_count(w);
    let scroll_y = if state.follow {
        wrapped_rows.saturating_sub(h).min(u16::MAX as usize) as u16
    } else {
        0
    };
    // Reconstruct each logical line's physical wrapped rows using the same
    // `Paragraph` + `Wrap` configuration as the real render. This makes the
    // click target land on the label even when earlier transcript lines wrap
    // or the bottom-pinned paragraph scrolls excess physical rows off the top.
    if w > 0 && h > 0 {
        let viewport_start = scroll_y as usize;
        let viewport_end = viewport_start.saturating_add(h);
        for (block_index, physical_row, row_count, label_width) in reasoning_rows {
            let line_end = physical_row.saturating_add(row_count);
            let clipped_start = physical_row.max(viewport_start);
            let clipped_end = line_end.min(viewport_end);
            if clipped_start < clipped_end {
                state.reasoning_hitboxes.push(ReasoningHitbox {
                    block_index,
                    x: area.x,
                    y: area
                        .y
                        .saturating_add(clipped_start.saturating_sub(viewport_start) as u16),
                    width: label_width.min(w as usize).max(1) as u16,
                    height: clipped_end.saturating_sub(clipped_start) as u16,
                });
            }
        }
        for (block_index, physical_row, row_count, label_width) in tool_rows {
            let line_end = physical_row.saturating_add(row_count);
            let clipped_start = physical_row.max(viewport_start);
            let clipped_end = line_end.min(viewport_end);
            if clipped_start < clipped_end {
                state.tool_hitboxes.push(ToolHitbox {
                    block_index,
                    x: area.x,
                    y: area
                        .y
                        .saturating_add(clipped_start.saturating_sub(viewport_start) as u16),
                    width: label_width.min(w as usize).max(1) as u16,
                    height: clipped_end.saturating_sub(clipped_start) as u16,
                });
            }
        }
    }
    frame.render_widget(paragraph.scroll((scroll_y, 0)), area);
}

/// Fit assistant content to the viewport before `Paragraph` sees it.
///
/// Oversized table rows become one or more rows of a constrained boxed table,
/// so cells wrap internally and every border still meets. Other assistant lines
/// are pre-wrapped with a hanging two-cell gutter; ratatui's default wrapper
/// starts continuation rows at column zero and would lose that indentation.
fn responsive_content_lines(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width as usize {
        return vec![line];
    }

    let gutter_len = assistant_gutter_len(&line);
    let gutter_width = if gutter_len == 1 {
        line.spans[0].width()
    } else {
        0
    };
    let table_spans = &line.spans[gutter_len..];
    let content_width = (width as usize).saturating_sub(gutter_width);
    if table_spans.is_empty() || content_width == 0 {
        return vec![line];
    }

    if let Some((left, middle, right, natural_widths, style)) = table_border(table_spans) {
        if let Some(widths) = constrained_table_widths(&natural_widths, content_width) {
            return vec![render_constrained_border(
                line.spans.first().cloned().filter(|_| gutter_len == 1),
                &widths,
                left,
                middle,
                right,
                style,
            )];
        }
    }

    if let Some((cells, natural_widths, separator_style)) = table_cells(table_spans) {
        if let Some(widths) = constrained_table_widths(&natural_widths, content_width) {
            return render_constrained_row(
                line.spans.first().cloned().filter(|_| gutter_len == 1),
                gutter_width,
                cells,
                &widths,
                separator_style,
            );
        }
    }

    if gutter_len == 1 {
        return wrap_assistant_line(line, width as usize, gutter_width);
    }
    vec![line]
}

fn assistant_gutter_len(line: &Line<'static>) -> usize {
    line.spans.first().is_some_and(|span| {
        let content = span.content.as_ref();
        content == "  " || (content.ends_with(' ') && content.trim_end() == theme::DEFAULT_LOGO)
    }) as usize
}

/// Parse `┌──┬──┐` / `├──┼──┤` / `└──┴──┘` and recover each natural cell
/// width (the border segment includes the cell's two padding spaces).
fn table_border(spans: &[Span<'static>]) -> Option<(char, char, char, Vec<usize>, Style)> {
    let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
    let chars: Vec<char> = text.chars().collect();
    let (&left, &right) = (chars.first()?, chars.last()?);
    let middle = match (left, right) {
        ('┌', '┐') => '┬',
        ('├', '┤') => '┼',
        ('└', '┘') => '┴',
        _ => return None,
    };
    let mut widths = Vec::new();
    let mut segment = 0usize;
    for ch in &chars[1..chars.len().saturating_sub(1)] {
        if *ch == '─' {
            segment += 1;
        } else if *ch == middle && segment >= 2 {
            widths.push(segment - 2);
            segment = 0;
        } else {
            return None;
        }
    }
    if segment < 2 {
        return None;
    }
    widths.push(segment - 2);
    Some((
        left,
        middle,
        right,
        widths,
        spans.first().map_or_else(Style::new, |span| span.style),
    ))
}

/// Split `│ cell │ cell │` while retaining inline styles and the natural padded
/// width of every cell.
fn table_cells(spans: &[Span<'static>]) -> Option<(Vec<Vec<Span<'static>>>, Vec<usize>, Style)> {
    if spans.len() < 3
        || spans.first().map(|span| span.content.as_ref()) != Some("│")
        || spans.last().map(|span| span.content.as_ref()) != Some("│")
    {
        return None;
    }

    let separator_style = spans[0].style;
    let mut cells: Vec<Vec<Span<'static>>> = Vec::new();
    let mut widths = Vec::new();
    let mut cell: Vec<Span<'static>> = Vec::new();
    for span in spans.iter().skip(1).cloned() {
        if span.content.as_ref() == "│" {
            let padded_width: usize = cell.iter().map(Span::width).sum();
            widths.push(padded_width.saturating_sub(2));
            trim_cell_spans(&mut cell);
            cells.push(std::mem::take(&mut cell));
        } else {
            cell.push(span);
        }
    }
    (!cells.is_empty()).then_some((cells, widths, separator_style))
}

/// Allocate the exact inner-width budget while favoring the natural size of
/// short identifier columns. Large prose columns absorb the wrapping first.
fn constrained_table_widths(natural: &[usize], table_width: usize) -> Option<Vec<usize>> {
    let columns = natural.len();
    let chrome = columns.checked_mul(3)?.checked_add(1)?; // `│ ` + ` │` per cell
    let budget = table_width.checked_sub(chrome)?;
    if columns == 0 || budget < columns {
        return None;
    }
    if natural.iter().sum::<usize>() <= budget {
        return Some(natural.to_vec());
    }

    let preferred: Vec<usize> = natural.iter().map(|width| (*width).clamp(1, 8)).collect();
    let mut widths = if preferred.iter().sum::<usize>() <= budget {
        preferred
    } else {
        vec![1; columns]
    };
    let desired: Vec<usize> = natural.iter().map(|width| (*width).max(1)).collect();
    let mut remaining = budget.saturating_sub(widths.iter().sum());
    while remaining > 0 {
        let mut progressed = false;
        for (width, target) in widths.iter_mut().zip(&desired) {
            if remaining == 0 {
                break;
            }
            if *width < *target {
                *width += 1;
                remaining -= 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    Some(widths)
}

fn render_constrained_border(
    gutter: Option<Span<'static>>,
    widths: &[usize],
    left: char,
    middle: char,
    right: char,
    style: Style,
) -> Line<'static> {
    let mut border = String::from(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            border.push(middle);
        }
        border.extend(std::iter::repeat_n('─', width + 2));
    }
    border.push(right);
    let mut spans = Vec::with_capacity(2);
    if let Some(gutter) = gutter {
        spans.push(gutter);
    }
    spans.push(Span::styled(border, style));
    Line::from(spans)
}

fn render_constrained_row(
    gutter: Option<Span<'static>>,
    gutter_width: usize,
    cells: Vec<Vec<Span<'static>>>,
    widths: &[usize],
    separator_style: Style,
) -> Vec<Line<'static>> {
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| wrap_styled_spans(cell, *width))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let mut lines = Vec::with_capacity(height);
    for row in 0..height {
        let mut spans = Vec::new();
        if row == 0 {
            if let Some(gutter) = gutter.clone() {
                spans.push(gutter);
            }
        } else if gutter_width > 0 {
            spans.push(Span::raw(" ".repeat(gutter_width)));
        }
        spans.push(Span::styled("│", separator_style));
        for (column, width) in widths.iter().enumerate() {
            spans.push(Span::styled(" ", separator_style));
            let content = wrapped
                .get(column)
                .and_then(|rows| rows.get(row))
                .cloned()
                .unwrap_or_default();
            let used: usize = content.iter().map(Span::width).sum();
            spans.extend(content);
            if used < *width {
                spans.push(Span::raw(" ".repeat(width - used)));
            }
            spans.push(Span::styled(" │", separator_style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Pre-wrap ordinary assistant prose with the same gutter on every physical
/// row. This is the hanging indent ratatui's `Wrap` does not provide.
fn wrap_assistant_line(
    line: Line<'static>,
    width: usize,
    gutter_width: usize,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(gutter_width).max(1);
    let wrapped = wrap_styled_spans(&line.spans[1..], content_width);
    let line_style = line.style;
    let first_gutter = line.spans[0].clone();
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, content)| {
            let mut spans = Vec::with_capacity(content.len() + 1);
            if index == 0 {
                spans.push(first_gutter.clone());
            } else {
                spans.push(Span::raw(" ".repeat(gutter_width)));
            }
            spans.extend(content);
            Line::from(spans).style(line_style)
        })
        .collect()
}

/// Word-wrap styled spans, retaining style boundaries and hard-breaking a
/// single token only when it cannot fit on an empty row.
fn wrap_styled_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut glyphs: Vec<(char, Style, usize)> = Vec::new();
    for span in spans {
        for ch in span.content.chars() {
            let cell_width = Span::raw(ch.to_string()).width().max(1);
            glyphs.push((ch, span.style, cell_width));
        }
    }
    if glyphs.is_empty() {
        return vec![Vec::new()];
    }

    let mut rows = Vec::new();
    let mut start = 0usize;
    while start < glyphs.len() {
        while start < glyphs.len() && glyphs[start].0.is_whitespace() {
            start += 1;
        }
        if start >= glyphs.len() {
            break;
        }

        let mut end = start;
        let mut used = 0usize;
        let mut last_space = None;
        while end < glyphs.len() {
            let next = glyphs[end].2;
            if used + next > width {
                break;
            }
            used += next;
            if glyphs[end].0.is_whitespace() {
                last_space = Some(end);
            }
            end += 1;
        }

        let (cut, mut next_start) = if end == glyphs.len() {
            (end, end)
        } else if let Some(space) = last_space.filter(|space| *space > start) {
            (space, space + 1)
        } else if end > start {
            (end, end)
        } else {
            (start + 1, start + 1)
        };
        while next_start < glyphs.len() && glyphs[next_start].0.is_whitespace() {
            next_start += 1;
        }
        rows.push(glyphs_to_spans(&glyphs[start..cut]));
        start = next_start;
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

fn glyphs_to_spans(glyphs: &[(char, Style, usize)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (ch, style, _) in glyphs {
        if let Some(last) = spans.last_mut().filter(|span| span.style == *style) {
            last.content.to_mut().push(*ch);
        } else {
            spans.push(Span::styled(ch.to_string(), *style));
        }
    }
    spans
}

/// Remove table padding across span boundaries without losing cell styling.
fn trim_cell_spans(spans: &mut Vec<Span<'static>>) {
    while spans
        .first()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.remove(0);
    }
    if let Some(first) = spans.first_mut() {
        let trimmed = first.content.trim_start().to_string();
        first.content = trimmed.into();
    }

    while spans
        .last()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.pop();
    }
    if let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_string();
        last.content = trimmed.into();
    }
}

/// The pre-conversation welcome card: the brand logo (an adaptive half-block
/// raster of `logo.svg`) on the left, and the model + cwd + key hints
/// on the right in one unbordered horizontal layout. A real `Layout` keeps
/// both columns aligned at any terminal width —
/// the info column wraps within its space on narrow terminals instead of
/// breaking. Sized to the taller of the eight-row logo and wrapped info.
///
/// Deliberately static: motion here would compete with the composer for
/// attention on an idle screen. In-turn motion uses only the one-cell logo.
fn draw_welcome_card(frame: &mut Frame, area: Rect) {
    let p = theme::palette();

    // Key hints, one grammar throughout: a key, then an imperative verb. The
    // keys are styled as a column of their own so the eye can scan down them
    // without reading the labels. `Shift+Tab` names the modes it cycles —
    // it is the only binding here whose effect a new user cannot guess.
    let hints: &[(&str, &str)] = &[
        ("@", "mention a file"),
        ("/", "run a command — /help lists them"),
        (
            "Shift+Tab",
            "switch mode — default, accept edits, plan, ask, auto",
        ),
        ("Alt+↑ ↓", "recall an earlier prompt"),
        ("Ctrl+C", "quit"),
    ];

    let lines: Vec<Line<'static>> = vec![
        // One mark, one name. The glyph *is* the brand element, so the name
        // does not need a second one beside it.
        Line::from(vec![
            Span::styled(format!("{} ", theme::logo_glyph()), p.accent()),
            Span::styled("Subconscious Code", p.accent()),
        ]),
        // Keep the eight-row intro aligned with the eight-row logo after the
        // model moves to its persistent footer beneath the composer.
        Line::default(),
        Line::default(),
    ]
    .into_iter()
    .chain(hints.iter().map(|(key, label)| {
        Line::from(vec![
            Span::styled(format!("  {key:<11}"), p.code()),
            Span::styled((*label).to_string(), p.body()),
        ])
    }))
    .collect();

    // Use the highest-resolution SVG raster that leaves enough room for the
    // onboarding copy. Narrow or short terminals keep the compact glyph,
    // since clipping a large logo would make both columns less useful.
    let splash = welcome_logo(area);
    let splash_width = splash.map_or(0, |logo| logo.width);
    let info_width = area.width.saturating_sub(splash_width).max(1);
    let info_height = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(info_width) as u16;
    let splash_height = splash.map_or(0, |logo| logo.height);

    // The start screen belongs at the top, where a terminal session begins.
    // Conversation content replaces it as soon as the first turn starts.
    let card_h = info_height.max(splash_height).min(area.height);
    let card = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: card_h,
    };
    // No border. The indent alone groups it, which leaves the box outline free
    // to mean "this is the interactive element" — see `draw_composer`.
    if let Some(logo) = splash {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(splash_width), Constraint::Min(1)])
            .split(card);
        // Borrow the embedded raster directly: no SVG parsing, file read, or
        // per-frame String/Line allocation on the startup screen.
        frame.render_widget(Paragraph::new(logo.art).style(p.accent()), columns[0]);
        let info_area = Rect {
            x: columns[1].x,
            y: columns[1]
                .y
                .saturating_add(columns[1].height.saturating_sub(info_height)),
            width: columns[1].width,
            height: info_height.min(columns[1].height),
        };
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info_area);
    } else {
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), card);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WelcomeLogo {
    art: &'static str,
    width: u16,
    height: u16,
}

/// Select the densest faithful `logo.svg` raster the start screen can show.
/// Width includes two blank cells between the image and onboarding copy.
fn welcome_logo(area: Rect) -> Option<WelcomeLogo> {
    const MIN_INFO_WIDTH: u16 = 46;
    const GAP: u16 = 2;
    const LOGO: WelcomeLogo = WelcomeLogo {
        art: theme::LOGO_ART_SMALL,
        width: 16 + GAP,
        height: 8,
    };

    (area.height >= LOGO.height && area.width >= LOGO.width + MIN_INFO_WIDTH).then_some(LOGO)
}

fn draw_status(frame: &mut Frame, state: &ViewState, area: Rect, now: Instant) {
    let p = theme::palette();

    // Left side: the primary, at-a-glance facts. Built from spans so the parts
    // that matter (model, mode, the headline numbers) read in the default
    // foreground while the chrome (separators, labels) stays dim. Middle-dot
    // separators read lighter than `|` and let the line breathe. The mode is
    // semantic-colored: green = accept-edits, cyan = plan, red = bypass
    // (dangerous), so Shift+Tab's state is unmistakable.
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" "));
    // Active work gets a live state prefix. At idle there is deliberately no
    // permanent indicator; the location becomes the first status-bar field.
    if state.busy {
        // Once a turn crosses a second, replace "working" with its elapsed
        // time so long-running work stays legible at a glance.
        let secs = elapsed_secs(now, state.turn_started);
        let activity = if secs >= 1 {
            format!("{secs}s")
        } else {
            "working".to_string()
        };
        spans.push(logo_spinner_span(now, state.turn_started));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(activity, p.accent()));
        spans.push(Span::styled(" · ", p.chrome()));
    }
    // Where you are, not what model you're on. The model has its own footer
    // below the composer; the location is the thing that scrolls off the
    // top of the transcript and never comes back, so it earns the persistent
    // slot.
    spans.push(Span::styled(state.location.clone(), p.body()));
    spans.push(Span::styled(" · ", p.chrome()));
    spans.push(Span::styled(
        mode_name(state.mode).to_string(),
        mode_style(state.mode),
    ));
    // One context metric: a preflight estimate before the request goes out,
    // replaced by the authoritative prompt-token count returned with usage.
    // Character counts and cumulative billed tokens are deliberately omitted;
    // neither describes the model context as directly as `prompt_tokens`.
    if let Some(tokens) = state.context_tokens {
        spans.push(Span::styled(" · ctx: ", p.chrome()));
        if state.context_tokens_estimated {
            spans.push(Span::styled("~", p.chrome()));
        }
        spans.push(Span::styled(human_count(tokens as usize), p.body()));
        spans.push(Span::styled(" tok", p.chrome()));
        // Compact on purpose: at the capped content width the state line has
        // to leave room for the right-hand indicator, and "(62.0% cache hit)"
        // spent eleven columns saying what "62% cached" says.
        if let Some(rate) = state.cache_hit_rate {
            spans.push(Span::styled(
                format!(" · {} cached", human_percent(rate)),
                p.chrome(),
            ));
        }
    }
    if let Some(s) = stream_rate_span(state, now) {
        spans.push(s);
    }

    // Right side: a context-sensitive hint. Scrolled up shows the held-view
    // indicator (where the new content is); busy shows the interrupt key;
    // idle shows the discoverability hint. Right-aligned so it parks at the
    // screen edge instead of trailing the left content.
    let right = right_hint(state);
    let line = right_align(spans, right, area.width);
    frame.render_widget(Paragraph::new(line), area);
}

/// Persistent model metadata directly beneath the primary input surface.
fn draw_model_footer(frame: &mut Frame, state: &ViewState, area: Rect) {
    let p = theme::palette();
    let line = Line::from(Span::styled(format!(" {}", state.model_name), p.body()));
    frame.render_widget(Paragraph::new(line), area);
}

/// Use almost the whole terminal. Wide screens keep one quiet column on each
/// side; narrower screens keep every available column for wrapped content.
fn content_area(area: Rect) -> Rect {
    if area.width <= 100 {
        return area;
    }
    Rect {
        x: area.x.saturating_add(1),
        y: area.y,
        width: area.width - 2,
        height: area.height,
    }
}

const TURN_DIVIDER_LEFT_MARKER: &str = "──────── ";
const TURN_DIVIDER_RIGHT_MARKER: &str = " ────────";

/// A compact transcript marker for a completed turn. It is expanded to the
/// current terminal width by [`full_width_turn_divider`] during rendering, so
/// resizes do not leave a stale short rule in history.
pub(crate) fn turn_divider_line(
    duration: String,
    lines_added: usize,
    lines_removed: usize,
) -> Line<'static> {
    let p = theme::palette();
    let changed = lines_added.saturating_add(lines_removed);
    let noun = if changed == 1 { "line" } else { "lines" };
    let label = if changed == 0 {
        vec![Span::styled(
            format!("worked for {duration} · 0 lines changed"),
            p.accent_dim(),
        )]
    } else {
        vec![
            Span::styled(
                format!("worked for {duration} · {changed} {noun} changed ("),
                p.accent_dim(),
            ),
            Span::styled(format!("+{lines_added}"), p.semantic(Color::Green)),
            Span::styled(" ", p.accent_dim()),
            Span::styled(format!("-{lines_removed}"), p.semantic(Color::Red)),
            Span::styled(")", p.accent_dim()),
        ]
    };
    let mut spans = Vec::with_capacity(label.len() + 2);
    spans.push(Span::styled(TURN_DIVIDER_LEFT_MARKER, p.chrome()));
    spans.extend(label);
    spans.push(Span::styled(TURN_DIVIDER_RIGHT_MARKER, p.chrome()));
    Line::from(spans)
}

/// Recognize our compact turn marker and left-align its label inside a rule
/// that occupies exactly `width` cells. Ordinary transcript lines return
/// `None`.
fn full_width_turn_divider(line: &Line<'static>, width: u16) -> Option<Line<'static>> {
    if line.spans.len() < 3
        || line.spans[0].content.as_ref() != TURN_DIVIDER_LEFT_MARKER
        || line.spans.last()?.content.as_ref() != TURN_DIVIDER_RIGHT_MARKER
        || !line.spans[1].content.starts_with("worked for ")
    {
        return None;
    }

    let width = width as usize;
    let label = &line.spans[1..line.spans.len() - 1];
    let label_width = label.iter().map(Span::width).sum::<usize>();
    if width <= label_width {
        let mut remaining = width;
        let mut clipped = Vec::new();
        for span in label {
            if remaining == 0 {
                break;
            }
            let content = span.content.chars().take(remaining).collect::<String>();
            remaining = remaining.saturating_sub(content.chars().count());
            clipped.push(Span::styled(content, span.style));
        }
        return Some(Line::from(clipped));
    }
    let available = width - label_width;
    if available == 1 {
        let mut spans = label.to_vec();
        spans.push(Span::styled("─", line.spans.last()?.style));
        return Some(Line::from(spans));
    }
    if available == 2 {
        let mut spans = vec![Span::styled("─ ", line.spans[0].style)];
        spans.extend_from_slice(label);
        return Some(Line::from(spans));
    }

    // One rule cell and a space introduce the label; every remaining cell is
    // assigned to the trailing rule so the timing reads from the left edge.
    let right_width = available - 3;
    let mut spans = Vec::with_capacity(label.len() + 2);
    spans.push(Span::styled("─ ", line.spans[0].style));
    spans.extend_from_slice(label);
    spans.push(Span::styled(
        format!(" {}", "─".repeat(right_width)),
        line.spans.last()?.style,
    ));
    Some(Line::from(spans))
}

/// How long the "copied N chars" confirmation holds the status corner.
const COPY_NOTICE_TTL: Duration = Duration::from_secs(3);

/// The right-aligned status hint, chosen for the current state.
fn right_hint(state: &ViewState) -> Vec<Span<'static>> {
    let p = theme::palette();
    // A just-finished copy takes the corner for a beat: an escape sequence
    // leaves no other trace, so this line is the only evidence the clipboard
    // got anything.
    if let Some((msg, at)) = &state.copy_notice {
        if at.elapsed() < COPY_NOTICE_TTL {
            return vec![Span::styled(msg.clone(), p.accent())];
        }
    }
    // Scrolled up dominates: the user is navigating, so the "where am I"
    // indicator earns the corner.
    let indicator = scroll_indicator(state);
    if !indicator.is_empty() {
        // Body, not chrome: where you are in a held transcript is live state,
        // and dim gray is the first thing to disappear on a busy background.
        return vec![Span::styled(indicator, p.body())];
    }
    if state.busy {
        return vec![
            Span::styled("Esc ", p.code()),
            Span::styled("interrupt", p.body()),
        ];
    }
    // A drafted prompt is one Esc from being cleared (not lost — the second
    // Esc, with the line empty, quits), so surface that action while typing;
    // an empty line shows the discoverability hint instead.
    if !state.composer.is_empty() {
        vec![
            Span::styled("Esc ", p.code()),
            Span::styled("clear", p.body()),
        ]
    } else {
        // Nothing to report. The corner stays empty rather than parking
        // onboarding here — `/help` and `Ctrl+C` are on the welcome card,
        // where they belong, and this line is for things that change.
        Vec::new()
    }
}

/// Build a one-line `Line` with `left` content packed to the left and `right`
/// content pinned to the right edge of `width`, filled with spaces between.
/// If the two sides together are wider than `width`, the filler collapses to a
/// single space and the right side wraps off the 1-row area — a graceful
/// degradation on narrow terminals rather than a broken layout.
fn right_align(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let w = width as usize;
    let left_chars: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let right_chars: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let gap = w
        .saturating_sub(left_chars)
        .saturating_sub(right_chars)
        .max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

/// The permission mode's color: neutral for the safe default, green for
/// accept-edits, cyan for plan, red for bypass (it skips all prompts — make it
/// impossible to miss). Routed through the palette so `NO_COLOR` drops to
/// monochrome (the word still reads).
fn mode_style(m: AgentMode) -> Style {
    let p = theme::palette();
    match m {
        AgentMode::Default => p.chrome(),
        AgentMode::AcceptEdits => p.semantic(Color::Green),
        AgentMode::Plan => p.code(),
        // Yellow for ask (cautious, not dangerous) vs red for auto (nothing
        // will stop to confirm) — the two extremes must not look alike.
        AgentMode::Ask => p.semantic(Color::Yellow),
        AgentMode::Auto => p.semantic(Color::Red),
    }
}

/// A live tokens/sec readout while assistant text is streaming — the "speedy"
/// meter. Uses ~4 chars/token (the standard rough estimate) over the pure
/// streaming epoch ([`ViewState::stream_started`]) so tool/TTFT time doesn't
/// drag the rate down. Hidden until ≥1s of streaming so the first second isn't a
/// noisy divisor; a dim "streaming" pulse covers the warm-up.
fn stream_rate_span(state: &ViewState, now: Instant) -> Option<Span<'static>> {
    if !state.busy {
        return None;
    }
    let p = theme::palette();
    // While only reasoning is streaming (no answer text yet), the tok/s meter
    // doesn't apply — it measures answer output. Show a dim "reasoning" pulse
    // alongside the content-free collapsed thinking row.
    if state.current_text.is_empty() {
        if !state.current_reasoning.is_empty() {
            return Some(Span::styled(" ↑ reasoning", p.chrome()));
        }
        return None;
    }
    let started = state.stream_started?;
    let elapsed = now.saturating_duration_since(started).as_secs_f64();
    if elapsed < 1.0 {
        return Some(Span::styled(" ↑ streaming", p.chrome()));
    }
    let tps = (state.stream_chars as f64 / 4.0) / elapsed;
    Some(Span::styled(format!(" ↑ {:.0} tok/s", tps), p.accent()))
}

/// When the user has scrolled up away from the bottom, the held-view
/// indicator: how many lines of new content sit below the held position, or
/// `top` when at the oldest. Empty (nothing shown) when following the bottom.
/// Bare (no leading separator) — the right-aligned status hint adds spacing.
fn scroll_indicator(state: &ViewState) -> String {
    if state.follow {
        return String::new();
    }
    // The trailing-line count mirrors what `draw_transcript` renders: the cached
    // parse of the in-progress text (refreshed earlier this frame), or the one
    // live spinner line while busy, or nothing. Reading the cache here avoids a
    // second full re-parse of the streaming buffer every frame.
    let streaming = if !state.current_text.is_empty() || !state.current_reasoning.is_empty() {
        state.current_parsed.len()
    } else if state.busy {
        1 // the live "thinking"/"running <tool>" spinner line
    } else {
        0
    };
    let total = state.transcript.len() + streaming;
    let h = state.area_height.max(1);
    let top = state.scroll_top.min(total.saturating_sub(h));
    let below = total.saturating_sub(top + h);
    if below == 0 {
        "↑ top".to_string()
    } else {
        format!("↑ {below} below")
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

/// Render a provider cache ratio as a compact percentage. Whole numbers: at
/// the capped status width a tenth of a percent is not worth a column, and the
/// clamp keeps a provider that reports a nonsense ratio from printing "140%".
fn human_percent(rate: f64) -> String {
    format!("{:.0}%", rate.clamp(0.0, 1.0) * 100.0)
}

fn draw_composer(frame: &mut Frame, state: &ViewState, area: Rect, now: Instant) {
    let p = theme::palette();
    // A styled prompt: dim `> `, the typed text, then a caret that blinks
    // (solid for a beat after the last keystroke, then toggling between the
    // bright accent and a dim block so its position never disappears). Built
    // from spans so the caret can carry its own style. When the composer is
    // empty, show a dim placeholder hint after the caret instead of a bare
    // cursor — the standard "type here" affordance, so an empty box reads as
    // ready-for-input rather than blank.
    let mut spans: Vec<Span<'static>> = vec![Span::styled("> ", p.chrome())];
    if state.composer.is_empty() {
        spans.push(composer_caret_span(state, now));
        // One phrase, not a menu: `/help` and `@file` are on the welcome
        // card. Chrome-dim placeholder text is the first thing to vanish on a
        // translucent background, so this sits a tier brighter.
        spans.push(Span::styled(" type a prompt", p.accent_dim()));
    } else {
        spans.extend(composer_content_spans(state));
        spans.push(composer_caret_span(state, now));
    }
    // Wrap so long prompts don't clip; scroll the excess off the top so the
    // caret — the last span, on the bottom wrapped row — stays visible. This
    // mirrors the transcript's follow-the-bottom logic. `line_count` is measured
    // on a borderless probe: with a `Block`, `line_count` returns the content
    // rows *plus* the 2 border rows, which would inflate `total_rows` and scroll
    // the content off the top. The inner content height is `area.height - 2`
    // (borders), so the probe must match it — content rows only.
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2).max(1) as usize;
    let line = Line::from(spans);
    let total_rows = Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner_w);
    let scroll_y = total_rows.saturating_sub(inner_h).min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(line)
        .wrap(Wrap { trim: false })
        // Untitled on purpose: the `>` prompt and the placeholder already say
        // what the box is, so a "compose" label was redundant chrome.
        // Accented border. It is the only interactive element on screen, and
        // the welcome card no longer draws one, so the outline now means
        // "type here" rather than just "a box".
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(p.accent_dim()),
        );
    frame.render_widget(paragraph.scroll((scroll_y, 0)), area);
}

/// Render the composer without materializing multiline paste bodies. Invalid
/// markers are ignored defensively and their underlying text is shown.
fn composer_content_spans(state: &ViewState) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for marker in &state.paste_markers {
        if marker.start < offset
            || marker.end <= marker.start
            || marker.end > state.composer.len()
            || !state.composer.is_char_boundary(marker.start)
            || !state.composer.is_char_boundary(marker.end)
        {
            continue;
        }
        if marker.start > offset {
            spans.push(Span::raw(state.composer[offset..marker.start].to_string()));
        }
        spans.push(Span::styled(
            format!("[pasted {} lines]", marker.lines),
            theme::palette().chrome(),
        ));
        offset = marker.end;
    }
    if offset < state.composer.len() {
        spans.push(Span::raw(state.composer[offset..].to_string()));
    }
    spans
}

fn composer_display_text(state: &ViewState) -> String {
    composer_content_spans(state)
        .into_iter()
        .map(|span| span.content)
        .collect()
}

/// Max rows shown in the completion popup. The candidate list in
/// [`CompletionMenu`] may be longer; the menu window-clips to this many.
const MENU_ROWS: usize = 8;

/// Max inner rows the composer grows to before it scrolls internally. Caps
/// the box at `MAX_COMPOSER_ROWS + 2` (borders) so a pasted novel can't eat the
/// whole transcript.
const MAX_COMPOSER_ROWS: u16 = 6;

/// How many wrapped rows the composer text occupies at `width` inner columns.
/// Uses ratatui's own `WordWrapper` (`Paragraph::line_count`) so the count
/// matches what `draw_composer` renders exactly.
fn composer_rows(composer: &str, width: u16) -> usize {
    if width == 0 {
        return 1;
    }
    let content = format!("> {composer}█");
    Paragraph::new(content)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

// ---- motion ----------------------------------------------------------------

/// The one-cell `logo.svg` reduction used when loading animation is disabled.
/// The same tiny mark prefixes assistant output; user prompts stay neutral.
fn logo_glyph() -> &'static str {
    theme::DEFAULT_LOGO
}

/// One-cell phases of the six-petal mark. Cycling these clockwise-ordered
/// silhouettes gives the tiny terminal reduction visible motion without
/// expanding it into an ASCII-art block.
const LOGO_SPINNER_FRAMES: [&str; 8] = ["✻", "✽", "✼", "✽", "✻", "✽", "✼", "✽"];
/// A deliberately quick 22 fps turn; the busy poll loop runs at 125 Hz, so
/// each phase is sampled smoothly without slowing streaming event delivery.
const LOGO_SPINNER_FRAME_MS: u128 = 45;
/// One complete text-shimmer sweep. Kept under a second so the activity reads
/// as brisk without flickering at the TUI's 125 Hz render cadence.
const MARATHON_SHIMMER_PERIOD_MS: u128 = 900;
/// The caret holds solid this long after the last keystroke before it starts
/// to blink, so typing doesn't fight a flicker.
const CARET_HOLD: Duration = Duration::from_secs(1);
/// Caret blink half-period.
const CARET_BLINK_MS: u64 = 530;

/// One fast clockwise turn of the tiny brand mark used for running tools.
fn logo_spinner_span(now: Instant, started: Option<Instant>) -> Span<'static> {
    let p = theme::palette();
    if !theme::animations_enabled() {
        return Span::styled(logo_glyph().to_string(), p.accent());
    }
    let elapsed_ms = started
        .map(|s| now.saturating_duration_since(s).as_millis())
        .unwrap_or(0);
    let frame = (elapsed_ms / LOGO_SPINNER_FRAME_MS) as usize % LOGO_SPINNER_FRAMES.len();
    Span::styled(LOGO_SPINNER_FRAMES[frame].to_string(), p.loading(frame))
}

/// Whole seconds elapsed since a turn's epoch, 0 while idle.
fn elapsed_secs(now: Instant, started: Option<Instant>) -> u64 {
    started
        .map(|s| now.saturating_duration_since(s).as_secs())
        .unwrap_or(0)
}

fn marathoning_label(now: Instant, started: Option<Instant>) -> String {
    format!("Marathoning · {}s", elapsed_secs(now, started))
}

/// Match Codex's text shimmer: one fast sweep with ten characters of
/// off-screen padding on each side and a cosine-softened five-character band.
/// The animation is calculated from the current phase start so every new model
/// request begins with a calm label before the highlight travels across it.
fn marathoning_spans(now: Instant, started: Option<Instant>) -> Vec<Span<'static>> {
    let p = theme::palette();
    let label = marathoning_label(now, started);
    if !theme::animations_enabled() {
        return vec![Span::styled(label, p.chrome())];
    }

    let chars: Vec<char> = label.chars().collect();
    let char_count = chars.len();
    let elapsed = started
        .map(|start| now.saturating_duration_since(start))
        .unwrap_or_default();

    chars
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let intensity = shimmer_intensity(index, char_count, elapsed);
            Span::styled(ch.to_string(), p.shimmer(intensity))
        })
        .collect()
}

fn shimmer_intensity(index: usize, char_count: usize, elapsed: Duration) -> f32 {
    let padding = 10usize;
    let period = char_count + padding * 2;
    let phase = (elapsed.as_millis() % MARATHON_SHIMMER_PERIOD_MS) as f32
        / MARATHON_SHIMMER_PERIOD_MS as f32;
    let pos = (phase * period as f32) as isize;
    let char_pos = index as isize + padding as isize;
    let band_half_width = 5.0;
    let distance = (char_pos - pos).abs() as f32;
    if distance <= band_half_width {
        let x = std::f32::consts::PI * (distance / band_half_width);
        0.5 * (1.0 + x.cos())
    } else {
        0.0
    }
}

/// Waiting for the first token uses only the animated, timed label.
fn thinking_lines(state: &ViewState, now: Instant) -> Vec<Line<'static>> {
    let started = state.reasoning_started.or(state.turn_started);
    vec![Line::from(marathoning_spans(now, started))]
}

/// A running tool stays on one compact line: the same tiny spinner plus its
/// tool name and elapsed time. It never revives the old multi-row loader.
fn tool_running_lines(state: &ViewState, now: Instant) -> Vec<Line<'static>> {
    let p = theme::palette();
    let tool = state.running_tool.as_deref().unwrap_or("tool");
    let secs = elapsed_secs(now, state.turn_started);
    let label = if secs > 0 {
        format!("{tool}… {secs}s")
    } else {
        format!("{tool}…")
    };
    vec![Line::from(vec![
        Span::raw("  "),
        logo_spinner_span(now, state.turn_started),
        Span::styled(format!(" {label}"), p.chrome()),
    ])]
}

/// The composer caret as a styled span. Solid (accent) right after a keystroke
/// or when motion is off; otherwise blinking between accent and a dim block so
/// the caret's position is always visible.
fn composer_caret_span(state: &ViewState, now: Instant) -> Span<'static> {
    let p = theme::palette();
    let since_input = state
        .last_input
        .map(|t| now.saturating_duration_since(t))
        .unwrap_or(Duration::from_secs(60));
    if since_input < CARET_HOLD || !theme::animations_enabled() {
        return Span::styled("█".to_string(), p.accent());
    }
    let ms = now
        .saturating_duration_since(state.process_started)
        .as_millis() as u64;
    let on = (ms / CARET_BLINK_MS).is_multiple_of(2);
    if on {
        Span::styled("█".to_string(), p.accent())
    } else {
        Span::styled("█".to_string(), p.chrome())
    }
}

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
    let area = Rect {
        x,
        y,
        width,
        height: h,
    };

    let visible_rows = h.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible_rows);
    let title = match menu.completion.kind {
        crate::complete::MenuKind::File => "files",
        crate::complete::MenuKind::Slash => "commands",
    };
    let selected = menu.selected.min(candidates.len().saturating_sub(1));
    let start = selected
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(candidates.len().saturating_sub(visible_rows));
    for (i, cand) in candidates.iter().enumerate().skip(start).take(visible_rows) {
        let marker = if i == selected { "▶ " } else { "  " };
        let line = Line::from(format!("{marker}{cand}"));
        if i == selected {
            lines.push(line.style(theme::palette().menu_selected()));
        } else {
            lines.push(line.style(theme::palette().code()));
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
    // A two-line permission prompt with visual hierarchy: the warning glyph +
    // tool name lead (yellow + accent), the reason is dim chrome, and each
    // answer key is colored by its consequence — green for the permissive
    // once/always, cyan for session, red for deny — so the choice reads at a
    // glance and the dangerous one stands out. The key labels keep their exact
    // "[y]once" form so the render test's substring check still holds.
    let p = theme::palette();
    let header = Line::from(vec![
        Span::styled("⚠ ", p.semantic(Color::Yellow)),
        Span::styled(ask.tool.clone(), p.accent()),
        Span::styled(": ".to_string(), p.chrome()),
        Span::styled(ask.reason.clone(), p.chrome()),
    ]);
    let keys = Line::from(vec![
        Span::styled(" [y]once  ", p.semantic(Color::Green)),
        Span::styled("[s]ession  ", p.semantic(Color::Cyan)),
        Span::styled("[a]lways  ", p.semantic(Color::Green)),
        Span::styled("[n]o", p.semantic(Color::Red)),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(vec![header, keys]))
            .block(Block::default().borders(Borders::ALL).title("permission")),
        area,
    );
}

// ---- the /menu overlay ------------------------------------------------------

/// Render the `/menu` modal: a titled box holding the current page's rows, a
/// help line, and any status message.
///
/// Every page is the same shape — heading, rows, footer — so the pages differ
/// only in how a [`Row`] becomes text ([`menu_row_line`]). The selected row is
/// marked with `>` *and* reversed styling, so it reads under `NO_COLOR` too.
fn draw_menu_overlay(frame: &mut Frame, menu: &MenuState, area: Rect, now: Instant) {
    let p = theme::palette();
    let block = Block::default().borders(Borders::ALL).title(" menu ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    // Indented to the same column as a row's text (past the "> " marker).
    lines.push(Line::styled(
        format!("  {}", menu_heading(menu)),
        p.accent(),
    ));
    lines.push(Line::default());

    let rows = menu.rows();
    if rows.is_empty() {
        // Only reachable with no sessions on disk yet. Say so rather than
        // showing an empty box the user can't tell from a bug.
        lines.push(Line::styled(
            "  no sessions yet — start one and it'll show up here".to_string(),
            p.chrome(),
        ));
    }
    // Keep the selected row on screen: rows beyond the box scroll as a window
    // around the cursor rather than being clipped invisibly.
    // Two heading lines plus three footer lines (spacer + help/editor + keys)
    // are structural. Reserve them before selecting the scroll window so a new
    // setting can never push the controls below the terminal viewport.
    let body_rows = inner.height.saturating_sub(5) as usize;
    let start = menu.selected.saturating_sub(body_rows.saturating_sub(1));
    for (i, row) in rows.iter().enumerate().skip(start).take(body_rows.max(1)) {
        lines.push(menu_row_line(menu, row, i == menu.selected, now));
    }

    lines.push(Line::default());
    if let Some(buf) = &menu.editing {
        // An open editor replaces the help line — while typing, the only
        // relevant keys are Enter and Esc. The API key is masked: a secret
        // typed in clear is a shoulder-surfing leak.
        let shown: String = if menu.editing_api_key {
            "•".repeat(buf.chars().count())
        } else {
            buf.clone()
        };
        lines.push(Line::from(vec![
            Span::styled("  > ".to_string(), p.accent()),
            Span::styled(shown, p.body()),
            Span::styled("█".to_string(), p.accent()),
        ]));
        let hint = if menu.editing_api_key {
            "  api key · ↵ save · Esc cancel"
        } else {
            "  ↵ save · Esc cancel"
        };
        lines.push(Line::styled(hint.to_string(), p.chrome()));
    } else {
        if let Some(field) = menu.current_field() {
            lines.push(Line::styled(format!("  {}", field.help), p.chrome()));
        }
        lines.push(Line::styled(format!("  {}", menu_help(menu)), p.chrome()));
    }
    if let Some(status) = &menu.status {
        lines.push(Line::styled(format!("  {status}"), p.accent_dim()));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The page's heading line.
fn menu_heading(menu: &MenuState) -> String {
    match &menu.page {
        MenuPage::Root => format!("{} Subconscious Code", theme::DEFAULT_LOGO),
        MenuPage::Projects => format!("projects ({})", menu.projects.len()),
        MenuPage::Sessions(dir) => match menu.project(dir) {
            Some(proj) => format!(
                "{} — {}",
                proj.name(),
                plural(proj.sessions.len(), "session")
            ),
            None => dir.display().to_string(),
        },
        MenuPage::Settings => "settings".to_string(),
    }
}

/// The per-page key hints.
fn menu_help(menu: &MenuState) -> &'static str {
    match menu.page {
        MenuPage::Root => "↑↓ move · ↵ select · Esc close",
        MenuPage::Projects => "↑↓ move · ↵ open · ← back · r refresh · Esc close",
        MenuPage::Sessions(_) => "↑↓ move · ↵ resume · ← back · Esc close",
        MenuPage::Settings => "↑↓ move · ↵ edit/add · ←→ change · d remove model · Esc close",
    }
}

/// One row, rendered for its page.
fn menu_row_line(menu: &MenuState, row: &Row, selected: bool, now: Instant) -> Line<'static> {
    let p = theme::palette();
    let marker = if selected { "> " } else { "  " };
    let style = if selected {
        p.menu_selected()
    } else {
        p.body()
    };

    let text = match row {
        Row::Goto(MenuPage::Projects) => {
            format!("Projects{:>12}", plural(menu.projects.len(), "project"))
        }
        Row::Goto(MenuPage::Settings) => "Settings".to_string(),
        Row::Goto(_) => "…".to_string(),
        Row::ChangeApiKey => {
            // Never show the key itself — only where the active one came from,
            // so the user can tell whether a save will take effect. Env wins, so
            // a set env var is reported even when a key file also exists.
            let env_set = std::env::var(&menu.settings.api_key_env)
                .ok()
                .filter(|s| !s.is_empty())
                .is_some();
            let source = if env_set {
                format!("(set via ${})", menu.settings.api_key_env)
            } else if rc_config::key_file_path()
                .map(|p| p.exists())
                .unwrap_or(false)
            {
                "(saved, ~/.sc/key)".to_string()
            } else {
                "(unset)".to_string()
            };
            format!("{:<20} {}", "Change API key", source)
        }
        Row::Close => "Close".to_string(),
        Row::Project(dir) => match menu.project(dir) {
            Some(proj) => format!(
                "{:<28} {:>12}  {}",
                ellipsize(&proj.name(), 28),
                plural(proj.sessions.len(), "session"),
                ago(proj.last, now_wall(now)),
            ),
            None => dir.display().to_string(),
        },
        Row::Session(path) => {
            let info = menu
                .projects
                .iter()
                .flat_map(|p| p.sessions.iter())
                .find(|s| &s.path == path);
            match info {
                Some(s) => format!(
                    "{:<11} {}",
                    ago(s.modified, now_wall(now)),
                    ellipsize(s.first_prompt.as_deref().unwrap_or("(no prompt)"), 48),
                ),
                None => path.display().to_string(),
            }
        }
        Row::NewSession(_) => "+ New session here".to_string(),
        Row::Field(i) => match rc_config::edit::EDITABLE.get(*i) {
            Some(f) => {
                let value = f.current(&menu.settings);
                let mut note = String::new();
                // Show where the active model sits in the roster, so ←/→ has
                // something visible to move through ("2/3").
                if f.kind == rc_config::edit::FieldKind::Model && menu.settings.models.len() > 1 {
                    let n = menu.settings.models.len();
                    let at = menu.settings.models.iter().position(|m| *m == value);
                    if let Some(at) = at {
                        note.push_str(&format!("  [{}/{n}]", at + 1));
                    }
                }
                if f.env_override().is_some() {
                    note.push_str(&format!("  (${} overrides)", f.env));
                }
                format!("{:<20} {}{}", f.name, ellipsize(&value, 30), note)
            }
            None => String::new(),
        },
    };
    Line::from(vec![
        Span::styled(marker.to_string(), p.accent()),
        Span::styled(text, style),
    ])
}

/// `SystemTime` for the wall clock. The menu's timestamps are file mtimes
/// (wall time), while animation uses a monotonic `Instant`; `now` is taken as
/// a parameter only so callers stay consistent within one frame.
fn now_wall(_now: Instant) -> std::time::SystemTime {
    std::time::SystemTime::now()
}

/// `"1 session"` / `"4 sessions"`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Shorten to `max` display cells with an ellipsis, on a char boundary.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

fn mode_name(m: AgentMode) -> &'static str {
    match m {
        AgentMode::Default => "default",
        AgentMode::AcceptEdits => "accept-edits",
        AgentMode::Plan => "plan",
        AgentMode::Ask => "ask",
        AgentMode::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render `state` to a 60x10 TestBackend and return the joined cell symbols.
    fn rendered(state: &mut ViewState) -> String {
        rendered_sized(state, 60, 10)
    }

    /// Same as [`rendered`], but at a caller-chosen terminal size — for tests
    /// that need more width than the default 60x10 to avoid clipping a wide
    /// line (the welcome card's logo + info columns, e.g.).
    fn rendered_sized(state: &mut ViewState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, state)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        let mut out = String::new();
        for (i, cell) in buf.content().iter().enumerate() {
            out.push_str(cell.symbol());
            if (i + 1) % w == 0 {
                out.push('\n');
            }
        }
        out
    }

    /// A menu fixture with two projects, so the render tests don't touch the
    /// real `~/.sc`.
    fn sample_menu(page: crate::menu::MenuPage) -> crate::menu::MenuState {
        use crate::menu::{group_projects, MenuState};
        use rc_session::SessionInfo;
        use std::path::PathBuf;
        use std::time::SystemTime;

        let now = SystemTime::now();
        let mk = |id: &str, cwd: &str, age: u64, prompt: &str| SessionInfo {
            path: PathBuf::from(format!("/s/{id}.jsonl")),
            id: id.into(),
            cwd: PathBuf::from(cwd),
            model: "m".into(),
            modified: now - Duration::from_secs(age),
            first_prompt: Some(prompt.into()),
        };
        MenuState {
            page,
            selected: 0,
            projects: group_projects(vec![
                mk(
                    "a",
                    "/home/d/subconscious-code",
                    120,
                    "add a rotating logo to the cli",
                ),
                mk("b", "/home/d/subconscious-code", 3600, "fix the base url"),
                mk("c", "/home/d/dotfiles", 260_000, "why is zsh startup slow"),
            ]),
            settings: rc_config::Settings::load(std::path::Path::new("/nonexistent")),
            editing: None,
            editing_api_key: false,
            status: None,
            pending_outcome: None,
        }
    }

    fn menu_screen(page: crate::menu::MenuPage) -> String {
        let mut state = ViewState::new("gw-glm-5.2".into());
        state.menu_overlay = Some(sample_menu(page));
        rendered_sized(&mut state, 78, 18)
    }

    /// The root page lists the two destinations and the project count, so
    /// `/menu` is navigable without knowing any keys beyond the arrows.
    #[test]
    fn menu_root_lists_projects_and_settings() {
        let screen = menu_screen(crate::menu::MenuPage::Root);
        assert!(screen.contains("Projects"), "projects entry: {screen}");
        assert!(screen.contains("2 projects"), "project count: {screen}");
        assert!(screen.contains("Settings"), "settings entry: {screen}");
        assert!(screen.contains("↵ select"), "key hints: {screen}");
    }

    /// The root menu offers "Change API key" — the one setting that can't live
    /// in `settings.json` — with its source indicator (env / key file / unset).
    #[test]
    fn menu_root_lists_change_api_key() {
        let screen = menu_screen(crate::menu::MenuPage::Root);
        assert!(screen.contains("Change API key"), "entry: {screen}");
        // Exactly one source indicator is shown; which one depends on the
        // test env, so assert the label format rather than a specific source.
        assert!(
            screen.contains("(set via $")
                || screen.contains("(saved, ~/.sc/key)")
                || screen.contains("(unset)"),
            "source indicator: {screen}"
        );
    }

    /// A drag copies exactly the glyphs the user dragged across, including
    /// across a line break, with the grid's trailing padding stripped. The
    /// coordinates are discovered from a first render rather than hardcoded,
    /// so the test doesn't encode the current layout arithmetic.
    #[test]
    fn dragging_across_lines_copies_what_is_on_screen() {
        let mut state = ViewState::new("gw-glm-5.2".into());
        state.transcript.push(Line::raw("alpha one"));
        state.transcript.push(Line::raw("beta two"));
        let screen = rendered_sized(&mut state, 40, 12);
        let rows: Vec<&str> = screen.lines().collect();
        let (ay, ax) = rows
            .iter()
            .enumerate()
            .find_map(|(y, r)| r.find("alpha one").map(|x| (y as u16, x as u16)))
            .expect("first line on screen");
        let (by, bx) = rows
            .iter()
            .enumerate()
            .find_map(|(y, r)| r.find("beta two").map(|x| (y as u16, x as u16)))
            .expect("second line on screen");

        state.selection = Some(Selection {
            anchor: (ax, ay),
            head: (bx + "beta two".len() as u16 - 1, by),
        });
        let _ = rendered_sized(&mut state, 40, 12);
        assert_eq!(
            state.selection_text.as_deref(),
            Some("alpha one\nbeta two"),
            "the harvested text is what was on screen, unpadded"
        );
    }

    /// A press that never moved selects nothing, so a plain click can keep its
    /// existing meaning (expand/collapse a block) without copying.
    #[test]
    fn a_click_harvests_no_text() {
        let mut state = ViewState::new("gw-glm-5.2".into());
        state.transcript.push(Line::raw("alpha one"));
        state.selection = Some(Selection {
            anchor: (2, 2),
            head: (2, 2),
        });
        let _ = rendered_sized(&mut state, 40, 12);
        assert!(state.selection_text.is_none());
    }

    #[test]
    fn mouse_capture_does_not_add_status_chrome() {
        let mut state = ViewState::new("gw-glm-5.2".into());
        let released = right_hint(&state);
        state.mouse_capture = true;
        let captured = right_hint(&state);

        let text = |spans: &[Span<'static>]| {
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(text(&captured), text(&released));
        assert!(!text(&captured).contains("mouse"));
    }

    /// The API-key editor never puts the secret on screen: the buffer renders
    /// as bullets. A key typed in clear is a shoulder-surfing leak, and a
    /// terminal scrollback (or a screenshot) keeps it.
    #[test]
    fn menu_api_key_editor_masks_the_typed_key() {
        let mut state = ViewState::new("gw-glm-5.2".into());
        let mut menu = sample_menu(crate::menu::MenuPage::Root);
        menu.begin_api_key_edit();
        menu.editing = Some("sk-secret-123".into());
        state.menu_overlay = Some(menu);
        let screen = rendered_sized(&mut state, 78, 18);

        assert!(
            !screen.contains("sk-secret-123"),
            "the key must never be rendered: {screen}"
        );
        assert!(
            screen.contains(&"•".repeat("sk-secret-123".len())),
            "expected one bullet per character: {screen}"
        );
        assert!(screen.contains("api key · ↵ save"), "editor hint: {screen}");
    }

    /// Projects are grouped directories with a session count and recency —
    /// the whole point of the page.
    #[test]
    fn menu_projects_page_shows_grouped_directories() {
        let screen = menu_screen(crate::menu::MenuPage::Projects);
        assert!(
            screen.contains("subconscious-code"),
            "project name: {screen}"
        );
        assert!(screen.contains("2 sessions"), "session count: {screen}");
        assert!(screen.contains("dotfiles"), "second project: {screen}");
        assert!(screen.contains("1 session"), "singular for one: {screen}");
    }

    /// The sessions page labels each session by its first prompt and always
    /// offers a fresh start in that directory.
    #[test]
    fn menu_sessions_page_labels_by_prompt_and_offers_new() {
        let screen = menu_screen(crate::menu::MenuPage::Sessions(std::path::PathBuf::from(
            "/home/d/subconscious-code",
        )));
        assert!(
            screen.contains("add a rotating logo"),
            "first prompt as label: {screen}"
        );
        assert!(
            screen.contains("fix the base url"),
            "older session listed: {screen}"
        );
        assert!(
            screen.contains("+ New session here"),
            "new-session row: {screen}"
        );
        // The other project's session must not leak into this one.
        assert!(
            !screen.contains("zsh startup"),
            "only this project's sessions: {screen}"
        );
    }

    /// The settings page shows each field's resolved value, including a mode
    /// that is unset in the file but resolves to `default`.
    #[test]
    fn menu_settings_page_shows_resolved_values() {
        let screen = menu_screen(crate::menu::MenuPage::Settings);
        assert!(screen.contains("model"), "model field: {screen}");
        assert!(screen.contains("base_url"), "base_url field: {screen}");
        assert!(screen.contains("default_mode"), "mode field: {screen}");
        assert!(
            screen.contains("default"),
            "an unset mode still renders its effective value: {screen}"
        );
        assert!(screen.contains("↵ edit"), "edit hint: {screen}");
    }

    /// The menu is modal: while it's open the composer and status bar behind
    /// it are not drawn, so there's no doubt about where a keystroke goes.
    #[test]
    fn menu_replaces_the_normal_chrome_while_open() {
        let screen = menu_screen(crate::menu::MenuPage::Root);
        assert!(
            !screen.contains("compose"),
            "composer hidden behind the modal: {screen}"
        );
        assert!(
            !screen.contains("Ctrl+C quit"),
            "status bar hidden: {screen}"
        );
    }

    /// A menu whose settings carry a multi-model roster.
    fn menu_with_models(models: &[&str]) -> crate::menu::MenuState {
        let mut m = sample_menu(crate::menu::MenuPage::Settings);
        m.settings.model = models[0].to_string();
        m.settings.models = models.iter().map(|s| s.to_string()).collect();
        m
    }

    /// With several saved models the row shows the roster position, so ←/→
    /// has something visible to move through.
    #[test]
    fn settings_model_row_shows_roster_position() {
        let mut state = ViewState::new("m".into());
        state.menu_overlay = Some(menu_with_models(&["a/one", "b/two", "c/three"]));
        let screen = rendered_sized(&mut state, 78, 18);
        assert!(screen.contains("a/one"), "active model shown: {screen}");
        assert!(screen.contains("[1/3]"), "roster position shown: {screen}");
        assert!(screen.contains("d remove model"), "removal hint: {screen}");
    }

    /// A single saved model has no position indicator — "[1/1]" would imply a
    /// list worth cycling when there isn't one.
    #[test]
    fn settings_model_row_omits_position_for_a_lone_model() {
        let mut state = ViewState::new("m".into());
        state.menu_overlay = Some(menu_with_models(&["only/one"]));
        let screen = rendered_sized(&mut state, 78, 18);
        assert!(screen.contains("only/one"), "model shown: {screen}");
        assert!(
            !screen.contains("[1/1]"),
            "no position for a lone model: {screen}"
        );
    }

    /// While adding a model the editor replaces the key hints, so the only
    /// advertised keys are the ones that apply.
    #[test]
    fn settings_editor_shows_the_typed_buffer() {
        let mut m = menu_with_models(&["a/one", "b/two"]);
        m.editing = Some("new/model".into());
        let mut state = ViewState::new("m".into());
        state.menu_overlay = Some(m);
        let screen = rendered_sized(&mut state, 78, 18);
        assert!(screen.contains("new/model"), "typed buffer shown: {screen}");
        assert!(screen.contains("↵ save"), "save hint: {screen}");
        assert!(
            !screen.contains("d remove model"),
            "nav hints hidden while typing: {screen}"
        );
    }

    /// Every mode renders its own name in the status bar — in particular
    /// `auto` (renamed from the old "bypass") and the new `ask`.
    #[test]
    fn status_bar_names_every_mode() {
        for (mode, label) in [
            (AgentMode::Ask, "ask"),
            (AgentMode::Default, "default"),
            (AgentMode::AcceptEdits, "accept-edits"),
            (AgentMode::Plan, "plan"),
            (AgentMode::Auto, "auto"),
        ] {
            let mut state = ViewState::new("m".into());
            state.mode = mode;
            state.transcript.push(Line::from("> hi"));
            let screen = rendered_sized(&mut state, 78, 8);
            assert!(
                screen.contains(label),
                "expected {label:?} in the status bar: {screen}"
            );
        }
    }

    /// "bypass" is gone from the UI entirely — the mode is called `auto` now.
    #[test]
    fn status_bar_never_says_bypass() {
        let mut state = ViewState::new("m".into());
        state.mode = AgentMode::Auto;
        state.transcript.push(Line::from("> hi"));
        let screen = rendered_sized(&mut state, 78, 8);
        assert!(
            !screen.contains("bypass"),
            "the old name must not appear: {screen}"
        );
    }

    /// The composer box carries no title — the `>` prompt and placeholder
    /// already identify it.
    #[test]
    fn composer_box_has_no_title() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("> hi"));
        let screen = rendered_sized(&mut state, 78, 8);
        assert!(
            !screen.contains("compose"),
            "composer title removed: {screen}"
        );
        assert!(
            screen.contains("type a prompt"),
            "placeholder still identifies it: {screen}"
        );
    }

    /// The persistent chrome reports live state: whether sc is working, where
    /// the session is, and which permission mode is armed. The model is *not*
    /// here — it has a footer below the composer, while this slot goes to the
    /// location, which is what scrolls away and never comes back.
    #[test]
    fn status_line_shows_state_location_and_mode() {
        let mut state = ViewState::new("mock-model".into());
        state.mode = AgentMode::Plan;
        state.location = "subconscious-code (main)".into();
        state.busy = true;
        let screen = rendered_sized(&mut state, 78, 10);
        assert!(screen.contains("working"), "busy state: {screen}");
        assert!(screen.contains("plan"), "mode: {screen}");
        assert!(
            screen.contains("subconscious-code (main)"),
            "location: {screen}"
        );
        let status = screen
            .lines()
            .find(|line| line.contains("subconscious-code (main)"))
            .expect("status row");
        assert!(!status.contains("mock-model"), "status row: {status}");
    }

    /// Idle status starts directly with useful session metadata instead of a
    /// permanent state glyph and label.
    #[test]
    fn idle_status_omits_the_state_indicator() {
        let mut state = ViewState::new("m".into());
        state.location = "subconscious-code".into();
        let screen = rendered(&mut state);
        let status = screen
            .lines()
            .find(|line| line.contains("subconscious-code"))
            .expect("status row");
        assert!(!status.contains("● idle"), "idle indicator: {status}");
        assert!(
            status.trim_start().starts_with("subconscious-code"),
            "location should be the first field: {status}"
        );
    }

    /// The context figure is the headline number for this agent, so it has to
    /// actually reach the status bar — and at a readable scale.
    #[test]
    fn status_line_shows_context_size() {
        let mut state = ViewState::new("m".into());
        state.context_tokens = Some(2_748_220);
        state.context_tokens_estimated = true;
        let screen = rendered(&mut state);
        assert!(
            screen.contains("ctx: ~2.7M tok"),
            "estimated tokens: {screen}"
        );
        assert!(
            !screen.contains("12.1M"),
            "character count removed: {screen}"
        );
    }

    /// No context yet (before the first request) means no stale figure shown.
    #[test]
    fn status_line_omits_context_before_the_first_request() {
        let mut state = ViewState::new("m".into());
        assert!(!rendered(&mut state).contains("ctx:"));
    }

    #[test]
    fn human_scales_read_at_a_glance() {
        assert_eq!(human_count(2_748_220), "2.7M");
        assert_eq!(human_percent(0.08125), "8%");
        assert_eq!(human_percent(1.4), "100%", "a nonsense ratio is clamped");
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
        let screen = rendered(&mut state);
        assert!(screen.contains("permission"), "ask block titled: {screen}");
        assert!(
            screen.contains("Edit requires confirmation"),
            "reason: {screen}"
        );
        assert!(screen.contains("[y]once"), "answer keys: {screen}");
    }

    #[test]
    fn live_reasoning_is_hidden_beside_the_answer() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        let started = Instant::now();
        state.begin_reasoning_phase(started);
        state.current_reasoning = "deliberating the approach".into();
        state.finish_reasoning(started + Duration::from_millis(390));
        state.current_text = "the answer".into();
        let screen = rendered(&mut state);
        assert!(
            screen.contains("thought for 0.39s"),
            "completed live phase is one timed row: {screen}"
        );
        assert!(
            !screen.contains("deliberating"),
            "live reasoning content stays hidden: {screen}"
        );
        assert!(screen.contains("the answer"), "answer body: {screen}");
    }

    /// On flush, reasoning collapses to a timed row but its full body remains
    /// available by clicking the row.
    #[test]
    fn flush_text_collapses_reasoning_to_a_timed_expandable_summary() {
        let mut state = ViewState::new("m".into());
        let started = Instant::now();
        state.begin_reasoning_phase(started);
        state.current_reasoning = "deliberating the approach\nconsidering options\n".into();
        state.finish_reasoning(started + Duration::from_millis(2_340));
        state.current_text = "the answer".into();
        state.flush_text_at(started + Duration::from_secs(9));
        let text_lines = |state: &ViewState| -> Vec<String> {
            state
                .transcript
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let collapsed = text_lines(&state);
        assert_eq!(collapsed[0], "▸ thought for 2.34s", "hundredth precision");
        assert!(
            collapsed.iter().all(|l| !l.contains("considering options")),
            "reasoning body starts collapsed: {collapsed:?}"
        );
        assert!(
            collapsed.iter().any(|l| l.contains("the answer")),
            "answer kept"
        );

        let toggle = state
            .toggle_latest_reasoning()
            .expect("a retained reasoning block");
        assert!(toggle.expanded);
        let expanded = text_lines(&state);
        assert_eq!(expanded[0], "▾ thought for 2.34s");
        assert!(
            expanded.iter().any(|l| l.contains("considering options")),
            "full reasoning restored: {expanded:?}"
        );

        let toggle = state
            .toggle_latest_reasoning()
            .expect("block collapses again");
        assert!(!toggle.expanded);
        assert_eq!(
            text_lines(&state),
            collapsed,
            "collapse restores the compact transcript"
        );
    }

    /// Reasoning with no answer text still collapses to one precisely timed row.
    #[test]
    fn flush_text_collapses_reasoning_only_to_one_timed_line() {
        let mut state = ViewState::new("m".into());
        let started = Instant::now();
        state.begin_reasoning_phase(started);
        state.current_reasoning = "thinking hard about it\n".into();
        state.flush_text_at(started + Duration::from_millis(70));
        let lines: Vec<String> = state
            .transcript
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(lines.len(), 1, "just the summary, no answer: {lines:?}");
        assert_eq!(lines[0], "▸ thought for 0.07s");
    }

    #[test]
    fn clicking_a_rendered_thought_label_toggles_that_exact_block() {
        let mut state = ViewState::new("m".into());
        let started = Instant::now();
        state.begin_reasoning_phase(started);
        state.current_reasoning = "first private thought".into();
        state.current_text = "first answer".into();
        state.flush_text_at(started + Duration::from_millis(390));

        let second = started + Duration::from_secs(1);
        state.begin_reasoning_phase(second);
        state.current_reasoning = "second private thought".into();
        state.current_text = "second answer".into();
        state.flush_text_at(second + Duration::from_millis(120));

        let screen = rendered_sized(&mut state, 60, 20);
        assert_eq!(
            state.reasoning_hitboxes.len(),
            2,
            "both visible labels are clickable"
        );
        let first = state.reasoning_hitboxes[0];
        let rendered_row = screen.lines().nth(first.y as usize).unwrap_or("");
        assert!(
            rendered_row.contains("thought for 0.39s"),
            "hitbox is on label: {rendered_row}"
        );
        assert!(
            state
                .toggle_reasoning_at(first.x.saturating_add(first.width), first.y)
                .is_none(),
            "the cell just outside the label is not clickable"
        );

        let toggle = state
            .toggle_reasoning_at(first.x, first.y)
            .expect("clicking the label expands it");
        assert!(toggle.expanded);
        assert!(
            state.reasoning_blocks[0].expanded,
            "clicked first block expanded"
        );
        assert!(
            !state.reasoning_blocks[1].expanded,
            "second block was untouched"
        );
        assert!(
            state.transcript.iter().any(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.contains("first private thought"))
            }),
            "the clicked thought body is restored"
        );
    }

    #[test]
    fn clicking_a_failed_tool_row_expands_and_recovers_the_entire_call() {
        let mut state = ViewState::new("m".into());
        state.transcript = vec![
            Line::from("> run it"),
            Line::from("▸ Bash  cargo test --workspace"),
            Line::from("after"),
        ];
        state.replace_with_tool_block(
            1,
            1,
            Line::from("▸ Bash · 0.39s · failed"),
            Line::from("▾ Bash · 0.39s · failed"),
            vec![
                Line::from("▸ Bash  cargo test --workspace"),
                Line::from("  ✗ error"),
                Line::from("  │ test one failed"),
                Line::from("  │ assertion mismatch"),
            ],
        );

        let screen = rendered_sized(&mut state, 60, 16);
        assert_eq!(state.tool_hitboxes.len(), 1, "completed row is clickable");
        let hitbox = state.tool_hitboxes[0];
        let rendered_row = screen.lines().nth(hitbox.y as usize).unwrap_or("");
        assert!(
            rendered_row.contains("Bash · 0.39s"),
            "hitbox is on tool row: {rendered_row}"
        );
        assert!(
            state
                .toggle_tool_at(hitbox.x.saturating_add(hitbox.width), hitbox.y)
                .is_none(),
            "the cell just outside the row is not clickable"
        );

        let toggle = state
            .toggle_tool_at(hitbox.x, hitbox.y)
            .expect("clicking the tool expands it");
        assert!(toggle.expanded);
        let expanded: Vec<String> = state
            .transcript
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(
            expanded,
            [
                "> run it",
                "▾ Bash · 0.39s · failed",
                "▸ Bash  cargo test --workspace",
                "  ✗ error",
                "  │ test one failed",
                "  │ assertion mismatch",
                "after",
            ],
            "the command and complete result return in place"
        );

        let screen = rendered_sized(&mut state, 60, 16);
        let hitbox = state.tool_hitboxes[0];
        assert!(screen
            .lines()
            .nth(hitbox.y as usize)
            .unwrap_or("")
            .contains("▾ Bash"));
        let toggle = state
            .toggle_tool_at(hitbox.x, hitbox.y)
            .expect("clicking again collapses it");
        assert!(!toggle.expanded);
        assert_eq!(state.transcript.len(), 3);
    }

    #[test]
    fn tool_expansion_reindexes_a_later_thought_block() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("live tool row"));
        state.replace_with_tool_block(
            0,
            1,
            Line::from("▸ Read · 0.12s"),
            Line::from("▾ Read · 0.12s"),
            vec![Line::from("▸ Read  README.md"), Line::from("  │ contents")],
        );

        let started = Instant::now();
        state.begin_reasoning_phase(started);
        state.current_reasoning = "later thought".into();
        state.current_text = "later answer".into();
        state.flush_text_at(started + Duration::from_millis(50));
        let original_index = state.reasoning_blocks[0].summary_index;

        state.toggle_tool(0).expect("tool expands");
        assert_eq!(
            state.reasoning_blocks[0].summary_index,
            original_index + 2,
            "the later thought follows the inserted tool body"
        );
        state.toggle_tool(0).expect("tool collapses");
        assert_eq!(state.reasoning_blocks[0].summary_index, original_index);
    }

    #[test]
    fn hiding_a_successful_tool_reindexes_later_failed_tools_and_thoughts() {
        let mut state = ViewState::new("m".into());
        state.transcript = vec![Line::from("live success"), Line::from("live failure")];
        state.replace_with_tool_block(
            1,
            1,
            Line::from("▸ Bash · 0.12s · failed"),
            Line::from("▾ Bash · 0.12s · failed"),
            vec![Line::from("▸ Bash  false"), Line::from("  │ exit 1")],
        );
        let started = Instant::now();
        state.begin_reasoning_phase(started);
        state.current_reasoning = "later thought".into();
        state.current_text = "later answer".into();
        state.flush_text_at(started + Duration::from_millis(50));
        assert_eq!(state.tool_blocks[0].summary_index, 1);
        assert_eq!(state.reasoning_blocks[0].summary_index, 2);

        state.remove_transcript_range(0, 1);
        assert_eq!(state.tool_blocks[0].summary_index, 0);
        assert_eq!(state.reasoning_blocks[0].summary_index, 1);
    }

    #[test]
    fn reasoning_only_stream_shows_one_content_free_marathoning_row() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        state.current_reasoning = "private chain of thought must stay hidden".into();
        let screen = rendered(&mut state);
        let marathoning = screen
            .lines()
            .find(|line| line.contains("Marathoning ·"))
            .expect("marathoning row");
        assert!(
            !marathoning.contains(theme::DEFAULT_LOGO),
            "marathoning row no longer carries the logo: {marathoning}"
        );
        assert!(
            screen.contains("Marathoning ·") && screen.contains('s'),
            "clear timed activity label: {screen}"
        );
        assert!(
            !screen.contains("private chain"),
            "streamed reasoning content is never rendered: {screen}"
        );
        assert!(
            !screen.contains('▋'),
            "the animated thinking logo must not get a second blinking cursor: {screen}"
        );
    }

    #[test]
    fn tiny_logo_spinner_advances_in_45_milliseconds() {
        let started = Instant::now();
        let first = logo_spinner_span(started, Some(started));
        let next = logo_spinner_span(
            started + Duration::from_millis(LOGO_SPINNER_FRAME_MS as u64),
            Some(started),
        );
        assert_ne!(
            first.content, next.content,
            "one-cell rotation advances rapidly"
        );
        assert_ne!(
            theme::loading_color(0),
            theme::loading_color(1),
            "each loading phase also advances through the warm color ramp"
        );
    }

    #[test]
    fn streaming_answer_has_no_trailing_cursor() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        state.current_text = "answer in progress".into();
        let screen = rendered(&mut state);
        assert!(
            screen.contains("answer in progress"),
            "stream renders: {screen}"
        );
        assert!(!screen.contains('▋'), "no output cursor: {screen}");
    }

    #[test]
    fn transcript_and_streaming_text_render() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("-> Read README.md"));
        state.transcript.push(Line::from("<- Read: # rc"));
        state.current_text = "streaming answer".into();
        let screen = rendered(&mut state);
        assert!(
            screen.contains("-> Read README.md"),
            "tool start line: {screen}"
        );
        assert!(
            screen.contains("streaming answer"),
            "in-progress text: {screen}"
        );
    }

    #[test]
    fn oversized_markdown_table_uses_responsive_rows_without_broken_borders() {
        let table = crate::markdown::parse_blocks(
            "| Crate | State |\n|---|---|\n| `runtime` | Declares cache, sampler, and scheduler modules that do not exist yet |",
        );
        assert!(
            table.iter().all(|line| line.width() > 40),
            "fixture must exercise the narrow path"
        );

        let narrow: Vec<Line<'static>> = table
            .clone()
            .into_iter()
            .flat_map(|line| responsive_content_lines(line, 40))
            .collect();
        let text: Vec<String> = narrow
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(
            narrow.iter().all(|line| line.width() == 40),
            "every physical table row fits exactly: {text:?}"
        );
        assert!(
            text[0].starts_with('┌'),
            "top border remains boxed: {text:?}"
        );
        assert!(text[1].starts_with("│ Crate"), "boxed header: {text:?}");
        assert!(
            text[2].starts_with('├'),
            "middle border remains boxed: {text:?}"
        );
        assert!(
            text[3..text.len() - 1]
                .iter()
                .all(|row| row.starts_with('│') && row.ends_with('│')),
            "wrapped body stays inside the box: {text:?}"
        );
        assert!(
            text.last().is_some_and(|row| row.starts_with('└')),
            "bottom border remains boxed: {text:?}"
        );

        let wide = table
            .clone()
            .into_iter()
            .flat_map(|line| responsive_content_lines(line, 200))
            .collect::<Vec<_>>();
        assert_eq!(wide, table, "a table that fits keeps its full box");
    }

    #[test]
    fn assistant_gutter_does_not_break_responsive_four_column_tables() {
        let source = "| Path | Type | Crate / Name | Purpose |\n\
|------|------|-------------|---------|\n\
| `qwen38-metal/` | workspace | — | From-scratch Qwen3.8-27B inference engine (Rust + MSL, M3 Pro target) |\n\
| `crates/kernels/src/lib.rs` | file | — | Kernel-name consts: rmsnorm, rope, mrope, gqa_fwd, gdn_fwd, swiglu, matmul_row, dequant_gemv |";
        let table = parse_assistant_output(source);
        let raw_top: String = table[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            raw_top.starts_with("✻ ┌"),
            "fixture reproduces the logo-prefixed top border: {raw_top}"
        );

        let narrow: Vec<Line<'static>> = table
            .into_iter()
            .flat_map(|line| responsive_content_lines(line, 80))
            .collect();
        let text: Vec<String> = narrow
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(
            narrow.iter().all(|line| line.width() == 80),
            "logo + constrained box fills the viewport: {text:?}"
        );
        assert!(text[0].starts_with("✻ ┌"), "top border remains: {text:?}");
        assert!(
            text[1].starts_with("  │ Path"),
            "header keeps the shared assistant gutter and box: {:?}",
            text[1]
        );
        let body = &text[3..text.len() - 1];
        assert!(
            body.iter().all(|row| {
                (row.starts_with("  │") && row.ends_with('│')) || row.starts_with("  ├")
            }),
            "every wrapped body row and separator stays indented and boxed: {text:?}"
        );
        assert_eq!(
            body.iter().filter(|row| row.starts_with("  ├")).count(),
            1,
            "the two data rows have a horizontal separator: {text:?}"
        );

        let separator_columns = |row: &str| {
            row.chars()
                .enumerate()
                .filter_map(|(column, ch)| (ch == '│').then_some(column))
                .collect::<Vec<_>>()
        };
        let expected = separator_columns(&text[1]);
        for row in body.iter().filter(|row| row.starts_with("  │")) {
            assert_eq!(separator_columns(row), expected, "aligned cells: {row}");
        }
    }

    #[test]
    fn wrapped_assistant_prose_keeps_the_two_cell_hanging_indent() {
        let line = parse_assistant_output(
            "This sentence is deliberately long enough to wrap over several terminal rows.",
        )
        .remove(0);
        let wrapped = responsive_content_lines(line, 24);
        let text: Vec<String> = wrapped
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(text.len() > 1, "fixture wraps: {text:?}");
        assert!(text[0].starts_with("✻ "), "first row keeps logo: {text:?}");
        assert!(
            text.iter().skip(1).all(|row| row.starts_with("  ")),
            "continuations align after the logo: {text:?}"
        );
        assert!(wrapped.iter().all(|line| line.width() <= 24));
    }

    #[test]
    fn assistant_output_uses_the_logo_then_a_matching_two_cell_gutter() {
        assert_eq!(theme::DEFAULT_LOGO, "✻", "one-cell reduction of logo.svg");
        let lines = parse_assistant_output("\nfirst line\nsecond line");
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(text[0], "", "leading whitespace stays unbranded");
        assert_eq!(text[1], "✻ first line", "tiny logo prefixes output");
        assert_eq!(
            text[2], "  second line",
            "later output aligns after the marker"
        );
    }

    #[test]
    fn scroll_up_shows_older_lines_and_indicator() {
        // A 60x10 screen leaves the transcript ~5 rows. With 20 lines pushed,
        // following shows the bottom; scrolling to the top shows the oldest and
        // flags the state in the status bar.
        let mut state = ViewState::new("m".into());
        for i in 0..20 {
            state.transcript.push(Line::from(format!("line {i}")));
        }
        let screen = rendered(&mut state);
        assert!(
            screen.contains("line 19"),
            "follow shows the bottom: {screen}"
        );
        assert!(
            !screen.contains("line 0"),
            "top hidden when following: {screen}"
        );

        state.follow = false;
        state.scroll_top = 0;
        let screen = rendered(&mut state);
        assert!(
            screen.contains("line 0"),
            "scrolled to top shows oldest: {screen}"
        );
        assert!(
            !screen.contains("line 19"),
            "bottom hidden when scrolled up: {screen}"
        );
        assert!(
            screen.contains("↑"),
            "status flags the scroll state: {screen}"
        );
    }

    #[test]
    fn markdown_and_diff_render_in_the_transcript() {
        // Completed markdown is parsed once on flush; an Edit previews a word diff.
        let mut state = ViewState::new("m".into());
        state.transcript.extend(crate::markdown::parse_blocks(
            "# Heading\n\nsome **bold** text",
        ));
        state
            .transcript
            .push(crate::diff::word_diff_line("old word", "new word"));
        let screen = rendered(&mut state);
        assert!(screen.contains("Heading"), "heading: {screen}");
        assert!(screen.contains("bold"), "bold: {screen}");
        // The inline word diff interleaves the deleted ("old") and inserted
        // ("new") tokens, then the shared " word" -> "oldnew word".
        assert!(
            screen.contains("oldnew"),
            "diff interleaves old+new: {screen}"
        );
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
        let screen = rendered(&mut state);
        assert!(screen.contains("commands"), "menu title: {screen}");
        assert!(screen.contains("/clear"), "first candidate: {screen}");
        assert!(screen.contains("/mode"), "second candidate: {screen}");
        assert!(screen.contains("▶"), "selection marker: {screen}");
    }

    #[test]
    fn completion_menu_scrolls_to_keep_selection_visible() {
        let mut state = ViewState::new("m".into());
        state.composer = "/".into();
        state.menu = Some(CompletionMenu {
            completion: Completion {
                kind: crate::complete::MenuKind::Slash,
                replace_start: 0,
                candidates: (0..10).map(|i| format!("/command-{i}")).collect(),
            },
            selected: 8,
        });
        let screen = rendered_sized(&mut state, 60, 14);

        assert!(
            screen.lines().any(|line| line.contains("▶ /command-8")),
            "selected command stays visible: {screen}"
        );
        assert!(
            screen.contains("/command-1"),
            "window advances one row: {screen}"
        );
        assert!(
            !screen.contains("/command-0"),
            "old rows scroll out: {screen}"
        );
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
        let screen = rendered(&mut state);
        // No "files"/"commands" title block should appear.
        assert!(
            !screen.contains("files"),
            "no menu for empty candidates: {screen}"
        );
        assert!(!screen.contains("commands"), "no menu title: {screen}");
    }

    /// When a transcript line wraps to more rows than the area is tall, the
    /// naive slice (`h` logical lines, no scroll) overflows and `Paragraph` clips
    /// the bottom — cutting off the newest line, right above the composer. The
    /// follow path must scroll the excess off the top so the newest line pins
    /// to the bottom and stays visible. (60x10 -> ~6 transcript rows; the long
    /// line below wraps to 8.)
    #[test]
    fn follow_keeps_newest_line_visible_when_wrapping_overflows() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("x".repeat(60 * 8)));
        state.transcript.push(Line::from("NEWEST"));
        let screen = rendered(&mut state);
        assert!(
            screen.contains("NEWEST"),
            "newest line must be visible (not clipped) when wrapping overflows: {screen}"
        );
    }

    /// An empty composer shows a dim placeholder hint instead of a bare caret —
    /// the "ready for input" affordance. Typing replaces it.
    #[test]
    fn composer_placeholder_when_empty() {
        let mut state = ViewState::new("m".into());
        let screen = rendered(&mut state);
        assert!(
            screen.contains("type a prompt"),
            "placeholder shown when empty: {screen}"
        );
        // Typing dismisses the placeholder.
        state.composer = "hello".into();
        let screen = rendered(&mut state);
        assert!(
            !screen.contains("type a prompt"),
            "no placeholder once typing: {screen}"
        );
        assert!(screen.contains("hello"), "typed text renders: {screen}");
    }

    #[test]
    fn composer_spans_the_full_terminal_width() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("> hello"));
        let screen = rendered_sized(&mut state, 120, 10);
        let top_border = screen
            .lines()
            .find(|line| line.starts_with('╭') && line.ends_with('╮'))
            .unwrap_or_else(|| panic!("full-width composer border missing: {screen}"));

        assert_eq!(top_border.chars().count(), 120, "composer width: {screen}");
        assert!(
            screen
                .lines()
                .any(|line| line.starts_with('╰') && line.ends_with('╯')),
            "composer has rounded bottom corners: {screen}"
        );
    }

    #[test]
    fn turn_divider_spans_the_full_terminal_width() {
        let mut state = ViewState::new("m".into());
        state
            .transcript
            .push(turn_divider_line("12.4s".into(), 0, 0));
        let screen = rendered_sized(&mut state, 120, 10);
        let divider = screen
            .lines()
            .find(|line| line.contains("worked for 12.4s"))
            .unwrap_or_else(|| panic!("turn divider missing: {screen}"));

        assert_eq!(divider.chars().count(), 120, "divider width: {divider:?}");
        assert!(
            divider.starts_with("─ worked for 12.4s · 0 lines changed ─"),
            "left-aligned duration: {divider:?}"
        );
        assert!(divider.contains("0 lines changed"), "{divider:?}");
        assert!(divider.ends_with('─'), "right terminal edge: {divider:?}");
    }

    #[test]
    fn turn_divider_colors_added_and_removed_counts_independently() {
        let p = theme::palette();
        let line = turn_divider_line("2.0s".into(), 12, 3);
        let added = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "+12")
            .expect("added count");
        let removed = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "-3")
            .expect("removed count");
        assert_eq!(added.style, p.semantic(Color::Green));
        assert_eq!(removed.style, p.semantic(Color::Red));

        let fitted = full_width_turn_divider(&line, 120).expect("full-width divider");
        assert_eq!(
            fitted
                .spans
                .iter()
                .find(|span| span.content.as_ref() == "+12")
                .expect("fitted added count")
                .style,
            p.semantic(Color::Green)
        );
        assert_eq!(
            fitted
                .spans
                .iter()
                .find(|span| span.content.as_ref() == "-3")
                .expect("fitted removed count")
                .style,
            p.semantic(Color::Red)
        );
    }

    #[test]
    fn multiline_paste_renders_as_a_line_count_without_losing_content() {
        let mut state = ViewState::new("m".into());
        state.composer.push_str("review ");
        let lines = state.append_paste("alpha\r\nbeta\ngamma");
        state.composer.push_str(" please");

        assert_eq!(lines, 3);
        assert_eq!(state.composer, "review alpha\nbeta\ngamma please");
        assert_eq!(
            composer_display_text(&state),
            "review [pasted 3 lines] please"
        );
        let screen = rendered_sized(&mut state, 70, 10);
        assert!(
            screen.contains("[pasted 3 lines]"),
            "compact paste label: {screen}"
        );
        assert!(
            !screen.contains("alpha"),
            "paste body remains hidden: {screen}"
        );
        assert!(
            !screen.contains("gamma"),
            "all pasted lines remain hidden: {screen}"
        );
    }

    #[test]
    fn single_line_paste_stays_visible_and_editing_reveals_multiline_text() {
        let mut state = ViewState::new("m".into());
        assert_eq!(state.append_paste("one line"), 1);
        assert_eq!(composer_display_text(&state), "one line");

        assert_eq!(state.append_paste("\ntwo\nthree"), 3);
        assert!(composer_display_text(&state).contains("[pasted 3 lines]"));
        state.clear_paste_markers();
        assert_eq!(composer_display_text(&state), "one line\ntwo\nthree");
    }

    /// The right corner is for things that change — interrupt, scroll
    /// position, a copy confirmation. Idle with nothing happening, it stays
    /// empty instead of parking onboarding in the one line that should be
    /// reporting live state.
    #[test]
    fn status_right_corner_is_empty_when_nothing_is_happening() {
        let mut state = ViewState::new("m".into());
        let screen = rendered(&mut state);
        // Scoped to the state line itself: the welcome card legitimately
        // mentions /help and Ctrl+C, and that is the point — one owner each.
        let status = screen
            .lines()
            .find(|r| r.contains(&state.location))
            .expect("status line on screen");
        assert!(
            !status.contains("Ctrl+C"),
            "no static onboarding in the state line: {status}"
        );
        assert!(
            !status.contains("/help"),
            "help lives on the welcome card: {status}"
        );
    }

    /// Busy → the right hint switches to the interrupt affordance.
    #[test]
    fn status_right_hint_shows_interrupt_when_busy() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        let screen = rendered(&mut state);
        assert!(screen.contains("Esc"), "busy shows Esc: {screen}");
        assert!(
            screen.contains("interrupt"),
            "busy shows interrupt: {screen}"
        );
    }

    /// Idle with a drafted prompt → the right hint surfaces "Esc clear" so the
    /// user knows a stray Esc won't quit and lose the draft (a second Esc does).
    #[test]
    fn status_right_hint_shows_esc_clear_when_draft_present() {
        let mut state = ViewState::new("m".into());
        state.composer = "a half-typed prompt".into();
        let screen = rendered(&mut state);
        assert!(screen.contains("Esc"), "draft shows Esc: {screen}");
        assert!(screen.contains("clear"), "draft shows clear: {screen}");
    }

    /// Returned prompt tokens replace the estimate, and cache is a rate over
    /// that context rather than another raw token count.
    #[test]
    fn status_shows_returned_context_and_cache_hit_rate() {
        let mut state = ViewState::new("m".into());
        state.context_tokens = Some(12_345);
        state.context_tokens_estimated = false;
        state.cache_hit_rate = Some(0.081);
        let screen = rendered_sized(&mut state, 78, 10);
        assert!(
            screen.contains("ctx: 12.3K tok"),
            "reported context: {screen}"
        );
        // Compact form — the state line is width-constrained and this says the
        // same thing in fewer columns.
        assert!(screen.contains("8% cached"), "cache rate: {screen}");
        assert!(!screen.contains('Σ'), "cumulative count removed: {screen}");
    }

    /// `right_align` pins the right content to the edge and fills between.
    #[test]
    fn right_align_pads_between_left_and_right() {
        let left = vec![Span::raw("abc")];
        let right = vec![Span::raw("xy")];
        let line = right_align(left, right, 10);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "abc     xy");
        assert_eq!(text.chars().count(), 10, "fills exactly to width: {text:?}");
    }

    /// Two close points in a turn render different one-cell rotational phases.
    #[test]
    fn compact_loader_rotates_over_the_turn() {
        let started = Instant::now();
        let first = logo_spinner_span(started, Some(started));
        let next = logo_spinner_span(started + Duration::from_millis(90), Some(started));
        assert_ne!(first.content, next.content);
    }

    /// Waiting for the first token renders one compact, left-aligned row.
    #[test]
    fn marathoning_loader_is_left_aligned_labeled_and_shimmering() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        let now = Instant::now();
        state.turn_started = Some(now);
        let lines = thinking_lines(&state, now);
        assert_eq!(lines.len(), 1, "no multi-row art");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Marathoning · 0s");
        assert!(text.starts_with("Marathoning"), "no leading logo: {text:?}");
        assert!(!text.contains(theme::DEFAULT_LOGO), "no logo: {text:?}");

        let later = thinking_lines(&state, now + Duration::from_secs(3));
        let later_text: String = later[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(later_text.ends_with("Marathoning · 3s"), "{later_text}");

        let label_len = marathoning_label(now, Some(now)).chars().count();
        assert!(
            (0..label_len).any(|index| {
                shimmer_intensity(index, label_len, Duration::ZERO)
                    != shimmer_intensity(
                        index,
                        label_len,
                        Duration::from_millis((MARATHON_SHIMMER_PERIOD_MS / 2) as u64),
                    )
            }),
            "the label's foreground colors should complete a sweep in 900ms"
        );
    }

    /// A running tool uses one compact line, never the old ASCII-art block.
    #[test]
    fn tool_running_line_shows_logo_and_tool_name() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        state.running = 1;
        state.running_tool = Some("Bash".into());
        let now = Instant::now();
        state.turn_started = Some(now);
        let lines = tool_running_lines(&state, now);
        assert_eq!(lines.len(), 1, "no multi-row art");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains(theme::DEFAULT_LOGO), "tiny logo: {text}");
        assert!(text.contains("Bash"), "tool name: {text}");
    }

    /// Before the first turn the welcome card carries the brand and key hints;
    /// model metadata lives in the footer beneath the prompt box.
    #[test]
    fn welcome_card_carries_the_onboarding() {
        let mut state = ViewState::new("gw-glm-5.2".into());
        let screen = rendered_sized(&mut state, 80, 20);
        assert!(screen.contains("Subconscious Code"), "wordmark: {screen}");
        assert!(screen.contains("gw-glm-5.2"), "model: {screen}");
        assert!(screen.contains("mention a file"), "@ hint: {screen}");
        assert!(screen.contains("Ctrl+C"), "quit hint: {screen}");
        assert!(
            screen.contains("███▄  ▄███"),
            "the welcome screen renders the high-quality logo.svg: {screen}"
        );
        // The one binding whose effect can't be guessed names its options.
        assert!(
            screen.contains("accept edits"),
            "Shift+Tab names the modes: {screen}"
        );

        let rows: Vec<&str> = screen.lines().collect();
        let box_bottom = rows
            .iter()
            .position(|row| row.starts_with('╰') && row.ends_with('╯'))
            .expect("composer bottom border");
        let model_row = rows
            .iter()
            .position(|row| row.contains("gw-glm-5.2"))
            .expect("model footer");
        assert_eq!(model_row, box_bottom + 1, "model below composer: {screen}");
        assert!(
            !rows[model_row].contains("model"),
            "footer keeps only the model name: {}",
            rows[model_row]
        );
        assert!(!screen.contains("scroll history"), "quiet footer: {screen}");
    }

    #[test]
    fn welcome_logo_matches_the_intro_height() {
        let logo = welcome_logo(Rect::new(0, 0, 80, 24)).expect("welcome logo");
        assert_eq!(logo.art, theme::LOGO_ART_SMALL);
        assert_eq!((logo.width, logo.height), (18, 8));

        assert_eq!(welcome_logo(Rect::new(0, 0, 60, 7)), None);
    }

    /// The unbordered welcome card begins at the top of a fresh terminal. The
    /// composer remains the only outlined element at the bottom.
    #[test]
    fn welcome_card_is_unbordered_and_starts_at_the_top() {
        let mut state = ViewState::new("m".into());
        let screen = rendered_sized(&mut state, 80, 24);
        let rows: Vec<&str> = screen.lines().collect();
        let wordmark = rows
            .iter()
            .position(|r| r.contains("Subconscious Code"))
            .expect("wordmark on screen");
        let box_top = rows
            .iter()
            .position(|r| r.contains("╭"))
            .expect("composer box on screen");
        let logo_top = rows
            .iter()
            .position(|r| r.contains("███▄  ▄███"))
            .expect("logo on screen");
        assert!(
            box_top > wordmark,
            "the only bordered box is below the card (the composer): {screen}"
        );
        assert_eq!(logo_top, 0, "logo begins at terminal top: {screen}");
        // Exactly one box: the card contributes no border of its own.
        assert_eq!(
            rows.iter().filter(|r| r.contains("╭")).count(),
            1,
            "one bordered element on screen: {screen}"
        );
    }

    /// The moment a turn starts (transcript non-empty), the welcome card is
    /// gone — replaced by the conversation.
    #[test]
    fn welcome_card_disappears_once_a_turn_starts() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("> hello"));
        let screen = rendered(&mut state);
        assert!(
            !screen.contains("Subconscious Code"),
            "no welcome card once conversation started: {screen}"
        );
    }

    /// `/clear` empties the transcript, so the welcome card comes back.
    #[test]
    fn welcome_card_returns_after_clear() {
        let mut state = ViewState::new("m".into());
        state.transcript.push(Line::from("> hello"));
        // Simulate /clear: transcript emptied.
        state.transcript.clear();
        let screen = rendered(&mut state);
        assert!(
            screen.contains("Subconscious Code"),
            "welcome card after clear: {screen}"
        );
    }

    #[test]
    fn wide_chat_uses_almost_the_entire_terminal() {
        let wide = content_area(Rect::new(0, 0, 200, 24));
        assert_eq!(wide.x, 1);
        assert_eq!(wide.width, 198);

        let narrow = content_area(Rect::new(0, 0, 80, 24));
        assert_eq!(narrow.x, 0);
        assert_eq!(narrow.width, 80);
    }
}
