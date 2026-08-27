//! The `Edit` tool (§6.3): exact-string replace with uniqueness enforcement,
//! `replace_all`, a CRLF retry, a safe unique near-match fallback, and a
//! helpful fuzzy hint on no match.

use crate::util::{
    atomic_write, current_read_state, params_schema, preserve_line_endings, record_read,
    resolve_within, stale_read_error, ReadState,
};
use async_trait::async_trait;
use rc_core::{Artifact, Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
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

const NEAR_MATCH_THRESHOLD: f32 = 0.97;

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
`old_string`. Identical old/new text is a successful no-op. On a unique exact match the edit is \
applied atomically; a unique single-line match at 97% similarity may recover minor whitespace or \
copy drift. Ambiguous or weaker matches return a hint instead of guessing."
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
            return Ok(ToolOutcome::Ok {
                content: "no changes needed: old_string and new_string are identical".to_string(),
                truncated: false,
                artifacts: Vec::new(),
            });
        }

        let canon = match resolve_within(&ctx.allowed_roots, &ctx.cwd, &inp.file_path) {
            Ok(p) => p,
            Err(msg) => {
                return Ok(ToolOutcome::Error {
                    message: msg,
                    retryable: false,
                })
            }
        };
        // Read-before-mutate (§6.3): "changed since read" is a hard error; "never
        // read" auto-reads and proceeds (the old_string match is the real gate),
        // avoiding two wasted round-trips when the model skips the `Read`.
        let mut auto_read = false;
        match current_read_state(ctx, &canon) {
            ReadState::Unread => {
                record_read(ctx, &canon);
                auto_read = true;
            }
            ReadState::Stale => return Ok(stale_read_error(&canon)),
            ReadState::Current => {}
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
        let mut near_match_note = None;
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

        // Recover a tiny amount of copy/whitespace drift, but only when the
        // requested old text is one line and exactly one file line clears a
        // deliberately high similarity threshold. Never guess between peers.
        if count == 0 {
            if let Some((candidate, line, ratio)) =
                unique_near_line_match(&old_content, &inp.old_string)
            {
                old_string = candidate;
                count = 1;
                near_match_note = Some(format!(
                    "applied unique near-match at line {line} ({:.0}% similar)",
                    ratio * 100.0
                ));
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

        if new_content == old_content {
            let mut message = "no changes needed: replacement already matches the file".to_string();
            if let Some(note) = near_match_note {
                message.push_str(&format!("\n\n({note})"));
            }
            return Ok(ToolOutcome::Ok {
                content: message,
                truncated: false,
                artifacts: Vec::new(),
            });
        }

        // M7: snapshot the prior contents for `/rewind` before mutating. Edit
        // always targets an existing file, so prior is `Some`.
        let prior: Option<std::sync::Arc<[u8]>> =
            Some(std::sync::Arc::from(old_content.as_bytes()));

        if let Err(e) = atomic_write(&canon, &new_content) {
            return Ok(ToolOutcome::Error {
                message: format!("write failed: {e}"),
                retryable: false,
            });
        }
        record_read(ctx, &canon);
        if let Ok(mut journal) = ctx.change_journal.lock() {
            journal.record(canon.clone(), prior.clone());
        }

        let mut msg = snippet_around(&new_content, &inp.new_string, 5);
        if let Some(note) = near_match_note {
            msg.push_str(&format!("\n({note})"));
        }
        if auto_read {
            msg.push_str(&format!(
                "\n\n(auto-read {} — read files before editing next time)",
                canon.display()
            ));
        }
        Ok(ToolOutcome::Ok {
            content: msg,
            truncated: false,
            artifacts: vec![Artifact::FileChange {
                path: canon,
                before: prior,
                after: Some(std::sync::Arc::from(new_content.into_bytes())),
            }],
        })
    }
}

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Return one high-confidence single-line replacement candidate. More than
/// one candidate above the threshold is ambiguous and therefore rejected.
fn unique_near_line_match(content: &str, needle: &str) -> Option<(String, usize, f32)> {
    if needle.contains(['\r', '\n']) || needle.trim().is_empty() {
        return None;
    }
    let needle = normalize_whitespace(needle);
    let mut candidates = content.lines().enumerate().filter_map(|(index, line)| {
        let ratio = similar::TextDiff::from_chars(&normalize_whitespace(line), &needle).ratio();
        (ratio >= NEAR_MATCH_THRESHOLD).then(|| (line.to_string(), index + 1, ratio))
    });
    let one = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(one)
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
    let nneedle = normalize_whitespace(needle);
    let mut best: Option<(f64, usize, String)> = None;
    for (i, line) in content.lines().enumerate() {
        let r = similar::TextDiff::from_chars(&normalize_whitespace(line), &nneedle).ratio() as f64;
        if best.as_ref().is_none_or(|(b, _, _)| r > *b) {
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
            .call(
                json!({"file_path": path.to_string_lossy().to_string()}),
                ctx,
            )
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
        match &out {
            ToolOutcome::Ok { artifacts, .. } => match artifacts.as_slice() {
                [Artifact::FileChange { before, after, .. }] => {
                    assert_eq!(before.as_deref(), Some(b"alpha\nbeta\ngamma\n".as_slice()));
                    assert_eq!(after.as_deref(), Some(b"alpha\nBETA\ngamma\n".as_slice()));
                }
                other => panic!("expected one file-change artifact, got {other:?}"),
            },
            other => panic!("expected success, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn auto_reads_without_read() {
        // An Edit on a never-read existing file now auto-reads and proceeds
        // (the old_string match is the real gate) instead of rejecting and
        // costing two round-trips. The result carries an auto-read note.
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let ctx = test_ctx(dir.path());
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "alpha", "new_string": "beta"}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("auto-read"), "{content}");
                assert!(content.contains("read files before editing"), "{content}");
            }
            o => panic!("expected auto-read-and-proceed, got {o:?}"),
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "beta\n");
        // The auto-read recorded into the shared registry.
        let canon = std::fs::canonicalize(&path).unwrap();
        assert!(ctx.read_registry.lock().unwrap().has_read(&canon));
    }

    #[tokio::test]
    async fn auto_read_still_fails_on_no_match() {
        // Auto-read is not a free pass: a wrong old_string still fails (the
        // real safety gate), with the usual fuzzy hint.
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, "alpha\n").unwrap();
        let out = Edit::new()
            .call(
                json!({"file_path": path.to_string_lossy().to_string(), "old_string": "zzz", "new_string": "beta"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => {
                assert!(message.contains("not found"), "{message}")
            }
            o => panic!("expected a no-match error, got {o:?}"),
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
            ToolOutcome::Error { message, .. } => {
                assert!(message.contains("closest match"), "{message}")
            }
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
    async fn identical_old_new_is_a_successful_noop() {
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
            ToolOutcome::Ok {
                content, artifacts, ..
            } => {
                assert!(content.contains("no changes needed"), "{content}");
                assert!(artifacts.is_empty());
            }
            o => panic!("expected a successful no-op, got {o:?}"),
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), "alpha\n");
    }

    #[tokio::test]
    async fn unique_high_confidence_near_match_is_applied() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("imports.rs");
        std::fs::write(
            &path,
            "use bench::roofline::{bench_decode, M3_PRO_ROOFLINE_GBPS};\nfn main() {}\n",
        )
        .unwrap();
        let out = Edit::new()
            .call(
                json!({
                    "file_path": path.to_string_lossy().to_string(),
                    "old_string": "use bench::roofline::{bench_decode,M3_PRO_ROOFLINE_GBPS};",
                    "new_string": "use bench::roofline::{bench_decode, bench_prefill, M3_PRO_ROOFLINE_GBPS};"
                }),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("unique near-match"), "{content}")
            }
            other => panic!("expected near-match recovery, got {other:?}"),
        }
        assert!(std::fs::read_to_string(path)
            .unwrap()
            .contains("bench_decode, bench_prefill"));
    }
}
