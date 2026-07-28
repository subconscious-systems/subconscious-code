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
mod markdown;
mod view;

use std::io::Stdout;
use std::path::PathBuf;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use rc_rt::Runtime;

pub(crate) type Term = Terminal<CrosstermBackend<Stdout>>;

/// Launch the TUI against `runtime`. Blocks the calling thread — run it on a
/// `tokio::task::spawn_blocking` thread so the rc-rt driver/pump keep running.
/// Returns when the user quits (Ctrl+C, or Esc while idle).
///
/// `cwd` is the session's working directory, used by the M4c composer
/// autocomplete to resolve `@file` mentions.
pub fn run(runtime: Runtime, model_name: String, cwd: PathBuf) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = app::run(&mut terminal, runtime, model_name, cwd);

    // Restore the terminal whatever happened above.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
    let _ = terminal.show_cursor();
    result
}
