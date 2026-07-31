//! Minimal markdown → ratatui `Line` renderer (M4b).
//!
//! A pragmatic subset for styling assistant output: ATX headings, fenced code
//! blocks, blockquotes, ordered/unordered lists, and inline `` `code` ``,
//! `**strong**`, `*emphasis*`, `_emphasis_`, and `[text](url)`. Block parsing is
//! line-based with code-fence state; inline parsing is a single recursive char
//! scan over ASCII markers. It is deliberately not a full CommonMark impl —
//! nested emphasis is flattened to the outer style, and edge cases degrade to
//! plain text. The goal is readable styled output, not spec compliance.
//!
//! Incremental rendering (§12 — the O(n²) trap): callers parse a *completed*
//! turn once and cache the returned lines; only the in-progress text is
//! re-parsed per frame. [`parse_blocks`](crate::markdown::parse_blocks) is
//! pure and cheap to call on the small growing buffer.

use ratatui::style::{Modifier, Style};

use crate::theme;
use ratatui::text::{Line, Span};

fn heading_style(level: usize) -> Style {
    // Headings are the one place the accent lives. Level deepens toward chrome.
    let p = theme::palette();
    let base = match level {
        1 | 2 => p.accent(),
        3 => p.accent_dim(),
        _ => p.chrome(),
    };
    base.add_modifier(Modifier::BOLD)
}

fn code_span_style() -> Style {
    theme::palette().code()
}

fn code_block_style() -> Style {
    theme::palette().code()
}

fn quote_style() -> Style {
    theme::palette().chrome()
}

fn link_style() -> Style {
    theme::palette().link()
}

/// Parse a block of markdown into styled lines (one logical line per source
/// line). The caller wraps these to the terminal width.
pub fn parse_blocks(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut fence_marker: Option<&str> = None; // the opening ``` or ~~~ run

    for line in text.split('\n') {
        let trimmed = line.trim_start();

        if in_fence {
            // A closing fence is a line of only the opening marker (3+).
            if fence_marker.is_some_and(|m| {
                let mc = m.chars().next().unwrap();
                trimmed.len() >= 3 && trimmed.chars().all(|c| c == mc)
            }) {
                in_fence = false;
                fence_marker = None;
                continue;
            }
            out.push(Line::styled(format!("  {line}"), code_block_style()));
            continue;
        }

        // Opening fence: 3+ backticks or tildes at the start (info string ignored).
        if trimmed.starts_with("```") {
            in_fence = true;
            fence_marker = Some("`");
            continue;
        }
        if trimmed.starts_with("~~~") {
            in_fence = true;
            fence_marker = Some("~");
            continue;
        }

        if let Some(rest) = strip_heading(line) {
            let level = line.chars().take_while(|&c| c == '#').count();
            out.push(Line::from(parse_inline(rest.trim()))
                .style(heading_style(level)));
            continue;
        }
        if let Some(rest) = strip_prefix(line, "> ") {
            out.push(Line::from(parse_inline(rest)).style(quote_style()));
            continue;
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
            let rest = &trimmed[2..];
            let mut spans = vec![Span::styled("• ", theme::palette().chrome())];
            spans.extend(parse_inline(rest));
            out.push(Line::from(spans));
            continue;
        }
        if let Some(rest) = strip_ordered(line) {
            let num: String = line.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
            let mut spans = vec![Span::styled(format!("{num}. "), theme::palette().chrome())];
            spans.extend(parse_inline(rest));
            out.push(Line::from(spans));
            continue;
        }
        if trimmed.is_empty() {
            out.push(Line::default());
            continue;
        }
        out.push(Line::from(parse_inline(line)));
    }
    out
}

/// If `line` is an ATX heading (`# ...`), return the text after the `# ` prefix.
fn strip_heading(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    rest.strip_prefix(' ').or_else(|| (rest.is_empty()).then_some(""))
}

fn strip_prefix<'a>(line: &'a str, p: &str) -> Option<&'a str> {
    let t = line.trim_start();
    t.strip_prefix(p)
}

/// If `line` is an ordered-list item (`1. ` / `1) `), return the text after the marker.
fn strip_ordered(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let after = &t[digits..];
    after.strip_prefix(". ").or_else(|| after.strip_prefix(") "))
}

// ---- inline -----------------------------------------------------------------

/// Parse inline markdown into styled spans (recursive over strong/emphasis).
pub fn parse_inline(s: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    let bytes = s.as_bytes();

    while i < s.len() {
        let rest = &s[i..];

        if let Some((content, consumed)) = take_code_span(rest) {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(content, code_span_style()));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_link(rest) {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(content, link_style()));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_strong(rest, b"**") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(content, Style::default().add_modifier(Modifier::BOLD)));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_strong(rest, b"__") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(content, Style::default().add_modifier(Modifier::BOLD)));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_emphasis(rest, b"*") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(content, Style::default().add_modifier(Modifier::ITALIC)));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_emphasis(rest, b"_") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(content, Style::default().add_modifier(Modifier::ITALIC)));
            i += consumed;
            continue;
        }
        // No marker here: copy one char to the plain buffer (char-boundary safe).
        let ch = s[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush_plain(&mut spans, &mut plain);
    let _ = bytes; // (markers above are ASCII; bytes kept for readability)
    spans
}

