//! The `ReadMany` tool: bounded, multi-file context collection in one model round.
//!
//! A model can already emit several parallel `Read` calls, but that is a
//! behavioral convention rather than an invariant. `ReadMany` makes the common
//! inventory flow explicit: discover paths with `List`, read the relevant text
//! files once, then answer.

use crate::read::Read;
use crate::util::params_schema;
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

const MAX_FILES: usize = 64;
const DEFAULT_OUTPUT_CAP: usize = 256 * 1024;

#[derive(Deserialize, JsonSchema)]
pub struct ReadManyInput {
    /// Absolute or working-directory-relative paths to text files. Put every
    /// independent file needed for the answer in this one array.
    pub file_paths: Vec<String>,
    /// Maximum lines returned from each file. Omit to use the configured Read
    /// default (normally the whole file).
    pub limit: Option<u32>,
}

/// A bounded batch of ordinary `Read` operations.
pub struct ReadMany {
    default_limit: u32,
    max_line_chars: usize,
    output_cap: usize,
    description: String,
}

impl Default for ReadMany {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadMany {
    pub fn new() -> Self {
        Self::with_limits(0, 0, 0)
    }

    /// Build a batch reader using the same per-file limits as `Read`.
    ///
    /// `configured_output_cap` is also honored when it is stricter than the
    /// built-in 256 KiB batch ceiling. Passing zero keeps the built-in ceiling.
    pub fn with_limits(
        default_limit: u32,
        max_line_chars: usize,
        configured_output_cap: usize,
    ) -> Self {
        let output_cap = match configured_output_cap {
            0 => DEFAULT_OUTPUT_CAP,
            cap => cap.min(DEFAULT_OUTPUT_CAP),
        };
        Self {
            default_limit,
            max_line_chars,
            output_cap,
            description: format!(
                "Read up to {MAX_FILES} independent text files in one bounded, read-only call. \
Use this once after `List` when a folder inventory needs file descriptions; put every relevant \
path in `file_paths` instead of issuing sequential `Read` calls. Paths may be absolute or relative \
to the working directory. Results use labeled `cat -n` sections, duplicate paths are skipped, and \
the combined output is capped at {output_cap} bytes."
            ),
        }
    }
}

#[async_trait]
impl Tool for ReadMany {
    fn name(&self) -> &str {
        "ReadMany"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        params_schema::<ReadManyInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let input: ReadManyInput = serde_json::from_value(input)?;
        if input.file_paths.is_empty() {
            return Ok(ToolOutcome::Error {
                message: "file_paths must contain at least one path".to_string(),
                retryable: true,
            });
        }

        let mut seen = HashSet::new();
        let mut paths = Vec::new();
        for path in input.file_paths {
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
        let omitted = paths.len().saturating_sub(MAX_FILES);
        paths.truncate(MAX_FILES);

        // Divide the output budget across files so one large source file cannot
        // crowd every later file out of the batch. Headers and the final
        // omission marker get a separate small allowance, and a final hard cap
        // below keeps the advertised bound exact.
        let per_file_cap = self.output_cap.saturating_sub(1024) / paths.len().max(1);
        let reader = Read::with_limits(self.default_limit, self.max_line_chars);
        let mut content = String::new();
        let mut truncated = omitted > 0;

        for path in &paths {
            if ctx.cancel.is_cancelled() {
                return Ok(ToolOutcome::Interrupted);
            }
            content.push_str(&format!("===== {path} =====\n"));
            let outcome = reader
                .call(
                    json!({
                        "file_path": path,
                        "limit": input.limit,
                    }),
                    ctx,
                )
                .await?;
            let section = match outcome {
                ToolOutcome::Ok {
                    content,
                    truncated: section_truncated,
                    ..
                } => {
                    truncated |= section_truncated;
                    content
                }
                ToolOutcome::Error { message, .. } => format!("<error: {message}>"),
                ToolOutcome::Denied { reason } => format!("<denied: {reason}>"),
                ToolOutcome::Interrupted => return Ok(ToolOutcome::Interrupted),
            };
            let (section, section_truncated) = truncate_utf8_bytes(&section, per_file_cap);
            truncated |= section_truncated;
            content.push_str(&section);
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push('\n');
        }

        if omitted > 0 {
            content.push_str(&format!("[… {omitted} additional paths omitted]\n"));
        }
        let (content, final_truncated) = truncate_utf8_bytes(&content, self.output_cap);
        truncated |= final_truncated;

        Ok(ToolOutcome::Ok {
            content,
            truncated,
            artifacts: Vec::new(),
        })
    }
}

/// Keep a UTF-8-safe prefix within an exact byte ceiling and leave a sentinel
/// whenever enough room exists. Batch reads favor file starts because imports,
/// declarations, and module docs carry most inventory-description value.
fn truncate_utf8_bytes(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_string(), false);
    }
    const SENTINEL: &str = "\n[… section truncated]\n";
    let mut end = cap.saturating_sub(SENTINEL.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = text[..end].to_string();
    if output.len() + SENTINEL.len() <= cap {
        output.push_str(SENTINEL);
    }
    (output, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use tempfile::tempdir;

    fn ok(outcome: ToolOutcome) -> (String, bool) {
        match outcome {
            ToolOutcome::Ok {
                content, truncated, ..
            } => (content, truncated),
            other => panic!("expected batched read, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reads_and_registers_multiple_files_in_one_call() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "pub fn b() {}\n").unwrap();
        let ctx = test_ctx(root.path());

        let outcome = ReadMany::new()
            .call(json!({"file_paths": ["a.rs", "b.rs", "a.rs"]}), &ctx)
            .await
            .unwrap();
        let (content, truncated) = ok(outcome);
        assert!(!truncated, "{content}");
        assert_eq!(content.matches("===== a.rs =====").count(), 1);
        assert!(content.contains("     1\tpub fn a() {}"), "{content}");
        assert!(content.contains("     1\tpub fn b() {}"), "{content}");

        for file in ["a.rs", "b.rs"] {
            let path = std::fs::canonicalize(root.path().join(file)).unwrap();
            assert!(ctx.read_registry.lock().unwrap().has_read(&path));
        }
    }

    #[tokio::test]
    async fn shares_a_hard_output_budget_across_files() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "a".repeat(500)).unwrap();
        std::fs::write(root.path().join("b.txt"), "b".repeat(500)).unwrap();

        let outcome = ReadMany::with_limits(0, 0, 160)
            .call(
                json!({"file_paths": ["a.txt", "b.txt"]}),
                &test_ctx(root.path()),
            )
            .await
            .unwrap();
        let (content, truncated) = ok(outcome);
        assert!(truncated, "{content}");
        assert!(content.len() <= 160, "{} bytes", content.len());
    }

    #[test]
    fn byte_cap_never_splits_utf8() {
        let (content, truncated) = truncate_utf8_bytes(&"é".repeat(100), 31);
        assert!(truncated);
        assert!(content.len() <= 31);
        assert!(std::str::from_utf8(content.as_bytes()).is_ok());
    }
}
