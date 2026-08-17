//! Composer autocomplete (M4c).
//!
//! A pure, synchronous completion engine for the single-line composer: `@`
//! file-path mentions and `/` slash commands. No tokio, no channels — the TUI
//! calls [`complete`] on the composer buffer each frame a menu is open, and
//! applies [`Completion`] with [`apply`]. Both helpers are plain functions of
//! the buffer + a directory root, so they are trivially testable with no TUI.
//!
//! Design notes:
//! - File completion walks the session cwd once (bounded, non-recursive for
//!   the prefix's own directory) and matches the prefix case-insensitively;
//!   hidden files are skipped unless the user typed the leading dot.
//! - Slash commands are a fixed, small palette (no external registry): the
//!   ones the runtime already supports plus `/help`.
//! - Everything is sync and cheap; the TUI re-runs [`complete`] per frame
//!   only while the menu is open, and caps the displayed rows.
//!
//! Rendering of the menu itself lives in [`crate::view`]; this module only
//! computes the candidates and the replacement.

use std::path::Path;

/// The kind of completion menu to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// `@path...` — file/directory mentions.
    File,
    /// `/cmd...` — slash commands.
    Slash,
}

/// A computed completion result: the menu kind, the shared prefix being
/// replaced, and the ordered candidate labels.
#[derive(Debug, Clone)]
pub struct Completion {
    pub kind: MenuKind,
    /// Byte range in the composer buffer to replace (`start..end`), where
    /// `end` is the current caret (end of the buffer for the single-line
    /// composer). `start` is the index of the `@` / `/`.
    pub replace_start: usize,
    /// The candidate labels to display, in priority order. Already truncated
    /// to a display cap by the caller; this is the raw ordered list.
    pub candidates: Vec<String>,
}

/// The fixed slash-command palette — the union of the slash commands shipped
/// by Claude Code, Codex, and Cursor (de-duplicated; aliases live in
/// [`crate::app`]'s `handle_slash`, not here, so they don't clutter the menu).
/// `/clear`, `/mode`, and `/rewind` map to host actions the TUI already
/// supported; the rest are either new host-side actions or prompt-expansion
/// commands that submit a canned instruction to the model.
///
/// `pub(crate)` so `app::run_slash` can render `/help` from this single source
/// of truth — the help text and the menu can never drift apart.
pub(crate) fn slash_palette() -> &'static [(&'static str, &'static str)] {
    &[
        // Conversation / context hygiene.
        ("/clear", "Clear the transcript and start a fresh turn"),
        (
            "/compact",
            "Compact the conversation (free context, keep a summary)",
        ),
        (
            "/context",
            "Show returned context tokens (or preflight estimate)",
        ),
        (
            "/rewind",
            "Restore files changed in the last turn (Write/Edit only)",
        ),
        // Session / environment introspection.
        ("/cost", "Show token usage for the last turn"),
        ("/usage", "Show token usage (alias of /cost)"),
        ("/menu", "Open the menu: projects, sessions, and settings"),
        ("/status", "Show session status (model, mode, cwd, busy)"),
        ("/model", "Show the active model"),
        ("/mode", "Show or cycle the permission mode"),
        (
            "/permissions",
            "Show the active permission mode and rule hints",
        ),
        ("/doctor", "Run a self-check of the environment and config"),
        ("/history", "Summarize the transcript length so far"),
        ("/export", "Export the transcript to a file"),
        // Lifecycle.
        ("/quit", "Quit the session"),
        ("/resume", "Resume a previous conversation"),
        ("/update", "Check for an sc CLI update"),
        ("/login", "Show authentication status"),
        ("/logout", "Show authentication status"),
        // Integrations / capabilities (notes — not all backends are wired yet).
        ("/mcp", "List connected MCP servers"),
        ("/memory", "Show the memory / CLAUDE.md location"),
        ("/add-dir", "Add a working directory to the session"),
        ("/vim", "Toggle vim keybindings (not yet wired)"),
        (
            "/terminal-setup",
            "Show terminal setup hints for Shift+Enter",
        ),
        ("/approval", "Show approval / permission mode"),
        // Prompt-expansion commands (submit a canned instruction to the model).
        (
            "/review",
            "Review the pending code changes on the current branch",
        ),
        ("/pr", "Create a pull request for the current branch"),
        ("/init", "Generate a CLAUDE.md documenting the codebase"),
        ("/diff", "Show the diff of pending changes"),
        (
            "/release-notes",
            "Summarize release notes from recent git history",
        ),
        ("/bug", "Help report a bug from recent errors"),
        ("/doc", "Generate documentation for the code"),
        ("/fix", "Find and fix bugs in the code"),
        ("/explain", "Explain the code"),
        ("/edit", "Apply edits to the code"),
        ("/codebase", "Summarize the structure of the codebase"),
        // Reference.
        ("/help", "Show composer keybindings and slash commands"),
    ]
}