fn flush_plain(spans: &mut Vec<Span<'static>>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::raw(std::mem::take(plain)));
    }
}

/// `` `code` `` (a run of n backticks closed by a matching run of n). Returns
/// (content, bytes consumed). `None` if there is no closing run (literal text).
fn take_code_span(rest: &str) -> Option<(String, usize)> {
    let n = rest.chars().take_while(|&c| c == '`').count();
    if n == 0 {
        return None;
    }
    let open_len = n; // all backticks
    let after = &rest[open_len..];
    // Find a run of exactly n backticks further in.
    if let Some(pos) = find_backtick_run(after, n) {
        let content = &after[..pos];
        let close_len = n;
        let consumed = open_len + pos + close_len;
        Some((content.to_string(), consumed))
    } else {
        None
    }
}

/// Find the byte index of a run of exactly `n` backticks in `s`.
fn find_backtick_run(s: &str, n: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = s[i..].chars().take_while(|&c| c == '`').count();
            if run >= n {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// `[text](url)` → text. `None` if not a well-formed link.
fn take_link(rest: &str) -> Option<(String, usize)> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let close = rest.find(']')?;
    let text = &rest[1..close];
    let after = &rest[close + 1..];
    let url_part = after.strip_prefix('(')?;
    let url_end = url_part.find(')')?;
    let _url = &url_part[..url_end];
    let consumed = 1 + close + 1 + url_end + 1;
    Some((text.to_string(), consumed))
}

/// `**bold**` / `__bold__` (marker is `m`). Returns (content, bytes consumed).
/// Nested emphasis inside is flattened to the outer (bold) style.
fn take_strong(rest: &str, m: &[u8]) -> Option<(String, usize)> {
    if !rest.as_bytes().starts_with(m) {
        return None;
    }
    let open_len = m.len();
    let after = &rest[open_len..];
    let closer = find_marker(after, m)?;
    let content = &after[..closer];
    let consumed = open_len + closer + open_len;
    Some((content.to_string(), consumed))
}

/// `*italic*` / `_italic_` (single-char marker `m`), but not `**`/`__`.
fn take_emphasis(rest: &str, m: &[u8]) -> Option<(String, usize)> {
    if !rest.as_bytes().starts_with(m) {
        return None;
    }
    // Not part of a double marker.
    if rest.as_bytes().get(1) == Some(&m[0]) {
        return None;
    }
    let after = &rest[1..];
    let closer = find_marker(after, m)?;
    let content = &after[..closer];
    let consumed = 1 + closer + 1;
    Some((content.to_string(), consumed))
}

/// Byte index of the next occurrence of the ASCII marker `m` in `s`.
fn find_marker(s: &str, m: &[u8]) -> Option<usize> {
    s.find(std::str::from_utf8(m).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn heading_is_bold_colored() {
        let l = parse_blocks("# Title");
        assert_eq!(plain(&l), vec!["Title".to_string()]);
        assert!(l[0].style.add_modifier == Modifier::BOLD, "heading is bold");
    }

    #[test]
    fn code_fence_body_is_plain_block() {
        let l = parse_blocks("```rust\nlet x = 1;\n```");
        // The opening/closing fences are consumed; the body is one indented line.
        assert_eq!(plain(&l), vec!["  let x = 1;".to_string()]);
    }

    #[test]
    fn unordered_list_gets_a_bullet() {
        let l = parse_blocks("- one\n- two");
        assert_eq!(plain(&l), vec!["• one".to_string(), "• two".to_string()]);
    }

    #[test]
    fn ordered_list_keeps_its_number() {
        let l = parse_blocks("1. first\n2. second");
        assert_eq!(plain(&l), vec!["1. first".to_string(), "2. second".to_string()]);
    }

    #[test]
    fn inline_code_strong_and_emphasis() {
        let l = parse_blocks("use `fmt` for **bold** and *ital*.");
        assert_eq!(plain(&l), vec!["use fmt for bold and ital.".to_string()]);
    }

    #[test]
    fn link_renders_its_text() {
        let l = parse_blocks("see [docs](https://x) now");
        assert_eq!(plain(&l), vec!["see docs now".to_string()]);
    }

    #[test]
    fn blockquote_is_its_own_line() {
        let l = parse_blocks("> quoted");
        assert_eq!(plain(&l), vec!["quoted".to_string()]);
    }

    #[test]
    fn unterminated_marker_is_literal() {
        // No closing ** -> literal asterisks, not a crash.
        let l = parse_blocks("a **b c");
        assert_eq!(plain(&l), vec!["a **b c".to_string()]);
    }
}
