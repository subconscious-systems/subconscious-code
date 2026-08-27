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
//! turn once and cache the returned lines; the in-progress text is parsed at
//! most once per token (the TUI caches it behind a dirty flag in
//! [`ViewState`](crate::view::ViewState) and reuses it across frames, so a
//! long streaming reply is never re-parsed every frame).
//! [`parse_blocks`](crate::markdown::parse_blocks) is pure and cheap to call
//! on the small growing buffer.

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

/// The top of a fenced code block: a corner glyph plus the language tag (the
/// info string) in the brand accent, or a bare corner when no language is
/// given. The block draws as a left-guttered box (┌ / │ / └) inside the flat
/// `Vec<Line>` the transcript renders — no separate widget, so it still wraps
/// and scrolls with the surrounding paragraph.
fn code_block_header(lang: &str) -> Line<'static> {
    let p = theme::palette();
    if lang.is_empty() {
        Line::styled("┌".to_string(), p.chrome())
    } else {
        Line::from(vec![
            Span::styled("┌ ".to_string(), p.chrome()),
            Span::styled(lang.to_string(), p.accent()),
        ])
    }
}

/// One body line of a fenced block: a chrome gutter `│ ` then the code in the
/// code hue. The original (untrimmed) line is kept so inner indentation reads.
fn code_block_body_line(line: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("│ ".to_string(), theme::palette().chrome()),
        Span::styled(line.to_string(), code_block_style()),
    ])
}

/// The bottom of a fenced code block: a bare corner.
fn code_block_footer() -> Line<'static> {
    Line::styled("└".to_string(), theme::palette().chrome())
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
                                               // Index-based so a multi-line construct (a table) can peek the next line
                                               // and consume several at once.
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        if in_fence {
            // A closing fence is a line of only the opening marker (3+).
            if fence_marker.is_some_and(|m| {
                let mc = m.chars().next().unwrap();
                trimmed.len() >= 3 && trimmed.chars().all(|c| c == mc)
            }) {
                in_fence = false;
                fence_marker = None;
                out.push(code_block_footer());
                i += 1;
                continue;
            }
            out.push(code_block_body_line(line));
            i += 1;
            continue;
        }

        // Opening fence: 3+ backticks or tildes at the start. The info string
        // (after the fence) is the language tag — shown in the block header.
        // Only the first whitespace token is the language; the rest is attributes.
        if let Some(rest) = trimmed.strip_prefix("```") {
            in_fence = true;
            fence_marker = Some("`");
            out.push(code_block_header(
                rest.split_whitespace().next().unwrap_or(""),
            ));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("~~~") {
            in_fence = true;
            fence_marker = Some("~");
            out.push(code_block_header(
                rest.split_whitespace().next().unwrap_or(""),
            ));
            i += 1;
            continue;
        }

        // GFM table: a `|`-row immediately followed by a delimiter row
        // (`|---|:--:|--:|`). The delimiter must have the same column count as
        // the header, so a bare `---` under a 2-column header isn't mistaken
        // for one. Body rows are every following `|`-line until a blank or
        // non-`|` line ends the table.
        if trimmed.contains('|') && i + 1 < lines.len() {
            let hcells = split_row(line);
            let dcells = split_row(lines[i + 1]);
            if !hcells.is_empty() && hcells.len() == dcells.len() && is_delimiter_row(&dcells) {
                i += 2; // consume header + delimiter
                let mut rows: Vec<Vec<String>> = Vec::new();
                while i < lines.len() {
                    let rt = lines[i].trim();
                    if rt.is_empty() || !rt.contains('|') {
                        break;
                    }
                    rows.push(split_row(lines[i]));
                    i += 1;
                }
                out.extend(render_table(&hcells, &rows));
                continue;
            }
        }

        // Thematic break: 3+ of one marker (`-`, `*`, `_`) with optional
        // spaces — the `---` models love to drop between sections. Checked
        // before lists so `* * *` / `- - -` don't read as a one-item list.
        if is_thematic_break(trimmed) {
            out.push(thematic_break_line());
            i += 1;
            continue;
        }

        if let Some(rest) = strip_heading(line) {
            let level = line.chars().take_while(|&c| c == '#').count();
            out.push(Line::from(parse_inline(rest.trim())).style(heading_style(level)));
            i += 1;
            continue;
        }
        if let Some(rest) = strip_prefix(line, "> ") {
            out.push(Line::from(parse_inline(rest)).style(quote_style()));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            let mut spans = vec![Span::styled("• ", theme::palette().chrome())];
            spans.extend(parse_inline(rest));
            out.push(Line::from(spans));
            i += 1;
            continue;
        }
        if let Some(rest) = strip_ordered(line) {
            let num: String = line
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let mut spans = vec![Span::styled(format!("{num}. "), theme::palette().chrome())];
            spans.extend(parse_inline(rest));
            out.push(Line::from(spans));
            i += 1;
            continue;
        }
        if trimmed.is_empty() {
            out.push(Line::default());
            i += 1;
            continue;
        }
        out.push(Line::from(parse_inline(line)));
        i += 1;
    }
    out
}

