//! Rendering: turns [`ViewState`] into ratatui widgets. The state types live
//! here so a `TestBackend` render test can build them with no tokio and no model.
//!
//! M4a renders plain text (simple `Wrap`, no markdown, no diff) and a
//! single-line composer. Markdown / word-level diff land in M4b.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use rc_core::{AgentMode, Usage};
use serde_json::Value;
use std::time::{Duration, Instant};

use crate::complete::Completion;
use crate::menu::{ago, MenuPage, MenuState, Row};
use crate::theme;

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
    /// Completed reasoning bodies, normally hidden behind `thought for N.NNs`
    /// rows and restored in-place when the user presses Ctrl+T.
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
            running: 0,
            running_tool: None,
            stream_chars: 0,
            stream_started: None,
            last_input: None,
            process_started: Instant::now(),
            menu_overlay: None,
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

    /// Whether Ctrl+T has a completed reasoning block to toggle.
    pub(crate) fn has_completed_reasoning(&self) -> bool {
        !self.reasoning_blocks.is_empty()
    }

    /// Expand/collapse the most recent completed reasoning block in place.
    /// Completed bodies are retained in `reasoning_blocks`, so collapsing is a
    /// presentation choice rather than destructive compaction.
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

/// Parse assistant markdown and put the one-cell `logo.svg` reduction on its
/// first visible line. This is deliberately the only conversation-side orange
/// identity marker: user prompts use a neutral `>` echo, so the brand mark now
/// means "model output" at a glance. Leading blank lines stay blank.
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
    lines[first_visible].spans.insert(
        0,
        Span::styled(
            format!("{} ", theme::DEFAULT_LOGO),
            theme::palette().accent(),
        ),
    );
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
    Line::from(vec![
        Span::raw("  "),
        Span::styled(logo_glyph().to_string(), theme::palette().accent()),
    ])
}

/// The live reasoning placeholder is exactly one fast-turning brand glyph.
/// No reasoning text or multi-row art is materialized while the model thinks.
fn animated_live_reasoning_line(now: Instant, process_started: Instant) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        logo_spinner_span(now, Some(process_started)),
    ])
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

    let area = frame.area();
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
        let inner_w = area.width.saturating_sub(2).max(1);
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
        ])
        .split(area);
    draw_transcript(frame, state, chunks[0], now);
    frame.render_widget(Clear, chunks[1]); // keep the gap blank across frames
    draw_status(frame, state, chunks[2], now);
    if let Some(ask) = &state.pending_ask {
        draw_ask(frame, ask, chunks[3]);
    } else {
        draw_composer(frame, state, chunks[3], now);
        // The completion menu floats above the composer, as a popup.
        if let Some(menu) = &state.menu {
            draw_menu(frame, menu, chunks[3]);
        }
    }
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
        draw_welcome_card(frame, state, area);
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
            lines.push(state.transcript[i].clone());
        } else if has_stream {
            let stream_index = i - tr_len;
            if stream_index == 0
                && !state.current_reasoning.is_empty()
                && state.reasoning_elapsed.is_none()
            {
                // Replace only the cached content-free placeholder. This tiny
                // line can animate every frame without re-parsing a growing
                // markdown answer on every frame.
                lines.push(animated_live_reasoning_line(now, state.process_started));
            } else {
                lines.push(state.current_parsed[stream_index].clone());
            }
        } else {
            // The live loader block (thinking/tool). It's the only streaming
            // content, so its rows follow the transcript directly.
            lines.push(live_lines[i - tr_len].clone());
        }
    }
    // While the model streams, append a live typing caret to the end of the
    // last visible line so the generation point is visible — the "still typing"
    // cue. Shown while either reasoning or answer text is streaming, and only
    // when the streaming content is in the visible window (`end > tr_len` means
    // the last visible line is a streaming line, not a transcript one — avoids
    // mis-marking an old line while scrolled up) and a turn is in flight.
    if has_stream && state.busy && end > tr_len && !lines.is_empty() {
        if let Some(last) = lines.last_mut() {
            last.spans.push(stream_caret_span(state, now));
        }
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
            let global_index = start + offset;
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

/// The pre-conversation welcome card: the brand logo (the static half-block
/// [`theme::splash_lines`] art) on the left, and the model + cwd + key hints
/// on the right, both inside one bordered box. A real `Block` + horizontal
/// `Layout` (not text-art) so it stays a proper box at any terminal width —
/// the info column wraps within its space on narrow terminals instead of
/// breaking. Sized to the logo's height (8 rows + 2 border) and clamped to
/// the area.
///
/// Deliberately static: motion here would compete with the composer for
/// attention on an idle screen. In-turn motion uses only the one-cell logo.
fn draw_welcome_card(frame: &mut Frame, state: &ViewState, area: Rect) {
    let p = theme::palette();
    let card_h = 10u16.min(area.height);
    let card = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: card_h,
    };
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(card);
    frame.render_widget(block, card);
    // Left column: the logo. Right column: the info, top-aligned.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(Paragraph::new(theme::splash_lines()), cols[0]);
    // The info lines avoid colliding with strings the render tests assert are
    // absent in fresh state ("type a prompt" / "/help" live in the composer
    // placeholder and the status-bar hint), so the card stays up while typing.
    let info = vec![
        Line::styled(
            format!("{} sc · model: {}", theme::logo_glyph(), state.model_name),
            p.accent(),
        ),
        Line::styled(format!("cwd: {}", state.cwd), p.chrome()),
        Line::styled("@file mentions a file", p.chrome()),
        Line::styled("Shift+Tab mode · Alt+↑↓ history · Ctrl+C quit", p.chrome()),
    ];
    frame.render_widget(Paragraph::new(info), cols[1]);
}

