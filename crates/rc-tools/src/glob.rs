//! The `Glob` tool (§6.4): fast path matching over the `ignore` walker (which
//! respects `.gitignore`/`.ignore`/hidden rules), sorted by mtime descending,
//! capped at 1000, absolute paths.

use crate::util::{params_schema, resolve_within};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::SystemTime;

const CAP: usize = 1000;

#[derive(Deserialize, JsonSchema)]
pub struct GlobInput {
    /// Glob pattern, e.g. `src/**/*.rs`. `*` matches one path segment, `**`
    /// matches across segments.
    pub pattern: String,
    /// Directory to search from (default: the session cwd).
    pub path: Option<String>,
}

pub struct Glob;

impl Glob {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. `src/**/*.rs`), respecting .gitignore. \
Results are absolute paths sorted by modification time (newest first), capped at 1000. \
`*` matches one path segment; `**` matches across segments."
    }

    fn schema(&self) -> Value {
        params_schema::<GlobInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let inp: GlobInput = serde_json::from_value(input)?;

        let root = match inp.path.as_deref() {
            Some(p) => match resolve_within(&ctx.allowed_roots, &ctx.cwd, p) {
                Ok(c) => c,
                Err(msg) => return Ok(ToolOutcome::Error { message: msg, retryable: false }),
            },
            None => std::fs::canonicalize(&ctx.cwd).unwrap_or_else(|_| ctx.cwd.clone()),
        };

        let matcher = match globset::Glob::new(&inp.pattern) {
            Ok(g) => g.compile_matcher(),
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("invalid glob: {e}"),
                    retryable: false,
                })
            }
        };

        let mut hits: Vec<(PathBuf, SystemTime)> = Vec::new();
        for entry in ignore::WalkBuilder::new(&root).build() {
            let Ok(entry) = entry else { continue };
            let Some(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if matcher.is_match(rel) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                hits.push((entry.path().to_path_buf(), mtime));
            }
        }

        hits.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
        let truncated = hits.len() > CAP;
        let mut content: String = hits
            .iter()
            .take(CAP)
            .map(|(p, _)| format!("{}\n", p.to_string_lossy()))
            .collect();
        if truncated {
            content.push_str(&format!("… [{} more matches omitted]\n", hits.len() - CAP));
        }
        Ok(ToolOutcome::Ok { content, truncated, artifacts: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn finds_nested_files_by_glob() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "x").unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let out = Glob::new()
            .call(
                json!({"pattern": "**/*.rs", "path": dir.path().to_string_lossy().to_string()}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("main.rs"), "{content}");
                assert!(!content.contains("README.md"), "{content}");
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn sorts_newest_first() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("old.rs"), "x").unwrap();
        thread::sleep(Duration::from_millis(60));
        std::fs::write(dir.path().join("new.rs"), "x").unwrap();
        let out = Glob::new()
            .call(
                json!({"pattern": "*.rs", "path": dir.path().to_string_lossy().to_string()}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                let new_idx = content.find("new.rs").unwrap();
                let old_idx = content.find("old.rs").unwrap();
                assert!(new_idx < old_idx, "newest should be first: {content}");
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }
}
