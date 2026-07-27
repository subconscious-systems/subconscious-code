//! The `Grep` tool (§6.5): content search with a Rust regex over the `ignore`
//! walker. Modes: `content` (matching lines + line numbers, with -A/-B/-C
//! context), `files_with_matches` (default — just paths), `count`. Binary files
//! are skipped; output capped at 30k chars.
//!
//! Uses `regex` + `ignore` (ripgrep's walker) rather than `grep-searcher`/
//! `grep-regex` — same "no shelling out to rg, no quoting bugs" guarantee (§6.5),
//! with a simpler, well-tested matching core. The walker (gitignore-aware) is
//! the part that matters most; the matcher can swap to grep-regex later.

use crate::util::{cap_output, params_schema, resolve_within};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

const CAP: usize = 30_000;
const HEAD: usize = 10_000;
const TAIL: usize = 20_000;
const BINARY_SCAN: usize = 8192;

#[derive(Deserialize, JsonSchema)]
pub struct GrepInput {
    /// Rust regex / RE2 syntax.
    pub pattern: String,
    pub path: Option<String>,
    /// File filter glob, e.g. `*.rs` (matches at any depth).
    pub glob: Option<String>,
    /// `content` | `files_with_matches` | `count`. Default `files_with_matches`.
    pub output_mode: Option<String>,
    /// Case-insensitive (-i).
    #[serde(default)]
    pub case_insensitive: bool,
    /// Lines of context after a match (-A).
    pub after: Option<u32>,
    /// Lines of context before a match (-B).
    pub before: Option<u32>,
    /// Lines of context around a match (-C; sets both -A and -B).
    pub context: Option<u32>,
    /// `.` matches newline (multiline).
    #[serde(default)]
    pub multiline: bool,
    /// Approximate cap on the number of matches.
    pub head_limit: Option<u32>,
}

enum Mode {
    Content,
    Files,
    Count,
}

pub struct Grep;