// ---- tables ----------------------------------------------------------------

/// Split a GFM table row into trimmed cells. A single leading/trailing `|` is
/// stripped; `\|` is an escaped pipe (a literal `|` inside a cell), not a
/// separator.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if matches!(chars.peek(), Some('|')) {
                cur.push('|');
                chars.next();
                continue;
            }
            cur.push(c);
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur = String::new();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// A GFM delimiter row: every cell is only `-` and `:` with at least one `-`
/// (so `---`, `:--`, `--:`, `:-:` all qualify; `:::` does not).
fn is_delimiter_row(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let s = c.trim();
            !s.is_empty() && s.chars().all(|ch| ch == '-' || ch == ':') && s.contains('-')
        })
}

/// Render a GFM table as a box-drawn block (┌┬┐ / │ │ / ├┼┤ / └┴┘), columns
/// padded to the widest cell. The header row is accent + bold; body cells keep
/// inline markdown (`**bold**`, `` `code` `` …). Width-agnostic like the code
/// block — a table wider than the terminal wraps, the same tradeoff.
fn render_table(header: &[String], rows: &[Vec<String>]) -> Vec<Line<'static>> {
    let p = theme::palette();
    let ncols = header.len();
    let mut widths = vec![0usize; ncols];
    for (i, c) in header.iter().enumerate() {
        widths[i] = widths[i].max(c.chars().count());
    }
    for row in rows {
        for (i, c) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let mut out = Vec::new();
    out.push(Line::styled(
        border_line(&widths, '┌', '┬', '┐'),
        p.chrome(),
    ));
    out.push(table_row_line(header, &widths, true));
    out.push(Line::styled(
        border_line(&widths, '├', '┼', '┤'),
        p.chrome(),
    ));
    for (index, row) in rows.iter().enumerate() {
        out.push(table_row_line(row, &widths, false));
        if index + 1 < rows.len() {
            out.push(Line::styled(
                border_line(&widths, '├', '┼', '┤'),
                p.chrome(),
            ));
        }
    }
    out.push(Line::styled(
        border_line(&widths, '└', '┴', '┘'),
        p.chrome(),
    ));
    out
}

/// One border line of the table: `left`, `─`×(w+2) per column joined by `mid`,
/// then `right`.
fn border_line(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut s = String::from(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            s.push(mid);
        }
        for _ in 0..(w + 2) {
            s.push('─');
        }
    }
    s.push(right);
    s
}

