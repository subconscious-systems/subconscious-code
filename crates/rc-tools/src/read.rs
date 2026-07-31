//! The `Read` tool (§6.1). Output is `cat -n`, 1-based. By default the whole
//! file comes back with lines untruncated — see [`Read::with_limits`] to bound
//! it for a small-context model. Empty file → sentinel; binary (NUL in first
//! 8KB) → refused. Reads record into the shared registry (via `util`) so later
//! `Write`/`Edit` can enforce "read before mutate".

use crate::util::{params_schema, record_read, resolve_within};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const BINARY_SCAN: usize = 8192;

#[derive(Deserialize, JsonSchema)]
pub struct ReadInput {
    /// Absolute path to the file.
    pub file_path: String,
    /// 1-based line to start reading from.
    pub offset: Option<u32>,
    /// Maximum number of lines to return (default: the whole file).
    pub limit: Option<u32>,
}

pub struct Read {
    /// Lines returned when the call omits `limit`. `0` = the whole file (the
    /// default).
    default_limit: u32,
    /// Chars kept from a single line before the tail is elided. `0` = never
    /// truncate a line (the default).
    max_line_chars: usize,
    /// Built at construction so the advertised limits match the enforced ones.
    description: String,
}

impl Default for Read {
    fn default() -> Self {
        Self::new()
    }
}

impl Read {
    /// A `Read` that returns whole files with untruncated lines.
    pub fn new() -> Self {
        Self::with_limits(0, 0)
    }

    /// A `Read` bounded to `default_limit` lines and `max_line_chars` per line
    /// (`0` = unlimited in either position).
    pub fn with_limits(default_limit: u32, max_line_chars: usize) -> Self {
        Self {
            default_limit,
            max_line_chars,
            description: read_description(default_limit, max_line_chars),
        }
    }
}

/// The tool description, with the limit sentences matched to the caps in force.
fn read_description(default_limit: u32, max_line_chars: usize) -> String {
    let lines = if default_limit == 0 {
        "Returns the whole file".to_string()
    } else {
        format!("Returns up to {default_limit} lines")
    };
    let long = if max_line_chars == 0 {
        "Long lines are returned in full".to_string()
    } else {
        format!("Long lines are truncated at {max_line_chars} chars")
    };
    format!(
        "Read a file from the local filesystem. {lines} with 1-based line numbers in \
`cat -n` format (`<lineno>\\t<line>`). Use absolute paths. `offset` is 1-based; `limit` \
caps the line count. {long}. An empty file returns a sentinel; binary files are refused \
with type info. The line numbers are for your reference ONLY — when calling `Edit`, pass \
`old_string` WITHOUT the line-number prefix."
    )
}

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        params_schema::<ReadInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let inp: ReadInput = serde_json::from_value(input)?;

        let canon = match resolve_within(&ctx.allowed_roots, &ctx.cwd, &inp.file_path) {
            Ok(p) => p,
            Err(msg) => return Ok(ToolOutcome::Error { message: msg, retryable: false }),
        };

        let bytes = match std::fs::read(&canon) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("{}: {e}", canon.display()),
                    retryable: !matches!(e.kind(), std::io::ErrorKind::NotFound),
                });
            }
        };

        if bytes.is_empty() {
            record_read(ctx, &canon);
            return Ok(ToolOutcome::ok("<file is empty>".to_string()));
        }

        // Binary detection: NUL in the first 8KB (§6.1).
        let head = &bytes[..bytes.len().min(BINARY_SCAN)];
        if head.contains(&0u8) {
            return Ok(ToolOutcome::Error {
                message: format!("binary file ({} bytes) — not displayed", bytes.len()),
                retryable: false,
            });
        }

        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                return Ok(ToolOutcome::Error {
                    message: "file is not valid UTF-8 — not displayed".to_string(),
                    retryable: false,
                });
            }
        };

        record_read(ctx, &canon);

        Ok(ToolOutcome::ok(render_lines(
            &text,
            inp.offset,
            inp.limit,
            self.default_limit,
            self.max_line_chars,
        )))
    }
}

/// Render `text` as `cat -n`, honoring 1-based `offset` and `limit`.
fn render_lines(
    text: &str,
    offset: Option<u32>,
    limit: Option<u32>,
    default_limit: u32,
    max_line_chars: usize,
) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let offset = offset.unwrap_or(1).max(1) as usize;
    // An omitted `limit` means "the default", and a default of 0 means the
    // whole file — so an unset limit reads everything.
    let limit = match limit.unwrap_or(default_limit) {
        0 => total,
        n => n as usize,
    };
    let start = offset.saturating_sub(1);

    if start >= total {
        return format!("<offset {offset} is beyond the end of the file ({total} lines)>");
    }
    let end = (start + limit).min(total);

    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let lineno = start + i + 1; // 1-based
        out.push_str(&format!("{:>6}\t{}\n", lineno, truncate_line(line, max_line_chars)));
    }
    out
}