fn draw_status(frame: &mut Frame, state: &ViewState, area: Rect, now: Instant) {
    let p = theme::palette();

    // While busy, show the elapsed turn time once it crosses a second — a
    // Codex-style "how long has this been going" readout. Sub-second, keep
    // the word "working" so the state reads at a glance (and tests stay
    // stable).
    let activity = if state.busy {
        let secs = elapsed_secs(now, state.turn_started);
        if secs >= 1 {
            format!("{secs}s")
        } else {
            "working".to_string()
        }
    } else {
        "idle".to_string()
    };

    // Left side: the primary, at-a-glance facts. Built from spans so the parts
    // that matter (model, mode, the headline numbers) read in the default
    // foreground while the chrome (separators, labels) stays dim. Middle-dot
    // separators read lighter than `|` and let the line breathe. The mode is
    // semantic-colored: green = accept-edits, cyan = plan, red = bypass
    // (dangerous), so Shift+Tab's state is unmistakable.
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" "));
    if state.busy {
        spans.push(logo_spinner_span(now, state.turn_started));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(state.model_name.clone(), p.body()));
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
        if let Some(rate) = state.cache_hit_rate {
            spans.push(Span::styled(
                format!(" ({} cache hit)", human_percent(rate)),
                p.chrome(),
            ));
        }
    }
    spans.push(Span::styled(" · ", p.chrome()));
    spans.push(Span::styled(
        activity,
        if state.busy { p.accent() } else { p.chrome() },
    ));
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
    // Captured is the non-default state and the surprising one: while sc holds
    // the mouse, the terminal's own select-and-copy stops working, which reads
    // as the terminal being broken unless the corner says who has the mouse.
    if state.mouse_capture {
        return vec![
            Span::styled("sc has the mouse", p.accent()),
            Span::styled(" · Ctrl+O releases", p.chrome()),
        ];
    }
    let indicator = scroll_indicator(state);
    if !indicator.is_empty() {
        return vec![Span::styled(indicator, p.chrome())];
    }
    if state.busy {
        return vec![
            Span::styled("Esc ", p.chrome()),
            Span::styled("interrupt", p.accent()),
        ];
    }
    // A drafted prompt is one Esc from being cleared (not lost — the second
    // Esc, with the line empty, quits), so surface that action while typing;
    // an empty line shows the discoverability hint instead.
    if !state.composer.is_empty() {
        vec![
            Span::styled("Esc ", p.chrome()),
            Span::styled("clear", p.accent()),
            Span::styled(" · Ctrl+C quit", p.chrome()),
        ]
    } else if state.has_completed_reasoning() {
        vec![
            Span::styled("Ctrl+T ", p.chrome()),
            Span::styled("thought", p.accent()),
            Span::styled(" · Ctrl+C quit", p.chrome()),
        ]
    } else {
        vec![
            Span::styled("/help · ", p.chrome()),
            Span::styled("Ctrl+C quit", p.code()),
        ]
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

/// Render a provider cache ratio as a compact percentage.
fn human_percent(rate: f64) -> String {
    format!("{:.1}%", rate.clamp(0.0, 1.0) * 100.0)
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
        spans.push(Span::styled(" type a prompt · /help · @file", p.chrome()));
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
        .block(Block::default().borders(Borders::ALL));
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
const LOGO_SPINNER_FRAMES: [&str; 4] = ["✻", "✽", "✼", "✽"];
/// A deliberately quick 22 fps turn; the busy poll loop runs at 125 Hz, so
/// each phase is sampled smoothly without slowing streaming event delivery.
const LOGO_SPINNER_FRAME_MS: u128 = 45;
/// The caret holds solid this long after the last keystroke before it starts
/// to blink, so typing doesn't fight a flicker.
const CARET_HOLD: Duration = Duration::from_secs(1);
/// Caret blink half-period.
const CARET_BLINK_MS: u64 = 530;

/// One fast clockwise turn of the tiny brand mark. This is also the only
/// thinking animation in the transcript; no text shimmer or ASCII art.
fn logo_spinner_span(now: Instant, started: Option<Instant>) -> Span<'static> {
    let p = theme::palette();
    if !theme::animations_enabled() {
        return Span::styled(logo_glyph().to_string(), p.accent());
    }
    let elapsed_ms = started
        .map(|s| now.saturating_duration_since(s).as_millis())
        .unwrap_or(0);
    let frame = (elapsed_ms / LOGO_SPINNER_FRAME_MS) as usize % LOGO_SPINNER_FRAMES.len();
    Span::styled(LOGO_SPINNER_FRAMES[frame].to_string(), p.accent())
}

/// Whole seconds elapsed since a turn's epoch, 0 while idle.
fn elapsed_secs(now: Instant, started: Option<Instant>) -> u64 {
    started
        .map(|s| now.saturating_duration_since(s).as_secs())
        .unwrap_or(0)
}

/// Waiting for the first token is represented by only the tiny rotating mark.
fn thinking_lines(state: &ViewState, now: Instant) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw("  "),
        logo_spinner_span(now, state.turn_started),
    ])]
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
    let on = (ms / CARET_BLINK_MS) % 2 == 0;
    if on {
        Span::styled("█".to_string(), p.accent())
    } else {
        Span::styled("█".to_string(), p.chrome())
    }
}

