//! The `List` tool: a table-ready directory inventory in one read-only call.
//!
//! `Glob` is optimized for locating matching files. `List` instead returns
//! files *and* directories with type and byte size, which lets the model answer
//! “what is in this folder?” without a sequence of `ls`/`find` model rounds.

use crate::util::{params_schema, resolve_within};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Deserialize, JsonSchema)]
pub struct ListInput {
    /// Directory to inventory (default: the session working directory).
    pub path: Option<String>,
    /// Include every descendant. Use this for “everything in this folder” or
    /// repository-layout requests; false lists only direct children.
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug)]
struct Entry {
    path: String,
    kind: &'static str,
    size: Option<u64>,
}

pub struct List {
    /// Max entries returned. `0` = every entry.
    entry_cap: usize,
    /// Approximate maximum bytes returned before the omission sentinel.
    /// `0` = unlimited.
    output_cap: usize,
    description: String,
}

/// `List` has a deliberately smaller budget than arbitrary tool output. Path
/// inventories tokenize poorly, and one recursive parent-directory walk can
/// otherwise add more than 100k prompt tokens before the model has identified
/// the repository it actually needs.
pub const DEFAULT_ENTRY_CAP: usize = 2_000;
pub const DEFAULT_OUTPUT_CAP: usize = 64 * 1024;

impl Default for List {
    fn default() -> Self {
        Self::new()
    }
}

impl List {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_ENTRY_CAP, DEFAULT_OUTPUT_CAP)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self::with_limits(cap, DEFAULT_OUTPUT_CAP)
    }

    pub fn with_limits(entry_cap: usize, output_cap: usize) -> Self {
        let entry_limit = if entry_cap == 0 {
            "no separate entry limit".to_string()
        } else {
            format!("at most {entry_cap} entries")
        };
        let byte_limit = if output_cap == 0 {
            "no byte limit".to_string()
        } else {
            format!("approximately {output_cap} output bytes")
        };
        Self {
            entry_cap,
            output_cap,
            description: format!(
                "Inventory a directory in one read-only call, returning relative path, type, and \
byte size as tab-separated columns. Set `recursive: true` for ‘everything in this folder’ or \
repository-layout requests instead of making repeated `ls`, `find`, or `Glob` calls. Empty \
directories are included; hidden and ignored paths are skipped. Results are sorted by path and \
bounded to {entry_limit} and {byte_limit}. An omission row tells you when to narrow `path` or use \
`Glob`. If the answer also needs descriptions from file contents, follow this with one `ReadMany` \
call containing every relevant text file."
            ),
        }
    }
}

#[async_trait]
impl Tool for List {
    fn name(&self) -> &str {
        "List"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        params_schema::<ListInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let input: ListInput = serde_json::from_value(input)?;
        let root = match input.path.as_deref() {
            Some(path) => match resolve_within(&ctx.allowed_roots, &ctx.cwd, path) {
                Ok(path) => path,
                Err(message) => {
                    return Ok(ToolOutcome::Error {
                        message,
                        retryable: false,
                    })
                }
            },
            None => std::fs::canonicalize(&ctx.cwd).unwrap_or_else(|_| ctx.cwd.clone()),
        };
        if !root.is_dir() {
            return Ok(ToolOutcome::Error {
                message: format!("{} is not a directory", root.display()),
                retryable: false,
            });
        }

        let mut builder = ignore::WalkBuilder::new(&root);
        if !input.recursive {
            builder.max_depth(Some(1));
        }

        let mut entries = Vec::new();
        for result in builder.build() {
            let Ok(entry) = result else { continue };
            if entry.depth() == 0 {
                continue;
            }
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            let relative: PathBuf = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_path_buf();
            let (kind, size, suffix) = if file_type.is_dir() {
                ("dir", None, "/")
            } else if file_type.is_file() {
                (
                    "file",
                    entry.metadata().ok().map(|metadata| metadata.len()),
                    "",
                )
            } else if file_type.is_symlink() {
                ("symlink", None, "")
            } else {
                ("other", None, "")
            };
            entries.push(Entry {
                path: format!("{}{suffix}", relative.to_string_lossy()),
                kind,
                size,
            });
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));

