//! Word-level diff for `Edit` previews (M4b).
//!
//! Tokenizes old/new on unicode word boundaries (via `similar`'s `unicode`
//! feature) and emits a single styled line: removed words red + crossed-out,
//! added words green + bold, unchanged words plain. Rendered inline so the user
//! sees exactly what changed at a glance.

use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use similar::text::{ChangeTag, TextDiff};

use crate::theme;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

/// One styled line summarizing the word-level change from `old` to `new`.
pub fn word_diff_line(old: &str, new: &str) -> Line<'static> {
    let p = theme::palette();
    let diff = TextDiff::from_unicode_words(old, new);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let value = change.value();
            let span = match change.tag() {
                ChangeTag::Delete => Span::styled(
                    value.to_string(),
                    p.semantic(Color::Red).add_modifier(Modifier::CROSSED_OUT),
                ),
                ChangeTag::Insert => Span::styled(
                    value.to_string(),
                    p.semantic(Color::Green).add_modifier(Modifier::BOLD),
                ),
                ChangeTag::Equal => Span::raw(value.to_string()),
            };
            spans.push(span);
        }
    }
    Line::from(spans)
}

/// Render the actual changed lines from one successful file mutation. Equal
/// lines are omitted so a large full-file `Write` remains focused on what the
/// agent changed, while line numbers keep every insertion/deletion locatable.
/// Removed rows use a solid red highlight and inserted rows a solid green one.
pub fn file_diff_lines(
    path: &str,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> (Vec<Line<'static>>, DiffStats) {
    let before = before.map(String::from_utf8_lossy).unwrap_or_default();
    let after = after.map(String::from_utf8_lossy).unwrap_or_default();
    let diff = TextDiff::from_lines(before.as_ref(), after.as_ref());
    let mut stats = DiffStats::default();
    let mut body = Vec::new();
    let p = theme::palette();

    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let (marker, number, style) = match change.tag() {
                ChangeTag::Delete => {
                    stats.removed = stats.removed.saturating_add(1);
                    ("-", change.old_index(), p.diff_highlight(Color::Red))
                }
                ChangeTag::Insert => {
                    stats.added = stats.added.saturating_add(1);
                    ("+", change.new_index(), p.diff_highlight(Color::Green))
                }
                ChangeTag::Equal => continue,
            };
            let number = number.map_or_else(|| "   ".to_string(), |n| format!("{:>3}", n + 1));
            let value = change.value().trim_end_matches(&['\r', '\n'][..]);
            body.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{marker}{number} │ "), style),
                Span::styled(format!("{value} "), style),
            ]));
        }
    }

    let mut lines = vec![Line::from(vec![
        Span::styled("  ⤿ ".to_string(), p.chrome()),
        Span::styled(path.to_string(), p.accent()),
        Span::styled("  ".to_string(), p.chrome()),
        Span::styled(format!("+{}", stats.added), p.semantic(Color::Green)),
        Span::styled(" ".to_string(), p.chrome()),
        Span::styled(format!("-{}", stats.removed), p.semantic(Color::Red)),
    ])];
    lines.extend(body);
    (lines, stats)
}

pub fn line_stats(before: Option<&[u8]>, after: Option<&[u8]>) -> DiffStats {
    let before = before.map(String::from_utf8_lossy).unwrap_or_default();
    let after = after.map(String::from_utf8_lossy).unwrap_or_default();
    let mut stats = DiffStats::default();
    let diff = TextDiff::from_lines(before.as_ref(), after.as_ref());
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Delete => stats.removed = stats.removed.saturating_add(1),
                ChangeTag::Insert => stats.added = stats.added.saturating_add(1),
                ChangeTag::Equal => {}
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn unchanged_text_passes_through() {
        let l = word_diff_line("same", "same");
        assert_eq!(line_text(&l), "same");
    }

    #[test]
    fn a_word_swap_shows_delete_and_insert() {
        // "the quick fox" -> "the slow fox": "quick" deleted, "slow" inserted.
        let l = word_diff_line("the quick fox", "the slow fox");
        let text = line_text(&l);
        assert!(
            text.contains("quick"),
            "kept the deleted word (struck): {text}"
        );
        assert!(text.contains("slow"), "kept the inserted word: {text}");
        // The deleted word is red+crossed, the inserted green+bold.
        let deleted = l.spans.iter().find(|s| s.content == "quick").unwrap();
        assert!(
            deleted.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "deleted word is crossed out"
        );
        let inserted = l.spans.iter().find(|s| s.content == "slow").unwrap();
        assert!(
            inserted.style.add_modifier.contains(Modifier::BOLD),
            "inserted word is bold"
        );
    }

    #[test]
    fn empty_old_treats_everything_as_inserted() {
        let l = word_diff_line("", "brand new text");
        assert_eq!(line_text(&l), "brand new text");
        assert!(l
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn file_diff_reports_and_renders_changed_lines() {
        let (lines, stats) = file_diff_lines(
            "src/main.rs",
            Some(b"one\ntwo\n"),
            Some(b"one\nthree\nfour\n"),
        );
        assert_eq!(
            stats,
            DiffStats {
                added: 2,
                removed: 1
            }
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("src/main.rs"), "{text}");
        assert!(text.contains("two"), "{text}");
        assert!(text.contains("three"), "{text}");
        assert!(text.contains("four"), "{text}");
        assert!(!text.contains("one"), "unchanged lines stay hidden: {text}");

        for row in lines.iter().skip(1) {
            assert_eq!(row.spans[0].content.as_ref(), "  ");
            assert_eq!(row.spans[0].style.bg, None, "gutter stays unhighlighted");
            assert!(
                row.spans[1].style.bg.is_some()
                    || row.spans[1].style.add_modifier.contains(Modifier::REVERSED),
                "highlight begins after the two-cell gutter: {:?}",
                row.spans
            );
        }

        let removed = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("two"))
            .expect("removed row");
        assert!(
            (removed.style.bg == Some(theme::DIFF_REMOVED_BG) && removed.style.fg.is_none())
                || removed.style.add_modifier.contains(Modifier::REVERSED),
            "removed row is softly red-highlighted, or reversed under NO_COLOR: {:?}",
            removed.style
        );

        let added = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("three"))
            .expect("added row");
        assert!(
            (added.style.bg == Some(theme::DIFF_ADDED_BG) && added.style.fg.is_none())
                || added.style.add_modifier.contains(Modifier::REVERSED),
            "added row is softly green-highlighted, or reversed under NO_COLOR: {:?}",
            added.style
        );
    }
}
