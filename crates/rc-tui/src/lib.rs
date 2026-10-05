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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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

/// Whether this process currently owns the alternate screen + raw mode. The
/// first setup flips it on; the first restore flips it off, which makes every
/// later call (guard Drop, explicit restore before shutdown, the panic hook)
/// a no-op instead of a double restore.
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Restore the terminal to its pre-TUI state: raw mode off, primary screen,
/// cursor visible. Idempotent; errors are swallowed — restore is best-effort
/// on every path it runs from.
fn restore_terminal() {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = std::io::stdout();
    let _ = disable_raw_mode();
    let _ = execute!(
        out,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        crossterm::cursor::Show
    );
}

/// RAII ownership of the terminal setup: any exit from [`run`] — normal
/// return, an early `?` error, a panic that unwinds this thread — drops the
/// guard and restores the terminal.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Install the panic hook exactly once per process: the terminal is restored
/// *first*, so the panic message doesn't vanish with the alternate screen,
/// then the previously installed hook (the default, or the host's own) still
/// prints it. Registered only on the real-terminal path — the crate's
/// TestBackend tests never call [`run`], so they cannot clobber or observe
/// this hook.
fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

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
    // Opt-in (codex #50370/#50466, opencode #50242): capture breaks native
    // terminal selection and tmux copy mode, so it starts only when the user
    // asked for it (ui.mouse = true / SC_MOUSE=1). While captured, sc owns
    // selection and copies on release via OSC 52; Ctrl+O hands selection back.
    if mouse {
        execute!(stdout, EnableMouseCapture)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Setup complete: from here to restore, this process owns the terminal.
    TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
    install_panic_hook();
    let _guard = TerminalGuard;

    // `?` then `Ok(..)` rather than a direct `return app::run(..)`: the guard
    // must stay alive until after the app loop has finished restoring the
    // terminal, and this shape keeps both the error and the success path
    // under it without handing clippy a `let_and_return`.
    let outcome = app::run(
        &mut terminal,
        runtime,
        model_name,
        cwd,
        initial_mode,
        history,
        mouse,
    )?;
    Ok(outcome)
}