        let mut content = String::from("path\ttype\tsize_bytes\n");
        let max_entries = if self.entry_cap == 0 {
            entries.len()
        } else {
            self.entry_cap.min(entries.len())
        };
        let mut kept = 0;
        for entry in entries.iter().take(max_entries) {
            let size = entry
                .size
                .map(|size| size.to_string())
                .unwrap_or_else(|| "—".to_string());
            let row = format!("{}\t{}\t{size}\n", entry.path, entry.kind);
            if self.output_cap != 0 && content.len() + row.len() > self.output_cap {
                break;
            }
            content.push_str(&row);
            kept += 1;
        }
        let truncated = kept < entries.len();
        if truncated {
            content.push_str(&format!(
                "…\tomitted\t{} more entries; narrow path or use Glob\n",
                entries.len() - kept
            ));
        }
        Ok(ToolOutcome::Ok {
            content,
            truncated,
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use tempfile::tempdir;

    fn content(outcome: ToolOutcome) -> String {
        match outcome {
            ToolOutcome::Ok { content, .. } => content,
            other => panic!("expected inventory, got {other:?}"),
        }
    }

    #[test]
    fn default_inventory_has_an_independent_tight_budget() {
        let list = List::new();
        assert_eq!(list.entry_cap, DEFAULT_ENTRY_CAP);
        assert_eq!(list.output_cap, DEFAULT_OUTPUT_CAP);
        assert!(list.output_cap < 256 * 1024);
    }

    #[tokio::test]
    async fn one_recursive_call_inventories_files_directories_and_sizes() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/empty")).unwrap();
        std::fs::write(root.path().join("README.md"), "abc").unwrap();
        std::fs::write(root.path().join("src/main.rs"), "hello").unwrap();

        let direct = List::new()
            .call(json!({"recursive": false}), &test_ctx(root.path()))
            .await
            .unwrap();
        let direct = content(direct);
        assert!(direct.contains("README.md\tfile\t3"), "{direct}");
        assert!(direct.contains("src/\tdir\t—"), "{direct}");
        assert!(!direct.contains("src/main.rs"), "{direct}");

        let recursive = List::new()
            .call(json!({"recursive": true}), &test_ctx(root.path()))
            .await
            .unwrap();
        let recursive = content(recursive);
        assert!(recursive.starts_with("path\ttype\tsize_bytes\n"));
        assert!(recursive.contains("src/empty/\tdir\t—"), "{recursive}");
        assert!(recursive.contains("src/main.rs\tfile\t5"), "{recursive}");
    }

    #[tokio::test]
    async fn configured_cap_is_reported_instead_of_silently_dropping_entries() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("a"), "a").unwrap();
        std::fs::write(root.path().join("b"), "b").unwrap();

        let outcome = List::with_cap(1)
            .call(json!({"recursive": true}), &test_ctx(root.path()))
            .await
            .unwrap();
        match outcome {
            ToolOutcome::Ok {
                content, truncated, ..
            } => {
                assert!(truncated);
                assert!(content.contains("1 more entries"), "{content}");
            }
            other => panic!("expected inventory, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn byte_cap_bounds_a_large_recursive_inventory_at_source() {
        const CAP: usize = 180;
        let root = tempdir().unwrap();
        for i in 0..40 {
            std::fs::write(
                root.path().join(format!("long-inventory-entry-{i:03}.txt")),
                "x",
            )
            .unwrap();
        }

        let outcome = List::with_limits(0, CAP)
            .call(json!({"recursive": true}), &test_ctx(root.path()))
            .await
            .unwrap();
        match outcome {
            ToolOutcome::Ok {
                content, truncated, ..
            } => {
                assert!(truncated);
                assert!(content.contains("narrow path or use Glob"), "{content}");
                assert!(
                    content.len() <= CAP + 80,
                    "{} bytes: {content}",
                    content.len()
                );
            }
            other => panic!("expected inventory, got {other:?}"),
        }
    }
}