/// One `│ cell │ cell │` row, each cell padded to its column width. Header
/// cells are accent + bold; body cells parse inline markdown.
fn table_row_line(cells: &[String], widths: &[usize], header: bool) -> Line<'static> {
    let p = theme::palette();
    let mut spans: Vec<Span<'static>> = vec![Span::styled("│", p.chrome())];
    for (i, w) in widths.iter().enumerate() {
        spans.push(Span::styled(" ", p.chrome()));
        let cell = cells.get(i).map(|s| s.as_str()).unwrap_or("");
        if header {
            spans.push(Span::styled(
                cell.to_string(),
                p.accent().add_modifier(Modifier::BOLD),
            ));
            let dw = cell.chars().count();
            if dw < *w {
                spans.push(Span::raw(" ".repeat(w - dw)));
            }
        } else {
            let mut cell_spans = parse_inline(cell);
            let dw: usize = cell_spans.iter().map(|s| s.content.chars().count()).sum();
            if dw < *w {
                cell_spans.push(Span::raw(" ".repeat(w - dw)));
            }
            spans.extend(cell_spans);
        }
        spans.push(Span::styled(" ", p.chrome()));
        spans.push(Span::styled("│", p.chrome()));
    }
    Line::from(spans)
}

// ---- thematic break --------------------------------------------------------

/// Is `s` a GFM thematic break: 3+ of a single marker (`-`, `*`, `_`), the rest
/// spaces? (`---`, `* * *`, `___` all qualify; `- x` does not.)
fn is_thematic_break(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let Some(marker) = s.chars().next() else {
        return false;
    };
    if marker != '-' && marker != '*' && marker != '_' {
        return false;
    }
    s.chars().all(|c| c == marker || c == ' ') && s.chars().filter(|c| *c == marker).count() >= 3
}

/// A dim horizontal rule. The renderer has no width at parse time (lines wrap
/// later), so a fixed-length rule is the consistent choice — same width-agnostic
/// approach as the code-block gutter.
fn thematic_break_line() -> Line<'static> {
    Line::styled("─".repeat(40), theme::palette().chrome())
}

