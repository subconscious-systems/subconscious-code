//! Word-level diff for `Edit` previews (M4b).
//!
//! Tokenizes old/new on unicode word boundaries (via `similar`'s `unicode`
//! feature) and emits a single styled line: removed words red + crossed-out,
//! added words green + bold, unchanged words plain. Rendered inline so the user
//! sees exactly what changed at a glance.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::text::{ChangeTag, TextDiff};

/// One styled line summarizing the word-level change from `old` to `new`.
pub fn word_diff_line(old: &str, new: &str) -> Line<'static> {
    let diff = TextDiff::from_unicode_words(old, new);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let value = change.value();
            let span = match change.tag() {
                ChangeTag::Delete => Span::styled(
                    value.to_string(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::CROSSED_OUT),
                ),
                ChangeTag::Insert => Span::styled(
                    value.to_string(),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                ChangeTag::Equal => Span::raw(value.to_string()),
            };
            spans.push(span);
        }
    }
    Line::from(spans)
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
        assert!(text.contains("quick"), "kept the deleted word (struck): {text}");
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
        assert!(l.spans.iter().all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }
}
