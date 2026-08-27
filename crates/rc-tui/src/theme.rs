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
use std::sync::OnceLock;

/// Brand orange from `logo.svg`'s `#FF5C27` fill — headings + splash + menu bg.
const ACCENT_RGB: Color = Color::Rgb(255, 92, 39);
/// Dimmer brand orange — deep headings.
const ACCENT_DIM_RGB: Color = Color::Rgb(184, 74, 30);
/// Dark tints that read like a translucent diff overlay on the default black
/// terminal background, rather than saturated ANSI highlight colors.
pub(crate) const DIFF_ADDED_BG: Color = Color::Rgb(28, 57, 39);
pub(crate) const DIFF_REMOVED_BG: Color = Color::Rgb(67, 30, 32);

/// One-cell terminal reduction of `logo.svg`'s six-petal flower. `✻` keeps the
/// same radial/petaled silhouette at the smallest size a terminal can render;
/// it is used as the tiny assistant-output/activity mark, while the checked-in
/// raster below preserves the exact SVG at larger sizes.
pub const DEFAULT_LOGO: &str = "✻";

/// Terminal raster of the checked-in `logo.svg`, sampled into half-block cells
/// so each terminal row carries two square image pixels. Embedding it keeps
/// startup independent of terminal image protocols, SVG parsing, and file I/O.
pub const LOGO_ART_SMALL: &str = include_str!("../assets/logo.txt");

/// Warm loading colors, moving from the brand orange through amber and back.
/// The glyph changes silhouette at the same time, which keeps the animation
/// legible in terminals that quantize truecolor.
const LOADING_RGB: [Color; 8] = [
    Color::Rgb(255, 92, 39),
    Color::Rgb(255, 119, 42),
    Color::Rgb(255, 151, 49),
    Color::Rgb(255, 190, 69),
    Color::Rgb(255, 215, 112),
    Color::Rgb(255, 165, 55),
    Color::Rgb(255, 112, 39),
    Color::Rgb(184, 74, 30),
];

pub(crate) fn loading_color(phase: usize) -> Color {
    LOADING_RGB[phase % LOADING_RGB.len()]
}

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
    pub fn loading(&self, phase: usize) -> Style {
        self.paint(loading_color(phase))
    }
    /// One character in the Codex-style activity shimmer. The moving band
    /// blends quiet grey toward white; bold keeps the sweep legible at small
    /// terminal font sizes. `NO_COLOR` reduces it to unstyled text.
    pub fn shimmer(&self, intensity: f32) -> Style {
        if self.no_color {
            return Style::new();
        }
        let alpha = intensity.clamp(0.0, 1.0) * 0.9;
        let base = 128.0;
        let highlight = 255.0;
        let level = (highlight * alpha + base * (1.0 - alpha)) as u8;
        Style::new()
            .fg(Color::Rgb(level, level, level))
            .add_modifier(Modifier::BOLD)
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
    /// Submitted user turns — a quiet grey bubble that separates prompts from
    /// assistant output. Reversed video provides the same enclosure when
    /// `NO_COLOR` is active.
    pub fn user_prompt(&self) -> Style {
        if self.no_color {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(Color::White).bg(Color::DarkGray)
        }
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
    /// A changed file row: retain the terminal's normal foreground over a
    /// subtle red/green tint. Under `NO_COLOR`, reversed video keeps the row
    /// visibly highlighted.
    pub fn diff_highlight(&self, c: Color) -> Style {
        if self.no_color {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            let background = match c {
                Color::Green => DIFF_ADDED_BG,
                Color::Red => DIFF_REMOVED_BG,
                other => other,
            };
            Style::new().bg(background)
        }
    }
    /// The menu's selected row: brand bg, black text; reversed under `NO_COLOR`.
    pub fn menu_selected(&self) -> Style {
        if self.no_color {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().fg(Color::Black).bg(ACCENT_RGB)
        }
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
pub fn logo_glyph() -> &'static str {
    static GLYPH: OnceLock<String> = OnceLock::new();
    GLYPH
        .get_or_init(|| {
            let custom = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|home| home.join(".sc").join("logo.txt"))
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|body| {
                    body.lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                        .map(|line| line.chars().take(8).collect::<String>())
                })
                .filter(|line| !line.is_empty());
            custom.unwrap_or_else(|| DEFAULT_LOGO.to_string())
        })
        .as_str()
}
