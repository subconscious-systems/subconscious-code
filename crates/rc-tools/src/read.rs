//! The `Read` tool (§6.1) + read-registry recording.
//!
//! Output is `cat -n` style (`%6d\t<line>`), 1-based, default 2000 lines, lines
//! truncated at 2000 chars. Empty file → sentinel; binary (NUL in first 8KB) →
//! refused with type info. Reads record into the shared registry so later
//! `Write`/`Edit` can enforce "read before mutate".

use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DEFAULT_LIMIT: u32 = 2000;
const MAX_LINE_CHARS: usize = 2000;
const BINARY_SCAN: usize = 8192;

/// `Read` input. Schemas are generated via `schemars` and serialized
/// canonically on the wire (§4.6); key order is stable regardless of derive
/// output order.
#[derive(Deserialize, JsonSchema)]
pub struct ReadInput {
    /// Absolute path to the file.
    pub file_path: String,
    /// 1-based line to start reading from.
    pub offset: Option<u32>,
    /// Maximum number of lines to return (default 2000).
    pub limit: Option<u32>,
}

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

        let rendered = render_lines(&text, inp.offset, inp.limit);
        Ok(ToolOutcome::ok(rendered))
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

/// Record a read in the shared registry (path → (mtime, blake3)) for the
/// "read before mutate" rule (M2, §6.2/§6.3).
fn record_read(ctx: &ToolCtx, canon: &Path) {
    let mtime = std::fs::metadata(canon).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    let bytes = std::fs::read(canon).unwrap_or_default();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    if let Ok(mut reg) = ctx.read_registry.lock() {
        reg.record(canon.to_path_buf(), mtime, hash);
    }
}

/// Basic path-scope check (M3 hardens this: deny-read globs, `openat2`
/// RESOLVE_BENEATH, TOCTOU-safe canonicalize). Resolve symlinks physically
/// and require the result to live under an allowed root.
fn resolve_within(roots: &[PathBuf], cwd: &Path, candidate: &str) -> Result<PathBuf, String> {
    let p = Path::new(candidate);
    let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    let canon = std::fs::canonicalize(&abs)
        .map_err(|e| format!("{}: {e}", abs.display()))?;
    for root in roots {
        let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if canon.starts_with(&root_canon) {
            return Ok(canon);
        }
    }
    Err(format!("path outside allowed roots: {}", canon.display()))
}

/// Generate the JSON Schema `parameters` for a `JsonSchema` type, stripping
/// `$schema`/`title` to keep the on-wire object clean. Canonical serialization
/// (§4.6) makes the byte form stable regardless of derive key order.
fn params_schema<T: JsonSchema>() -> Value {
    let root = schemars::schema_for!(T);
    let mut v = serde_json::to_value(&root).expect("schema is serializable");
    if let Value::Object(map) = &mut v {
        map.remove("$schema");
        map.remove("title");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::state::ReadRegistry;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            allowed_roots: vec![dir.to_path_buf()],
            cancel: CancellationToken::new(),
            read_registry: Arc::new(Mutex::new(ReadRegistry::new())),
        }
    }

    #[tokio::test]
    async fn reads_a_file_with_line_numbers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let out = Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &ctx(dir.path()))
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
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &ctx(dir.path()))
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
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &ctx(dir.path()))
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
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &ctx(dir.path()))
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
        let c = ctx(dir.path());
        let _ = Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), &c)
            .await
            .unwrap();
        let canon = std::fs::canonicalize(&path).unwrap();
        assert!(c.read_registry.lock().unwrap().has_read(&canon));
    }
}
