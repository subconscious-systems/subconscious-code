//! The `Write` tool (§6.2): full overwrite, atomic, requires a prior `Read`
//! of an existing file (the registry rule that prevents overwriting
//! hallucinated content), and a content/mtime check so an externally-changed
//! file must be re-read.

use crate::util::{
    atomic_write, params_schema, preserve_line_endings, record_read, current_read_state,
    stale_read_error, resolve_within_loose, ReadState,
};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
pub struct WriteInput {
    /// Absolute path to the file to write (full overwrite).
    pub file_path: String,
    /// The complete new contents of the file.
    pub content: String,
}

#[derive(Default)]
pub struct Write;

impl Write {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Write a file's full contents, overwriting it entirely. If the file already exists \
you MUST have read it with `Read` first; if it changed since the read, re-read it. Parent \
directories must already exist (create them with `Bash` mkdir -p if needed). The file's \
line-ending style and trailing newline are preserved. Prefer `Edit` for targeted changes; \
use `Write` only for new files or complete rewrites."
    }

    fn schema(&self) -> Value {
        params_schema::<WriteInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::SerialWrite
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let inp: WriteInput = serde_json::from_value(input)?;

        let canon = match resolve_within_loose(&ctx.allowed_roots, &ctx.cwd, &inp.file_path) {
            Ok(p) => p,
            Err(msg) => {
                return Ok(ToolOutcome::Error {
                    message: msg,
                    retryable: false,
                })
            }
        };

        let mut auto_read = false;
        if canon.exists() {
            // Existing file: "changed since read" is a hard error; "never read"
            // auto-reads and proceeds (§6.2), avoiding two wasted round-trips.
            match current_read_state(ctx, &canon) {
                ReadState::Unread => {
                    record_read(ctx, &canon);
                    auto_read = true;
                }
                ReadState::Stale => return Ok(stale_read_error(&canon)),
                ReadState::Current => {}
            }
        }
        // For a new file, a missing parent dir is already refused by
        // resolve_within_loose (with a "create it first" hint) — explicit beats
        // magic (§6.2).

        let old = std::fs::read_to_string(&canon).ok();
        let content = preserve_line_endings(old.as_deref(), &inp.content);

        // M7: snapshot the prior contents for `/rewind` before mutating.
        let prior = std::fs::read(&canon).ok();

        match atomic_write(&canon, &content) {
            Ok(()) => {
                // A write counts as having "read" the file for a follow-up Edit.
                record_read(ctx, &canon);
                if let Ok(mut journal) = ctx.change_journal.lock() {
                    journal.record(canon.clone(), prior);
                }
                let mut msg = format!("wrote {} ({} bytes)", canon.display(), content.len());
                if auto_read {
                    msg.push_str(&format!(
                        "\n\n(auto-read {} — read files before editing next time)",
                        canon.display()
                    ));
                }
                Ok(ToolOutcome::ok(msg))
            }
            Err(e) => Ok(ToolOutcome::Error {
                message: format!("write failed: {e}"),
                retryable: !matches!(e.kind(), std::io::ErrorKind::NotFound),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn creates_a_new_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let out = Write::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "content": "hello\n"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolOutcome::Ok { .. }), "{out:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn auto_reads_and_overwrites_without_prior_read() {
        // A Write to a never-read existing file now auto-reads and proceeds
        // (full content is explicit intent) instead of rejecting and costing
        // two round-trips. The result carries an auto-read note.
        let dir = tempdir().unwrap();
        let path = dir.path().join("exists.txt");
        std::fs::write(&path, "old").unwrap();
        let ctx = test_ctx(dir.path());
        let out = Write::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "content": "new"}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("auto-read"), "{content}");
            }
            o => panic!("expected auto-read-and-proceed, got {o:?}"),
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let canon = std::fs::canonicalize(&path).unwrap();
        assert!(ctx.read_registry.lock().unwrap().has_read(&canon));
    }

    #[tokio::test]
    async fn overwrites_after_a_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rw.txt");
        std::fs::write(&path, "old\n").unwrap();
        let ctx = test_ctx(dir.path());
        // Read first (registers in the registry).
        let _ = crate::Read::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string()}),
                &ctx,
            )
            .await;
        let out = Write::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "content": "new\n"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolOutcome::Ok { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn refuses_when_file_changed_since_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ch.txt");
        std::fs::write(&path, "v1\n").unwrap();
        let ctx = test_ctx(dir.path());
        let _ = crate::Read::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string()}),
                &ctx,
            )
            .await;
        // Externally change the file (content differs from the recorded hash).
        std::fs::write(&path, "v2 — changed out of band\n").unwrap();
        let out = Write::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "content": "v3\n"}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("changed"), "{message}"),
            o => panic!("expected a changed-since-read refusal, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn refuses_when_parent_dir_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/deep.txt");
        let out = Write::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "content": "x"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("parent"), "{message}"),
            o => panic!("expected a missing-parent refusal, got {o:?}"),
        }
    }
}
