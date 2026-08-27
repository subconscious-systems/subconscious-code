//! The `Append` tool: bounded, durable file construction without requiring one
//! enormous `Write` tool call. Each chunk is committed atomically and can carry
//! an expected byte offset so a retried or stale append cannot duplicate text.

use crate::util::{
    atomic_write, current_read_state, params_schema, record_read, resolve_within_loose,
    stale_read_error, ReadState,
};
use async_trait::async_trait;
use rc_core::{Artifact, Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
pub struct AppendInput {
    /// Absolute or working-directory-relative path to append.
    pub file_path: String,
    /// The next bounded chunk. Split very large documents across calls.
    pub content: String,
    /// Optional current file size in bytes. When provided, the append fails if
    /// the file is not exactly this size, preventing duplicate/stale chunks.
    pub expected_size: Option<u64>,
}

#[derive(Default)]
pub struct Append;

impl Append {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for Append {
    fn name(&self) -> &str {
        "Append"
    }

    fn description(&self) -> &str {
        "Atomically append one bounded chunk to a text file. Use this instead of one huge `Write` \
call for long generated documents. The result reports `new_size`; pass it as `expected_size` on \
the next chunk to prevent duplicate or stale appends. Existing files are checked for out-of-band \
changes, missing parents are created, line endings are preserved, and every successful chunk is \
rewindable."
    }

    fn schema(&self) -> Value {
        params_schema::<AppendInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::SerialWrite
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let inp: AppendInput = serde_json::from_value(input)?;
        let canon = match resolve_within_loose(&ctx.allowed_roots, &ctx.cwd, &inp.file_path) {
            Ok(path) => path,
            Err(message) => {
                return Ok(ToolOutcome::Error {
                    message,
                    retryable: false,
                })
            }
        };

        let mut auto_read = false;
        if canon.exists() {
            match current_read_state(ctx, &canon) {
                ReadState::Unread => {
                    record_read(ctx, &canon);
                    auto_read = true;
                }
                ReadState::Stale => return Ok(stale_read_error(&canon)),
                ReadState::Current => {}
            }
        }

        let old = match std::fs::read_to_string(&canon) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Ok(ToolOutcome::Error {
                    message: format!("could not read {} before append: {error}", canon.display()),
                    retryable: false,
                })
            }
        };
        let old_size = old.as_ref().map_or(0, String::len) as u64;
        if let Some(expected) = inp.expected_size {
            if expected != old_size {
                return Ok(ToolOutcome::Error {
                    message: format!(
                        "append offset mismatch for {}: expected {expected} bytes, current size is {old_size}; read the file or use the reported new_size",
                        canon.display()
                    ),
                    retryable: false,
                });
            }
        }

        let mut created_parent = false;
        if let Some(parent) = canon.parent() {
            if !parent.exists() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    return Ok(ToolOutcome::Error {
                        message: format!("could not create parent {}: {error}", parent.display()),
                        retryable: true,
                    });
                }
                created_parent = true;
            }
        }

        let chunk = normalize_chunk(old.as_deref(), &inp.content);
        let mut combined = old.clone().unwrap_or_default();
        combined.push_str(&chunk);
        let prior = old
            .as_ref()
            .map(|contents| std::sync::Arc::<[u8]>::from(contents.as_bytes()));

        match atomic_write(&canon, &combined) {
            Ok(()) => {
                record_read(ctx, &canon);
                if let Ok(mut journal) = ctx.change_journal.lock() {
                    journal.record(canon.clone(), prior.clone());
                }
                let new_size = combined.len();
                let mut message = format!(
                    "appended {} bytes to {} (new_size: {new_size})",
                    chunk.len(),
                    canon.display()
                );
                if auto_read {
                    message.push_str("\n\n(auto-read existing file before append)");
                }
                if created_parent {
                    message.push_str("\n\n(created missing parent directories)");
                }
                Ok(ToolOutcome::Ok {
                    content: message,
                    truncated: false,
                    artifacts: vec![Artifact::FileChange {
                        path: canon,
                        before: prior,
                        after: Some(std::sync::Arc::from(combined.into_bytes())),
                    }],
                })
            }
            Err(error) => Ok(ToolOutcome::Error {
                message: format!("append failed: {error}"),
                retryable: !matches!(error.kind(), std::io::ErrorKind::NotFound),
            }),
        }
    }
}

fn normalize_chunk(old: Option<&str>, chunk: &str) -> String {
    let mut normalized = chunk.replace("\r\n", "\n");
    if old.is_some_and(|contents| contents.contains("\r\n")) {
        normalized = normalized.replace('\n', "\r\n");
    }
    if old.is_none() && !normalized.ends_with('\n') {
        normalized.push('\n');
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn builds_a_file_in_offset_checked_chunks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("long.md");
        let ctx = test_ctx(dir.path());
        let first = Append::new()
            .call(
                json!({"file_path": path, "content": "one", "expected_size": 0}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(first, ToolOutcome::Ok { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");

        let second = Append::new()
            .call(
                json!({"file_path": path, "content": "two\n", "expected_size": 4}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(second, ToolOutcome::Ok { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[tokio::test]
    async fn rejects_a_duplicate_or_stale_chunk_offset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("long.md");
        std::fs::write(&path, "existing\n").unwrap();
        let outcome = Append::new()
            .call(
                json!({"file_path": path, "content": "duplicate", "expected_size": 0}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match outcome {
            ToolOutcome::Error { message, .. } => assert!(message.contains("offset mismatch")),
            other => panic!("expected offset error, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), "existing\n");
    }
}