/// Compute a completion for `buffer` with the caret at the end (the
/// single-line composer always has the caret at the end). Returns `None` if
/// no menu should be open (no `@`/`/` trigger at the start of a token).
///
/// The file root is the session cwd; completion is best-effort and never
/// panics on IO errors (it just returns fewer candidates).
pub fn complete(buffer: &str, root: &Path) -> Option<Completion> {
    let trigger = last_trigger(buffer)?;
    match trigger {
        Trigger::Slash { start } => {
            let prefix = &buffer[start + 1..];
            let mut candidates: Vec<String> = slash_palette()
                .iter()
                .filter(|(name, _)| prefix.is_empty() || name[1..].starts_with(prefix))
                .map(|(name, _)| (*name).to_string())
                .collect();
            // Stable, alphabetical order for a predictable menu.
            candidates.sort();
            if candidates.is_empty() {
                return None;
            }
            Some(Completion {
                kind: MenuKind::Slash,
                replace_start: start,
                candidates,
            })
        }
        Trigger::File { start } => {
            let prefix = &buffer[start + 1..];
            let candidates = file_candidates(prefix, root);
            if candidates.is_empty() {
                return None;
            }
            Some(Completion {
                kind: MenuKind::File,
                replace_start: start,
                candidates,
            })
        }
    }
}

/// A detected trigger: the index of the `@` or `/` that opened a menu.
#[derive(Debug, Clone, Copy)]
enum Trigger {
    Slash { start: usize },
    File { start: usize },
}

/// Find the trigger for a menu at the caret (end of `buffer`).
///
/// Rules:
/// - `/` opens the slash menu only at the very start of the buffer (so a
///   bare `/clear` works, but `foo /bar` does not).
/// - `@` opens the file menu whenever it begins a new whitespace-delimited
///   token (so `edit @src/` and `@README` both work), up to the caret.
fn last_trigger(buffer: &str) -> Option<Trigger> {
    if let Some(rest) = buffer.strip_prefix('/') {
        // Only a slash at the very start; require no spaces in the candidate
        // (a space means the user typed `/foo bar` which isn't a command).
        if !rest.contains(' ') {
            return Some(Trigger::Slash { start: 0 });
        }
    }
    // The rightmost `@` that begins a token (preceded by start or whitespace)
    // and whose token has no interior space.
    let bytes = buffer.as_bytes();
    let mut i = 0;
    let mut found: Option<Trigger> = None;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1] == b' ') {
            let rest = &buffer[i + 1..];
            if !rest.contains(' ') {
                found = Some(Trigger::File { start: i });
            }
        }
        i += 1;
    }
    found
}

