//! Brand theme for the `sc` TUI.
//!
//! Orange is the accent: the chrome — header, status bar, tool-start markers,
//! headings, the completion-menu highlight, and the startup logo splash — all
//! draw from it. Semantic state colors stay universal (errors red, success
//! green, warnings yellow, diff insert/delete green/red) so a failed tool
//! result is still unmistakable inside an orange UI. Recoloring those orange
//! would make state invisible, which is worse than off-brand.
//!
//! Colors are `Color::Rgb` so they match the brand swatch (`#FF5C27`, pulled
//! from `logo.svg`) exactly. Modern terminals (the ssh box included) support
//! truecolor; a terminal that doesn't will fall back to its default, which is
//! the only real cost of using the exact brand orange over an xterm-256 index.

use ratatui::style::{Color, Style};
use ratatui::text::Line;

/// Brand orange, taken from `logo.svg`'s `#FF5C27` fill. Header, status bar,
/// tool-start markers, top-level headings, the menu selection background, and
/// the logo splash.
pub const ACCENT: Color = Color::Rgb(255, 92, 39);
/// Brighter brand orange — inline code, links, the menu's unselected candidates.
pub const ACCENT_BRIGHT: Color = Color::Rgb(255, 140, 92);
/// Dimmer brand orange — deeper headings, code blocks, secondary chrome.
pub const ACCENT_DIM: Color = Color::Rgb(184, 74, 30);

/// Default logo glyph for the header when the user has not supplied one.
pub const DEFAULT_LOGO: &str = "◈";

/// The rasterized `logo.svg` as monochrome half-block art (▀/▄/█/space). The
/// TUI renders it in [`ACCENT`] as a startup splash. Generating it is a one-off
/// offline step (`rsvg-convert` + a pixel→half-block pass); the art is checked
/// in so the binary ships with the brand and needs no image runtime.
pub const LOGO_ART: &str = include_str!("../assets/logo.txt");

/// The logo splash as orange styled lines, for the top of the transcript.
pub fn splash_lines() -> Vec<Line<'static>> {
    LOGO_ART
        .lines()
        .map(|l| Line::styled(l.to_string(), Style::default().fg(ACCENT)))
        .collect()
}

/// The user's logo glyph from `~/.sc/logo.txt`, if present. The first non-empty
/// line is trimmed and used as the header icon (capped at a few cells so a
/// tall ASCII-art file can't blow the header line wide). Absent or unreadable
/// falls back to [`DEFAULT_LOGO`].
///
/// To brand your header, write a short glyph (or the first line of an
/// ASCII-art logo) to `~/.sc/logo.txt`:
///
/// ```sh
/// echo "◈" > ~/.sc/logo.txt        # a single Unicode mark
/// echo "(o)" > ~/.sc/logo.txt      # or a short ASCII token
/// ```
pub fn logo_glyph() -> String {
    let Ok(home) = std::env::var("HOME") else { return DEFAULT_LOGO.to_string() };
    let path = std::path::Path::new(&home).join(".sc").join("logo.txt");
    let Ok(s) = std::fs::read_to_string(&path) else { return DEFAULT_LOGO.to_string() };
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(8).collect())
        .filter(|l: &String| !l.is_empty())
        .unwrap_or_else(|| DEFAULT_LOGO.to_string())
}
