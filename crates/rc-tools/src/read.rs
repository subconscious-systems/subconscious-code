//! The `Read` tool (§6.1). Output is `cat -n`, 1-based, default 2000 lines,
//! lines truncated at 2000 chars. Empty file → sentinel; binary (NUL in first
//! 8KB) → refused. Reads record into the shared registry (via `util`) so later
//! `Write`/`Edit` can enforce "read before mutate".

use crate::util::{params_schema, record_read, resolve_within};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_LIMIT: u32 = 2000;
const MAX_LINE_CHARS: usize = 2000;
const BINARY_SCAN: usize = 8192;

#[derive(Deserialize, JsonSchema)]
pub struct ReadInput {
    /// Absolute path to the file.
    pub file_path: String,
    /// 1-based line to start reading from.
    pub offset: Option<u32>,
    /// Maximum number of lines to return (default 2000).
    pub limit: Option<u32>,
}

#[derive(Default)]
pub struct Read;

impl Read {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file from the local filesystem. Returns up to 2000 lines with 1-based \
line numbers in `cat -n` format (`<lineno>\\t<line>`). Use absolute paths. `offset` is \
1-based; `limit` caps the line count. Long lines are truncated at 2000 chars. An empty \
file returns a sentinel; binary files are refused with type info. The line numbers are \
for your reference ONLY — when calling `Edit`, pass `old_string` WITHOUT the line-number \
prefix."
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

        Ok(ToolOutcome::ok(render_lines(&text, inp.offset, inp.limit)))
    }
}

/// Render `text` as `cat -n`, honoring 1-based `offset` and `limit`.
fn render_lines(text: &str, offset: Option<u32>, limit: Option<u32>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let offset = offset.unwrap_or(1).max(1) as usize;
    let limit = limit.unwrap_or(DEFAULT_LIMIT) as usize;
    let start = offset.saturating_sub(1);

    if start >= total {
        return format!("<offset {offset} is beyond the end of the file ({total} lines)>");
    }
    let end = (start + limit).min(total);

    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let lineno = start + i + 1; // 1-based
        out.push_str(&format!("{:>6}\t{}\n", lineno, truncate_line(line)));
    }
    out
}

fn truncate_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let head: String = chars.iter().take(MAX_LINE_CHARS).collect();
    format!("{head} …[+{} chars truncated]", chars.len() - MAX_LINE_CHARS)
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
}