/// The live "still typing" caret appended to the end of streaming assistant
/// text while the model generates — the cue Claude Code/Codex show so you can
/// see where the next token will land. A thin block that blinks (accent ↔ dim)
/// at the same cadence as the composer caret; solid accent when motion is off.
fn stream_caret_span(state: &ViewState, now: Instant) -> Span<'static> {
    let p = theme::palette();
    if !theme::animations_enabled() {
        return Span::styled("▋".to_string(), p.accent());
    }
    let ms = state
        .turn_started
        .map(|t| now.saturating_duration_since(t).as_millis() as u64)
        .unwrap_or(0);
    let on = (ms / CARET_BLINK_MS) % 2 == 0;
    let style = if on { p.accent() } else { p.chrome() };
    Span::styled("▋".to_string(), style)
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
    let body_rows = inner.height.saturating_sub(4) as usize;
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

    /// Capturing the mouse takes select-and-copy away from the terminal, so
    /// that state has to announce itself — otherwise it reads as the terminal
    /// having broken. The default (released) says nothing.
    #[test]
    fn captured_mouse_owns_the_status_hint() {
        let mut state = ViewState::new("gw-glm-5.2".into());
        let plain = rendered_sized(&mut state, 78, 10);
        assert!(
            !plain.contains("has the mouse"),
            "the default state is quiet: {plain}"
        );

        state.mouse_capture = true;
        let screen = rendered_sized(&mut state, 78, 10);
        assert!(screen.contains("has the mouse"), "state shown: {screen}");
        assert!(
            screen.contains("Ctrl+O"),
            "the way back is on screen: {screen}"
        );
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

    #[test]
    fn status_line_shows_model_mode_and_activity() {
        let mut state = ViewState::new("mock-model".into());
        state.mode = AgentMode::Plan;
        state.busy = true;
        let screen = rendered(&mut state);
        assert!(screen.contains("mock-model"), "model name: {screen}");
        assert!(screen.contains("plan"), "mode: {screen}");
        assert!(screen.contains("working"), "busy state: {screen}");
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
        assert_eq!(human_percent(0.08125), "8.1%");
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
    /// available behind Ctrl+T.
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
    fn reasoning_only_stream_shows_one_content_free_thinking_row() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        state.current_reasoning = "private chain of thought must stay hidden".into();
        let screen = rendered(&mut state);
        assert!(
            screen.contains(theme::DEFAULT_LOGO),
            "tiny activity mark: {screen}"
        );
        assert!(!screen.contains("thinking"), "no animated label: {screen}");
        assert!(
            !screen.contains("private chain"),
            "streamed reasoning content is never rendered: {screen}"
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
    fn assistant_output_uses_the_tiny_logo_on_first_visible_line_only() {
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
        assert_eq!(text[2], "second line", "marker appears only once");
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

    /// The status bar's right corner carries a context-sensitive hint — the
    /// discoverability hint when idle, the interrupt key when busy, the held-
    /// view indicator when scrolled up. Here: idle → the help/quit hint.
    #[test]
    fn status_right_hint_shows_help_when_idle() {
        let mut state = ViewState::new("m".into());
        let screen = rendered(&mut state);
        assert!(screen.contains("Ctrl+C quit"), "idle right hint: {screen}");
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
        // The quit affordance stays visible alongside it.
        assert!(
            screen.contains("Ctrl+C quit"),
            "quit still hinted: {screen}"
        );
        // The empty-state help hint is displaced by the clearer Esc-clear hint.
        assert!(
            !screen.contains("/help"),
            "no /help once drafting: {screen}"
        );
    }

    /// Returned prompt tokens replace the estimate, and cache is a rate over
    /// that context rather than another raw token count.
    #[test]
    fn status_shows_returned_context_and_cache_hit_rate() {
        let mut state = ViewState::new("m".into());
        state.context_tokens = Some(12_345);
        state.context_tokens_estimated = false;
        state.cache_hit_rate = Some(0.081);
        let screen = rendered(&mut state);
        assert!(
            screen.contains("ctx: 12.3K tok"),
            "reported context: {screen}"
        );
        assert!(screen.contains("8.1% cache hit"), "cache rate: {screen}");
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

    /// Waiting for the first token renders one small symbol, not ASCII art or
    /// a thinking label.
    #[test]
    fn thinking_loader_is_only_one_small_symbol() {
        let mut state = ViewState::new("m".into());
        state.busy = true;
        let now = Instant::now();
        state.turn_started = Some(now);
        let lines = thinking_lines(&state, now);
        assert_eq!(lines.len(), 1, "no multi-row art");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.trim(), theme::DEFAULT_LOGO);
        assert!(!text.contains("thinking"));
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

    /// Before the first turn, the welcome card shows the logo on the left and
    /// the model + cwd info on the right, all inside a bordered box.
    #[test]
    fn welcome_card_shows_logo_and_info_before_first_turn() {
        let mut state = ViewState::new("gw-glm-5.2".into());
        state.cwd = "/home/daniel/subconscious-code".into();
        // Wider than the default 60x10: the logo column widened to fit the
        // rotating render (see `draw_welcome_card`), and 60 cols no longer
        // leaves room for the cwd line on the right without clipping it —
        // same as it would on a real narrow terminal, not a test artifact.
        let screen = rendered_sized(&mut state, 80, 10);
        assert!(screen.contains("┌"), "box top border: {screen}");
        assert!(screen.contains("│"), "box side border: {screen}");
        assert!(
            screen.contains("sc · model: gw-glm-5.2"),
            "model line to the right: {screen}"
        );
        assert!(
            screen.contains("cwd: /home/daniel/subconscious-code"),
            "cwd line: {screen}"
        );
        // The rotating logo renders from a shading ramp (` .:-=+*#%@`), not
        // fixed glyphs — assert some non-blank ramp character shows up rather
        // than any one specific one.
        assert!(
            screen.chars().any(|c| ":-=+*#%@".contains(c)),
            "logo render on the left: {screen}"
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
            !screen.contains("sc · model:"),
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
            screen.contains("sc · model: m"),
            "welcome card after clear: {screen}"
        );
    }
}
