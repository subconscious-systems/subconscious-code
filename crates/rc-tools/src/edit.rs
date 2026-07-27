//! The `Edit` tool (§6.3): exact-string replace with uniqueness enforcement,
//! `replace_all`, a CRLF retry, and a helpful fuzzy hint on no match.

use crate::util::{
    atomic_write, params_schema, preserve_line_endings, record_read, require_current_read,
    resolve_within,
};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
pub struct EditInput {
    pub file_path: String,
    /// Must match byte-for-byte (including indentation/newlines) and occur
    /// exactly once unless `replace_all` is true.
    pub old_string: String,
    pub new_string: String,
    /// Replace every occurrence. Default false.
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Default)]
pub struct Edit;

impl Edit {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replace an exact substring in a file. `old_string` must match byte-for-byte \
(including indentation and newlines) and occur exactly once unless `replace_all=true`. \
Line numbers from `Read` are NOT part of the file content — don't include them in \
`old_string`. On a unique match the edit is applied atomically; on no match you get a \
hint, on multiple matches you must make `old_string` unique or set `replace_all`."
    }

    fn schema(&self) -> Value {
        params_schema::<EditInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::SerialWrite
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let inp: EditInput = serde_json::from_value(input)?;
        if inp.old_string == inp.new_string {
            return Ok(ToolOutcome::Error {
                message: "old_string and new_string are identical".to_string(),
                retryable: false,
            });
        }

        let canon = match resolve_within(&ctx.allowed_roots, &ctx.cwd, &inp.file_path) {
            Ok(p) => p,
            Err(msg) => return Ok(ToolOutcome::Error { message: msg, retryable: false }),
        };
        if let Some(err) = require_current_read(ctx, &canon) {
            return Ok(err);
        }

        let old_content = match std::fs::read_to_string(&canon) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("{}: {e}", canon.display()),
                    retryable: !matches!(e.kind(), std::io::ErrorKind::NotFound),
                });
            }
        };

        let mut old_string = inp.old_string.clone();
        let mut count = old_content.matches(&old_string).count();
        // CRLF retry (§6.3 / gotcha #13): models never emit CRLF; if the file is
        // CRLF and old_string is LF, retry the match with \n -> \r\n.
        if count == 0 && old_content.contains("\r\n") {
            let crlf = old_string.replace('\n', "\r\n");
            let c = old_content.matches(&crlf).count();
            if c > 0 {
                old_string = crlf;
                count = c;
            }
        }

        if count == 0 {
            return Ok(ToolOutcome::Error {
                message: format!(
                    "old_string not found in {}. {}",
                    canon.display(),
                    fuzzy_hint(&old_content, &inp.old_string)
                ),
                retryable: false,
            });
        }
        if count > 1 && !inp.replace_all {
            let lines = occurrence_lines(&old_content, &old_string);
            return Ok(ToolOutcome::Error {
                message: format!(
                    "old_string appears {count} times (lines: {}) — make it unique or set replace_all=true",
                    lines.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
                ),
                retryable: false,
            });
        }

        let new_content = if inp.replace_all {
            old_content.replace(&old_string, &inp.new_string)
        } else {
            old_content.replacen(&old_string, &inp.new_string, 1)
        };
        let new_content = preserve_line_endings(Some(&old_content), &new_content);

        if let Err(e) = atomic_write(&canon, &new_content) {
            return Ok(ToolOutcome::Error {
                message: format!("write failed: {e}"),
                retryable: false,
            });
        }
        record_read(ctx, &canon);

        Ok(ToolOutcome::ok(snippet_around(&new_content, &inp.new_string, 5)))
    }
}

/// Line numbers (1-based) of each occurrence of `needle` in `content`.
fn occurrence_lines(content: &str, needle: &str) -> Vec<usize> {
    content
        .match_indices(needle)
        .map(|(off, _)| content[..off].matches('\n').count() + 1)
        .collect()
}

/// A best-effort "did you mean" hint (§6.3): find the line most similar to the
/// needle (whitespace-normalized) and, if close, show it.
fn fuzzy_hint(content: &str, needle: &str) -> String {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let nneedle = norm(needle);
    let mut best: Option<(f64, usize, String)> = None;
    for (i, line) in content.lines().enumerate() {
        let r = similar::TextDiff::from_chars(&norm(line), &nneedle).ratio() as f64;
        if best.as_ref().map_or(true, |(b, _, _)| r > *b) {
            best = Some((r, i + 1, line.to_string()));
        }
    }
    if let Some((r, lineno, text)) = best {
        if r >= 0.6 {
            return format!(
                "closest match is line {lineno} (similarity {:.0}%): {text:?} — check exact whitespace/indentation",
                r * 100.0
            );
        }
    }
    "no close match found — re-read the file to see the exact contents".to_string()
}

/// A `cat -n` snippet of ±`ctx` lines around the first occurrence of `needle`
/// (trying LF then CRLF), so the model can verify the edit landed.
fn snippet_around(content: &str, needle: &str, ctx: usize) -> String {
    let off = content
        .find(needle)
        .or_else(|| content.find(&needle.replace('\n', "\r\n")));
    let lines: Vec<&str> = content.lines().collect();
    let target = match off {
        Some(o) => content[..o].matches('\n').count(),
        None => 0,
    };
    let start = target.saturating_sub(ctx);
    let end = (target + ctx + 1).min(lines.len());
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>6}\t{}\n", start + i + 1, line));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use crate::Read;
    use serde_json::json;
    use tempfile::tempdir;

    async fn read_first(ctx: &rc_core::ToolCtx, path: &std::path::Path) {
        Read::new()
            .call(json!({"file_path": path.to_string_lossy().to_string()}), ctx)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn edits_a_unique_match() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
        let ctx = test_ctx(dir.path());
        read_first(&ctx, &path).await;
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "beta", "new_string": "BETA"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolOutcome::Ok { .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nBETA\ngamma\n");
    }

    #[tokio::test]
    async fn refuses_without_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "alpha", "new_string": "beta"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("Read"), "{message}"),
            o => panic!("expected read-first refusal, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_gives_a_hint() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let ctx = test_ctx(dir.path());
        read_first(&ctx, &path).await;
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "bata", "new_string": "beta"}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("closest match"), "{message}"),
            o => panic!("expected a fuzzy hint, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_matches_without_replace_all_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "x\nx\n").unwrap();
        let ctx = test_ctx(dir.path());
        read_first(&ctx, &path).await;
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "x", "new_string": "y"}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("2 times"), "{message}"),
            o => panic!("expected a multiple-match error, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "x\nx\nx\n").unwrap();
        let ctx = test_ctx(dir.path());
        read_first(&ctx, &path).await;
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "x", "new_string": "y", "replace_all": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolOutcome::Ok { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\ny\ny\n");
    }

    #[tokio::test]
    async fn identical_old_new_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let ctx = test_ctx(dir.path());
        read_first(&ctx, &path).await;
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "alpha", "new_string": "alpha"}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("identical"), "{message}"),
            o => panic!("expected identical error, got {o:?}"),
        }
    }
}
