//! rc-tui: the ratatui frontend (§12), M4a minimal slice.
//!
//! A synchronous poll loop over crossterm events + a ratatui render. It owns no
//! tokio runtime; the rc-rt driver/pump run on the host's tokio runtime and the
//! TUI talks to them purely through [`rc_rt::EventStream`] (sync `try_next`)
//! and [`rc_rt::Runtime::action`]. Run it on a `spawn_blocking` thread so it
//! doesn't stall the async runtime (see the rc-cli wiring).
//!
//! M4a deliberately renders plain text (no markdown, no diff) and a single-line
//! composer (no `@` autocomplete, no slash palette, no history). Those land in
//! M4b/M4c.

mod app;
mod complete;
mod diff;
#[cfg(test)]
mod logo3d;
mod markdown;
mod menu;
mod theme;
mod view;

use std::io::Stdout;
use std::path::PathBuf;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use rc_rt::Runtime;

pub use menu::Outcome;

pub(crate) type Term = Terminal<CrosstermBackend<Stdout>>;

/// Launch the TUI against `runtime`. Blocks the calling thread — run it on a
/// `tokio::task::spawn_blocking` thread so the rc-rt driver/pump keep running.
/// Returns when the user quits (Ctrl+C, or Esc while idle).
///
/// `cwd` is the session's working directory, used by the M4c composer
/// autocomplete to resolve `@file` mentions.
/// `history` is the already-persisted turn log for a resumed session; it is
/// rendered before the first frame while the same turns live in the runtime's
/// model context.
///
/// Returns `Some(`[`Outcome`]`)` when the user picked another session (or a
/// fresh one) from `/menu`. The TUI can't perform that switch itself — a
/// different session means a different cwd, tool set, and permission roots,
/// all constructed above this crate — so the caller is expected to rebuild
/// against the returned target and call `run` again.
pub fn run(
    runtime: Runtime,
    model_name: String,
    cwd: PathBuf,
    initial_mode: rc_core::AgentMode,
    history: Vec<rc_core::Turn>,
    mouse: bool,
) -> anyhow::Result<Option<Outcome>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    // Off unless asked for. Capturing the mouse takes drag-to-select away from
    // the terminal, and a terminal program that can't be copied out of is
    // broken in a way no feature makes up for. `ui.mouse` (or Ctrl+O) turns on
    // sc's own selection, which copies on release but needs a terminal that
    // accepts OSC 52.
    if mouse {
        execute!(stdout, EnableMouseCapture)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = app::run(
        &mut terminal,
        runtime,
        model_name,
        cwd,
        initial_mode,
        history,
        mouse,
    );

    // Restore the terminal whatever happened above.
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = terminal.show_cursor();
    result
}