/// Bounded, single-directory file listing matching `prefix` (case-insensitive).
/// Walks only the directory component of `prefix` (so `@src/ut` lists
/// `src/`), non-recursive; returns paths relative to `root` with a trailing
/// `/` for directories so the user can keep typing. Hidden entries are shown
/// only if `prefix` itself starts with `.`.
fn file_candidates(prefix: &str, root: &Path) -> Vec<String> {
    // Split the prefix into a directory to list and a name filter.
    let (dir_rel, name_prefix) = match prefix.rfind('/') {
        Some(idx) => (&prefix[..=idx], &prefix[idx + 1..]),
        None => ("", prefix),
    };
    let dir = if dir_rel.is_empty() {
        root.to_path_buf()
    } else {
        // Strip a leading `./` if present; otherwise join. Ignore errors.
        let cleaned = dir_rel.strip_prefix("./").unwrap_or(dir_rel);
        root.join(cleaned)
    };

    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let show_hidden = name_prefix.starts_with('.') || dir_rel.contains("/.");
    let lower_filter = name_prefix.to_ascii_lowercase();

    let mut out: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if !show_hidden && fname.starts_with('.') {
            continue;
        }
        if !fname.to_ascii_lowercase().starts_with(&lower_filter) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        // Path relative to root, preserving the user's prefix dir.
        let rel = if dir_rel.is_empty() {
            fname.into_owned()
        } else {
            format!("{dir_rel}{fname}")
        };
        let label = if is_dir { format!("{rel}/") } else { rel };
        out.push(label);
    }
    out.sort();
    out
}