fn truncate_line(line: &str, max_line_chars: usize) -> String {
    // `0` = never truncate (the default): return the line untouched without
    // walking it at all.
    if max_line_chars == 0 {
        return line.to_string();
    }
    // Find the byte offset just past the max_line_chars-th char in one pass,
    // without collecting the whole line into a Vec<char> — a long line (the
    // exact case we truncate) would otherwise allocate a huge Vec just to
    // measure it. `char_indices` yields char-boundary offsets, so slicing at
    // `cut` is always valid UTF-8.
    let cut = line.char_indices().nth(max_line_chars).map(|(i, _)| i);
    let Some(cut) = cut else {
        // The line has ≤ max_line_chars chars — emit it unchanged.
        return line.to_string();
    };
    let elided = line[cut..].chars().count();
    let head = &line[..cut];
    format!("{head} …[+{} chars truncated]", elided)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn reads_a_file_with_line_numbers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let out = Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("     1\tfirst"), "{content}");
                assert!(content.contains("     2\tsecond"));
                assert!(content.contains("     3\tthird"));
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn empty_file_returns_sentinel() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();
        let out = Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert_eq!(content, "<file is empty>"),
            o => panic!("expected ok sentinel, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn binary_file_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bin.dat");
        std::fs::write(&path, b"abc\x00def").unwrap();
        let out = Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("binary"), "{message}"),
            o => panic!("expected binary refusal, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn path_outside_roots_is_refused() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let path = outside.path().join("secret.txt");
        std::fs::write(&path, "nope").unwrap();
        let out = Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => {
                assert!(message.contains("outside") || message.contains("roots"), "{message}")
            }
            o => panic!("expected an outside-roots refusal, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn records_into_the_read_registry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("r.txt");
        std::fs::write(&path, "x").unwrap();
        let c = test_ctx(dir.path());
        let _ = Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &c)
            .await
            .unwrap();
        let canon = std::fs::canonicalize(&path).unwrap();
        assert!(c.read_registry.lock().unwrap().has_read(&canon));
    }

    /// The historical M2 line cap. The shipped default is unlimited; these
    /// tests pin the bounded behaviour a small-context config opts into.
    const MAX_LINE_CHARS: usize = 2000;

    #[test]
    fn truncate_line_passes_short_lines_through() {
        assert_eq!(truncate_line("short", MAX_LINE_CHARS), "short");
        // Exactly at the cap is not truncated.
        let at_cap: String = "a".repeat(MAX_LINE_CHARS);
        assert_eq!(truncate_line(&at_cap, MAX_LINE_CHARS), at_cap);
    }

    #[test]
    fn truncate_line_caps_at_max_chars_and_reports_elided() {
        let long: String = "a".repeat(MAX_LINE_CHARS + 5);
        let out = truncate_line(&long, MAX_LINE_CHARS);
        assert!(out.contains("…[+5 chars truncated]"), "{out}");
        // The head is exactly MAX_LINE_CHARS chars.
        let head = out.split(" …[").next().unwrap();
        assert_eq!(head.chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn truncate_line_handles_multibyte_without_panic() {
        // Multibyte content over the cap: must still produce a valid UTF-8 head
        // and an elision count. Char-boundary slicing keeps the head valid.
        let long: String = "é".repeat(MAX_LINE_CHARS + 10);
        let out = truncate_line(&long, MAX_LINE_CHARS);
        assert!(out.contains("…[+10 chars truncated]"), "{out}");
        let head = out.split(" …[").next().unwrap();
        assert_eq!(head.chars().count(), MAX_LINE_CHARS);
    }

    /// The shipped default: no line cap, no line limit — a huge file comes
    /// back whole. This is the product thesis at the `Read` layer.
    #[test]
    fn render_lines_unlimited_returns_whole_file_untruncated() {
        let long_line = "z".repeat(MAX_LINE_CHARS * 3);
        let text: String = (0..5_000)
            .map(|i| format!("line {i} {long_line}\n"))
            .collect();
        let out = render_lines(&text, None, None, 0, 0);
        assert!(!out.contains("chars truncated"), "no line was elided");
        assert_eq!(
            out.lines().count(),
            5_000,
            "every line is returned, well past the old 2000-line default"
        );
        assert!(out.contains("  5000\t"), "the last line is present");
    }

    /// An explicit `limit` still wins over an unlimited default.
    #[test]
    fn render_lines_explicit_limit_beats_unlimited_default() {
        let text: String = (0..100).map(|i| format!("l{i}\n")).collect();
        let out = render_lines(&text, None, Some(3), 0, 0);
        assert_eq!(out.lines().count(), 3);
    }

    /// A configured default limit still applies when `limit` is omitted.
    #[test]
    fn render_lines_bounded_default_limit_applies() {
        let text: String = (0..100).map(|i| format!("l{i}\n")).collect();
        let out = render_lines(&text, None, None, 10, 0);
        assert_eq!(out.lines().count(), 10);
    }

    #[test]
    fn render_lines_truncates_a_very_long_line_in_output() {
        // End-to-end through render_lines: a single very long line is capped,
        // and the line number + truncation marker are both present.
        let long = format!("{}\n", "z".repeat(MAX_LINE_CHARS + 3));
        let out = render_lines(&long, None, None, 0, MAX_LINE_CHARS);
        assert!(out.contains("     1\t"), "{out}");
        assert!(out.contains("…[+3 chars truncated]"), "{out}");
    }
}
