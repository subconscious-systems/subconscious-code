//! Brand theme + color budget for the `sc` TUI.
//!
//! One accent does the brand work — headings and the startup splash. Chrome
//! (bullets, dim rules, the gutter, tool glyphs, status labels, compose hints)
//! is dim gray. Identifiers and code get a second hue, cyan. Body text is the
//! terminal's default foreground. Semantic state (a failed tool, a diff) stays
//! universal red/green so it reads inside any budget.
//!
//! All non-brand hues come from the 16-color palette (`DarkGray`, `Cyan`), so a
//! no-truecolor terminal still shows distinction; the brand accent stays
//! truecolor (`Color::Rgb`, the `#FF5C27` from `logo.svg`) and falls back to the
//! terminal's default where truecolor isn't supported. `NO_COLOR` (per
//! <https://no-color.org>: present and non-empty) drops every foreground color
//! to monochrome — modifiers (bold/underline/reversed) survive, so a light
//! terminal with `NO_COLOR` set still reads cleanly.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use std::sync::OnceLock;

/// Brand orange from `logo.svg`'s `#FF5C27` fill — headings + splash + menu bg.
const ACCENT_RGB: Color = Color::Rgb(255, 92, 39);
/// Dimmer brand orange — deep headings.
const ACCENT_DIM_RGB: Color = Color::Rgb(184, 74, 30);

/// One-cell terminal reduction of `logo.svg`'s six-petal flower. `✻` keeps the
/// same radial/petaled silhouette at the smallest size a terminal can render;
/// it is used as the tiny assistant-output/activity mark, while the checked-in
/// raster below preserves the exact SVG at larger sizes.
pub const DEFAULT_LOGO: &str = "✻";

/// The rasterized `logo.svg` as monochrome half-block art (▀/▄/█/space). The
/// TUI renders it in [`Palette::logo`] as the welcome card's static splash.
/// Generating it is a one-off offline step (`rsvg-convert` + a pixel→half-block
/// pass); the art is checked in so the binary ships with the brand and needs no
/// image runtime. In-turn animation stays a one-cell Unicode mark.
pub const LOGO_ART: &str = include_str!("../assets/logo.txt");

/// The resolved color budget, constructed once from the environment.
pub struct Palette {
    no_color: bool,
}

impl Palette {
    fn new() -> Self {
        // https://no-color.org: present and non-empty disables color.
        let no_color = std::env::var_os("NO_COLOR")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        Self { no_color }
    }

    fn paint(&self, c: Color) -> Style {
        if self.no_color {
            Style::new()
        } else {
            Style::new().fg(c)
        }
    }

    /// The one accent — headings and the splash. Truecolor brand orange.
    pub fn accent(&self) -> Style {
        self.paint(ACCENT_RGB)
    }
    pub fn accent_dim(&self) -> Style {
        self.paint(ACCENT_DIM_RGB)
    }
    /// Chrome — bullets, dim rules, the gutter, tool glyphs, status labels,
    /// compose hints. 16-color `DarkGray`.
    pub fn chrome(&self) -> Style {
        self.paint(Color::DarkGray)
    }
    /// Identifiers and code — inline `` `code` ``, code-block bodies, links.
    /// A second hue, 16-color `Cyan`, distinct from the accent.
    pub fn code(&self) -> Style {
        self.paint(Color::Cyan)
    }
    /// Body text — the terminal's default foreground.
    pub fn body(&self) -> Style {
        Style::new()
    }
    /// A link — cyan + underlined (underlined only under `NO_COLOR`).
    pub fn link(&self) -> Style {
        if self.no_color {
            Style::new().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::new()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED)
        }
    }
    /// Reasoning — the model's chain-of-thought, distinct from its answer:
    /// dim + italic, so it reads as private monologue rather than output. Under
    /// `NO_COLOR` the italic modifier alone carries the distinction.
    pub fn reasoning(&self) -> Style {
        if self.no_color {
            Style::new().add_modifier(Modifier::ITALIC)
        } else {
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        }
    }
    /// A semantic state color (red error, green success) — dropped to
    /// monochrome under `NO_COLOR`, where the glyph/word alone carries meaning.
    pub fn semantic(&self, c: Color) -> Style {
        self.paint(c)
    }
    /// The menu's selected row: brand bg, black text; reversed under `NO_COLOR`.
    pub fn menu_selected(&self) -> Style {
        if self.no_color {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(Color::Black).bg(ACCENT_RGB)
        }
    }
    /// The splash — same as the accent.
    pub fn logo(&self) -> Style {
        self.accent()
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// The process-wide color budget, resolved from the environment once.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(Palette::new)
}

/// Whether motion (spinner, caret blink) is allowed. Animations are on by
/// default; set `SC_NO_ANIM` (present and non-empty) to disable them — for
/// accessibility, low-CPU hosts, or terminals whose braille glyphs render
/// poorly. Disabled motion degrades to a static glyph; the elapsed-time
/// readouts (which are information, not decoration) keep updating.
static ANIM: OnceLock<bool> = OnceLock::new();

pub fn animations_enabled() -> bool {
    *ANIM.get_or_init(|| {
        // Mirror NO_COLOR's convention: present and non-empty disables.
        std::env::var_os("SC_NO_ANIM")
            .map(|v| !v.is_empty())
            .map(|disabled| !disabled)
            .unwrap_or(true)
    })
}

/// The logo splash as styled lines for the static welcome card.
pub fn splash_lines() -> Vec<Line<'static>> {
    let s = palette().logo();
    LOGO_ART
        .lines()
        .map(|l| Line::styled(l.to_string(), s))
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
/// echo "✻" > ~/.sc/logo.txt        # a single Unicode mark
/// echo "(o)" > ~/.sc/logo.txt      # or a short ASCII token
/// ```
pub fn logo_glyph() -> String {
    let Ok(home) = std::env::var("HOME") else {
        return DEFAULT_LOGO.to_string();
    };
    let path = std::path::Path::new(&home).join(".sc").join("logo.txt");
    let Ok(s) = std::fs::read_to_string(&path) else {
        return DEFAULT_LOGO.to_string();
    };
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(8).collect())
        .filter(|l: &String| !l.is_empty())
        .unwrap_or_else(|| DEFAULT_LOGO.to_string())
}