/// If `line` is an ATX heading (`# ...`), return the text after the `# ` prefix.
fn strip_heading(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    rest.strip_prefix(' ')
        .or_else(|| (rest.is_empty()).then_some(""))
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
    after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))
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
        if let Some((content, consumed)) = take_strikethrough(rest) {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(
                content,
                Style::default().add_modifier(Modifier::CROSSED_OUT),
            ));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_strong(rest, b"**") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(
                content,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_strong(rest, b"__") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(
                content,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_emphasis(rest, b"*") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(
                content,
                Style::default().add_modifier(Modifier::ITALIC),
            ));
            i += consumed;
            continue;
        }
        if let Some((content, consumed)) = take_emphasis(rest, b"_") {
            flush_plain(&mut spans, &mut plain);
            spans.push(Span::styled(
                content,
                Style::default().add_modifier(Modifier::ITALIC),
            ));
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

/// `~~strikethrough~~` (two tildes). Not `~~~` (triple). Returns (content,
/// bytes consumed). `None` if there is no closing `~~` (literal text).
fn take_strikethrough(rest: &str) -> Option<(String, usize)> {
    if !rest.as_bytes().starts_with(b"~~") {
        return None;
    }
    // Not part of a triple-tilde run.
    if rest.as_bytes().get(2) == Some(&b'~') {
        return None;
    }
    let after = &rest[2..];
    let closer = find_marker(after, b"~~")?;
    let content = &after[..closer];
    let consumed = 2 + closer + 2;
    Some((content.to_string(), consumed))
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
        // The block renders as a guttered box: a header with the language tag,
        // the body lines under a `│ ` gutter, and a closing corner.
        assert_eq!(
            plain(&l),
            vec![
                "┌ rust".to_string(),
                "│ let x = 1;".to_string(),
                "└".to_string()
            ]
        );
    }

    #[test]
    fn code_fence_without_language_gets_a_bare_corner() {
        let l = parse_blocks("```\ncode\n```");
        assert_eq!(
            plain(&l),
            vec!["┌".to_string(), "│ code".to_string(), "└".to_string()]
        );
    }

    #[test]
    fn unordered_list_gets_a_bullet() {
        let l = parse_blocks("- one\n- two");
        assert_eq!(plain(&l), vec!["• one".to_string(), "• two".to_string()]);
    }

    #[test]
    fn ordered_list_keeps_its_number() {
        let l = parse_blocks("1. first\n2. second");
        assert_eq!(
            plain(&l),
            vec!["1. first".to_string(), "2. second".to_string()]
        );
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

    #[test]
    fn thematic_break_renders_as_a_rule() {
        // `---` / `***` / `___` (3+ of one marker) -> a dim rule, not literal.
        for src in ["---", "***", "___", "- - -", "* * *"] {
            let l = parse_blocks(src);
            let p = plain(&l);
            assert_eq!(l.len(), 1, "one rule line for {src:?}: {p:?}");
            assert!(p[0].chars().all(|c| c == '─'), "rule is all ─: {p:?}");
            assert!(p[0].chars().count() >= 3, "rule spans some width: {p:?}");
        }
        // `--` (only two) is NOT a break — it falls through to plain text.
        let l = parse_blocks("--");
        assert_eq!(plain(&l), vec!["--".to_string()]);
    }

    #[test]
    fn strikethrough_renders_crossed_out() {
        let l = parse_blocks("~~removed~~ stays");
        assert_eq!(plain(&l), vec!["removed stays".to_string()]);
        let struck = l[0].spans.iter().find(|s| s.content == "removed").unwrap();
        assert!(
            struck.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "struck text is crossed out"
        );
        // Unterminated ~~ is literal.
        let l = parse_blocks("a ~~b c");
        assert_eq!(plain(&l), vec!["a ~~b c".to_string()]);
    }

    #[test]
    fn gfm_table_renders_boxed_with_aligned_columns() {
        let src = "| Name | Role |\n|------|------|\n| Ada | author |\n| Grace | engineer |";
        let l = parse_blocks(src);
        let text = plain(&l);
        // Top border, header separator, a separator between body rows, and the
        // bottom border.
        assert_eq!(text.len(), 7, "table is 7 lines: {text:?}");
        assert!(text[0].starts_with('┌'), "top border: {text:?}");
        assert!(text[0].ends_with('┐'), "top border corner: {text:?}");
        assert!(
            text.contains(&"│ Name  │ Role     │".to_string()),
            "header padded: {text:?}"
        );
        assert!(
            text.contains(&"│ Ada   │ author   │".to_string()),
            "row padded: {text:?}"
        );
        assert!(
            text.contains(&"│ Grace │ engineer │".to_string()),
            "row padded: {text:?}"
        );
        assert_eq!(
            text.iter().filter(|row| row.starts_with('├')).count(),
            2,
            "header and every data row are visually separated: {text:?}"
        );
        assert!(
            text.last().unwrap().starts_with('└'),
            "bottom border: {text:?}"
        );
    }

    #[test]
    fn table_ends_at_a_blank_line_and_does_not_eat_following_text() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n\nnot a table";
        let l = parse_blocks(src);
        let text = plain(&l);
        // Table (5 lines) + the trailing paragraph.
        assert!(
            text.contains(&"not a table".to_string()),
            "text after table preserved: {text:?}"
        );
        assert!(
            !text.iter().any(|t| t.contains("not a") && t.contains("│")),
            "table didn't swallow the paragraph: {text:?}"
        );
    }

    #[test]
    fn bare_dashes_under_a_header_are_not_a_one_column_table() {
        // A 2-column header followed by a bare `---` (no pipes) is NOT a GFM
        // table — the column counts mismatch. The header renders as text and
        // `---` as a thematic break.
        let src = "| a | b |\n---";
        let l = parse_blocks(src);
        let text = plain(&l);
        assert!(
            text.contains(&"| a | b |".to_string()),
            "header stays text: {text:?}"
        );
        assert!(
            text.iter().any(|t| t.chars().all(|c| c == '─')),
            "dashes become a rule: {text:?}"
        );
    }
}