impl Grep {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a Rust regex, respecting .gitignore; binary files are \
skipped. `output_mode`: `content` (matching lines with line numbers, plus -A/-B/-C context), \
`files_with_matches` (default — just paths, the cheapest mode), or `count`. Use \
`files_with_matches` first to find where to look, then `content` to see the lines."
    }

    fn schema(&self) -> Value {
        params_schema::<GrepInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let inp: GrepInput = serde_json::from_value(input)?;
        let mode = match inp.output_mode.as_deref() {
            Some("content") => Mode::Content,
            Some("count") => Mode::Count,
            Some("files_with_matches") | None => Mode::Files,
            Some(other) => {
                return Ok(ToolOutcome::Error {
                    message: format!("unknown output_mode: {other}"),
                    retryable: false,
                })
            }
        };

        let re = match regex::RegexBuilder::new(&inp.pattern)
            .case_insensitive(inp.case_insensitive)
            .dot_matches_new_line(inp.multiline)
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("invalid regex: {e}"),
                    retryable: false,
                })
            }
        };

        let root = match inp.path.as_deref() {
            Some(p) => match resolve_within(&ctx.allowed_roots, &ctx.cwd, p) {
                Ok(c) => c,
                Err(msg) => return Ok(ToolOutcome::Error { message: msg, retryable: false }),
            },
            None => std::fs::canonicalize(&ctx.cwd).unwrap_or_else(|_| ctx.cwd.clone()),
        };

        let glob_matcher = match inp.glob.as_deref() {
            Some(g) => match globset::GlobBuilder::new(g).literal_separator(false).build() {
                Ok(gb) => Some(gb.compile_matcher()),
                Err(e) => {
                    return Ok(ToolOutcome::Error {
                        message: format!("invalid glob: {e}"),
                        retryable: false,
                    })
                }
            },
            None => None,
        };

        let before = inp.before.or(inp.context).unwrap_or(0) as usize;
        let after = inp.after.or(inp.context).unwrap_or(0) as usize;
        let head_limit = inp.head_limit.map(|h| h as u64);

        let mut out = String::new();
        let mut total: u64 = 0;

        for entry in ignore::WalkBuilder::new(&root).build() {
            let Ok(entry) = entry else { continue };
            let Some(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&root).unwrap_or(path);
            if let Some(gm) = &glob_matcher {
                let base = rel.file_name().map(Path::new);
                let matched = gm.is_match(rel) || base.map_or(false, |b| gm.is_match(b));
                if !matched {
                    continue;
                }
            }

            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            // Binary skip: NUL in the first 8KB (§6.5).
            if bytes.iter().take(BINARY_SCAN.min(bytes.len())).any(|&b| b == 0) {
                continue;
            }
            let text = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };

            match mode {
                Mode::Files => {
                    if re.is_match(text) {
                        out.push_str(&format!("{}\n", path.to_string_lossy()));
                        total += 1;
                    }
                }
                Mode::Count => {
                    let c = re.find_iter(text).count() as u64;
                    if c > 0 {
                        out.push_str(&format!("{}:{}\n", path.to_string_lossy(), c));
                        total += c;
                    }
                }
                Mode::Content => {
                    let lines: Vec<&str> = text.lines().collect();
                    let mut to_print: BTreeSet<usize> = BTreeSet::new();
                    let mut file_hits: u64 = 0;
                    for (i, line) in lines.iter().enumerate() {
                        if re.is_match(line) {
                            file_hits += 1;
                            let lo = i.saturating_sub(before);
                            let hi = (i + after).min(lines.len().saturating_sub(1));
                            for j in lo..=hi {
                                to_print.insert(j);
                            }
                        }
                    }
                    if file_hits > 0 {
                        let mut prev: Option<usize> = None;
                        for &i in &to_print {
                            if let Some(p) = prev {
                                if p + 1 != i {
                                    out.push_str("--\n");
                                }
                            }
                            out.push_str(&format!("{}:{}:{}\n", path.to_string_lossy(), i + 1, lines[i]));
                            prev = Some(i);
                        }
                        out.push('\n');
                        total += file_hits;
                    }
                }
            }

            if let Some(hl) = head_limit {
                if total >= hl {
                    break;
                }
            }
            if out.len() > CAP + 4096 {
                break;
            }
        }

        let (truncated, content) = cap_output(&out, CAP, HEAD, TAIL);
        Ok(ToolOutcome::Ok { content, truncated, artifacts: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use tempfile::tempdir;

    fn write(dir: &std::path::Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[tokio::test]
    async fn content_mode_shows_lines_and_numbers() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.txt", "foo\nbar\nfoo baz\n");
        let out = Grep::new()
            .call(
                json!({"pattern": "foo", "path": dir.path().to_string_lossy().to_string(), "output_mode": "content"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains(":1:foo"), "{content}");
                assert!(content.contains(":3:foo baz"), "{content}");
                assert!(!content.contains(":2:bar"), "{content}");
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn files_with_matches_returns_paths() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.txt", "foo\n");
        write(dir.path(), "b.txt", "bar\n");
        let out = Grep::new()
            .call(
                json!({"pattern": "foo", "path": dir.path().to_string_lossy().to_string()}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("a.txt"), "{content}");
                assert!(!content.contains("b.txt"), "{content}");
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn count_mode_counts_matches() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.txt", "foo\nfoo\nfoo\n");
        let out = Grep::new()
            .call(
                json!({"pattern": "foo", "path": dir.path().to_string_lossy().to_string(), "output_mode": "count"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.contains(":3"), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn case_insensitive_and_glob_filter() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.rs", "FOO\n");
        write(dir.path(), "b.txt", "foo\n");
        let out = Grep::new()
            .call(
                json!({"pattern": "foo", "path": dir.path().to_string_lossy().to_string(), "output_mode": "files_with_matches", "case_insensitive": true, "glob": "*.rs"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("a.rs"), "{content}");
                assert!(!content.contains("b.txt"), "{content}");
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = tempdir().unwrap();
        write(dir.path(), "bin.dat", "foo\x00bar\nfoo\n");
        let out = Grep::new()
            .call(
                json!({"pattern": "foo", "path": dir.path().to_string_lossy().to_string(), "output_mode": "files_with_matches"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(!content.contains("bin.dat"), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
    }
}