/// Apply `completion`'s selected candidate (by index) to `buffer`, returning
/// the new buffer. Returns `None` if the index is out of bounds. The candidate
/// replaces `replace_start..` and keeps the leading `@` / `/`.
pub fn apply(buffer: &str, completion: &Completion, index: usize) -> Option<String> {
    let cand = completion.candidates.get(index)?;
    let lead = match completion.kind {
        MenuKind::File => "@",
        MenuKind::Slash => "",
    };
    let mut out = String::with_capacity(buffer.len() + cand.len());
    out.push_str(&buffer[..completion.replace_start]);
    out.push_str(lead);
    out.push_str(cand);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn files(root: &Path, names: &[&str]) {
        for n in names {
            let p = root.join(n);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, "x").unwrap();
        }
    }

    #[test]
    fn slash_at_start_offers_commands() {
        let c = complete("/", Path::new(".")).unwrap();
        assert_eq!(c.kind, MenuKind::Slash);
        assert_eq!(c.replace_start, 0);
        assert!(c.candidates.contains(&"/clear".to_string()));
        assert!(c.candidates.contains(&"/help".to_string()));
        assert!(c.candidates.contains(&"/mode".to_string()));
    }

    #[test]
    fn slash_filters_by_prefix() {
        let c = complete("/c", Path::new(".")).unwrap();
        // Every candidate starts with the `/c` prefix; the palette grew to many
        // `/c…` commands (clear, codebase, compact, context, cost), so assert
        // membership and the prefix invariant rather than an exact singleton.
        assert!(c.candidates.iter().all(|s| s.starts_with("/c")));
        assert!(c.candidates.contains(&"/clear".to_string()));
        assert!(c.candidates.contains(&"/compact".to_string()));
        assert!(c.candidates.contains(&"/cost".to_string()));
    }

    #[test]
    fn slash_with_unknown_prefix_is_none() {
        assert!(complete("/zzz", Path::new(".")).is_none());
    }

    #[test]
    fn slash_not_triggered_after_text() {
        // A slash not at the very start does not open the menu.
        assert!(complete("edit /x", Path::new(".")).is_none());
    }

    #[test]
    fn slash_with_space_is_not_a_command() {
        // `/foo bar` isn't a command candidate.
        assert!(complete("/foo bar", Path::new(".")).is_none());
    }

    #[test]
    fn at_lists_files_in_root() {
        let dir = tempdir().unwrap();
        files(dir.path(), &["README.md", "main.rs", ".hidden"]);
        let c = complete("@", dir.path()).unwrap();
        assert_eq!(c.kind, MenuKind::File);
        assert!(c.candidates.contains(&"README.md".to_string()));
        assert!(c.candidates.contains(&"main.rs".to_string()));
        // Hidden files are skipped unless the prefix starts with '.'.
        assert!(!c.candidates.iter().any(|s| s.contains(".hidden")));
    }

    #[test]
    fn at_filters_by_prefix_case_insensitive() {
        let dir = tempdir().unwrap();
        files(dir.path(), &["README.md", "main.rs", "rc.txt"]);
        let c = complete("@RE", dir.path()).unwrap();
        assert_eq!(c.candidates, vec!["README.md".to_string()]);
        let c = complete("@M", dir.path()).unwrap();
        assert_eq!(c.candidates, vec!["main.rs".to_string()]);
    }

    #[test]
    fn at_lists_a_subdirectory_with_trailing_slash() {
        let dir = tempdir().unwrap();
        files(dir.path(), &["src/main.rs", "src/util.rs", "README.md"]);
        let c = complete("@src/", dir.path()).unwrap();
        assert!(c.candidates.contains(&"src/main.rs".to_string()));
        assert!(c.candidates.contains(&"src/util.rs".to_string()));
        // The top-level file isn't listed when listing a subdirectory.
        assert!(!c.candidates.iter().any(|s| s == "README.md"));
    }

    #[test]
    fn at_directory_candidate_keeps_trailing_slash() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let c = complete("@s", dir.path()).unwrap();
        // `src` is a directory -> label is `src/` so the user can keep typing.
        assert_eq!(c.candidates, vec!["src/".to_string()]);
    }

    #[test]
    fn at_hidden_shown_when_prefix_starts_with_dot() {
        let dir = tempdir().unwrap();
        files(dir.path(), &[".env", ".gitignore", "main.rs"]);
        let c = complete("@.", dir.path()).unwrap();
        assert!(c.candidates.contains(&".env".to_string()));
        assert!(c.candidates.contains(&".gitignore".to_string()));
        assert!(!c.candidates.contains(&"main.rs".to_string()));
    }

    #[test]
    fn at_in_a_token_after_text() {
        // `edit @main` should still open the file menu at the `@`.
        let dir = tempdir().unwrap();
        files(dir.path(), &["main.rs", "other.rs"]);
        let c = complete("edit @main", dir.path()).unwrap();
        assert_eq!(c.kind, MenuKind::File);
        assert_eq!(c.replace_start, 5); // index of '@'
        assert_eq!(c.candidates, vec!["main.rs".to_string()]);
    }

    #[test]
    fn at_with_space_in_token_is_ignored() {
        // `@foo bar` — the token has an interior space, so no menu.
        let dir = tempdir().unwrap();
        files(dir.path(), &["foo"]);
        assert!(complete("@foo bar", dir.path()).is_none());
    }

    #[test]
    fn at_no_matches_is_none() {
        let dir = tempdir().unwrap();
        files(dir.path(), &["main.rs"]);
        assert!(complete("@zzz", dir.path()).is_none());
    }

    #[test]
    fn apply_file_completion_replaces_the_token() {
        let dir = tempdir().unwrap();
        files(dir.path(), &["main.rs", "other.rs"]);
        let c = complete("@m", dir.path()).unwrap();
        let out = apply("@m", &c, 0).unwrap();
        assert_eq!(out, "@main.rs");
    }

    #[test]
    fn apply_slash_completion_keeps_no_leading_at() {
        let c = complete("/c", Path::new(".")).unwrap();
        let out = apply("/c", &c, 0).unwrap();
        assert_eq!(out, "/clear");
    }

    #[test]
    fn apply_preserves_text_before_the_trigger() {
        let dir = tempdir().unwrap();
        files(dir.path(), &["main.rs"]);
        let c = complete("edit @m", dir.path()).unwrap();
        let out = apply("edit @m", &c, 0).unwrap();
        assert_eq!(out, "edit @main.rs");
    }

    #[test]
    fn apply_out_of_bounds_is_none() {
        let c = complete("/", Path::new(".")).unwrap();
        assert!(apply("/", &c, 999).is_none());
    }
}
