//! Batched content searches. `GrepMany` removes a model round trip when several
//! independent patterns or roots are already known, while retaining `Grep`'s
//! path checks, gitignore handling, binary filtering, and output modes.

use crate::grep::Grep;
use crate::read_many::truncate_utf8_bytes;
use crate::util::params_schema;
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_QUERIES: usize = 32;
const DEFAULT_OUTPUT_CAP: usize = 256 * 1024;

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct GrepQuery {
    /// Rust regex / RE2 syntax.
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    /// `content` | `files_with_matches` | `count`.
    pub output_mode: Option<String>,
    #[serde(default)]
    pub case_insensitive: bool,
    pub after: Option<u32>,
    pub before: Option<u32>,
    pub context: Option<u32>,
    #[serde(default)]
    pub multiline: bool,
    pub head_limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GrepManyInput {
    /// Independent searches to execute in this single model round trip.
    pub queries: Vec<GrepQuery>,
}

pub struct GrepMany {
    cap: usize,
}

impl Default for GrepMany {
    fn default() -> Self {
        Self::new()
    }
}

impl GrepMany {
    pub fn new() -> Self {
        Self::with_cap(0)
    }

    pub fn with_cap(configured_cap: usize) -> Self {
        Self {
            cap: if configured_cap == 0 {
                DEFAULT_OUTPUT_CAP
            } else {
                configured_cap.min(DEFAULT_OUTPUT_CAP)
            },
        }
    }
}

#[async_trait]
impl Tool for GrepMany {
    fn name(&self) -> &str {
        "GrepMany"
    }

    fn description(&self) -> &str {
        "Run up to 32 independent Grep searches in one model round trip. Use this whenever multiple \
patterns, roots, or file globs are already known instead of issuing sequential Grep calls. Results \
are labeled by query and share one bounded byte budget, including labels and truncation markers."
    }

    fn schema(&self) -> Value {
        params_schema::<GrepManyInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let input: GrepManyInput = serde_json::from_value(input)?;
        if input.queries.is_empty() {
            return Ok(ToolOutcome::Error {
                message: "queries must contain at least one search".into(),
                retryable: true,
            });
        }

        let omitted = input.queries.len().saturating_sub(MAX_QUERIES);
        let queries: Vec<_> = input.queries.into_iter().take(MAX_QUERIES).collect();
        let omission_note = if omitted > 0 {
            format!("[omitted {omitted} queries beyond limit {MAX_QUERIES}]\n")
        } else {
            String::new()
        };
        let labels: Vec<_> = queries
            .iter()
            .enumerate()
            .map(|(index, query)| format!("===== query {}: {} =====\n", index + 1, query.pattern))
            .collect();
        let overhead = labels.iter().fold(omission_note.len(), |total, label| {
            total.saturating_add(label.len()).saturating_add(1)
        });
        let per_query_cap = self.cap.saturating_sub(overhead) / queries.len();
        // Zero means unlimited to Grep. Even when labels consume the whole
        // batch budget, keep each underlying search bounded.
        let grep = Grep::with_cap(per_query_cap.max(1));
        let mut output = String::new();
        let mut truncated = omitted > 0;

        for (query, label) in queries.into_iter().zip(&labels) {
            if ctx.cancel.is_cancelled() {
                return Ok(ToolOutcome::Interrupted);
            }
            output.push_str(label);
            let section = match grep.call(json!(query), ctx).await? {
                ToolOutcome::Ok {
                    content,
                    truncated: query_truncated,
                    ..
                } => {
                    truncated |= query_truncated;
                    content
                }
                ToolOutcome::Error { message, .. } => {
                    format!("<error: {message}>\n")
                }
                ToolOutcome::Denied { reason } => {
                    format!("<denied: {reason}>\n")
                }
                ToolOutcome::Interrupted => return Ok(ToolOutcome::Interrupted),
            };
            let (section, section_truncated) = truncate_utf8_bytes(&section, per_query_cap);
            truncated |= section_truncated;
            output.push_str(&section);
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }
        output.push_str(&omission_note);
        let (output, hard_truncated) = truncate_utf8_bytes(&output, self.cap);
        Ok(ToolOutcome::Ok {
            content: output,
            truncated: truncated || hard_truncated,
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use tempfile::tempdir;

    #[tokio::test]
    async fn batches_multiple_patterns_with_labeled_results() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "alpha\nbeta\n").unwrap();
        let outcome = GrepMany::new()
            .call(
                json!({"queries": [
                    {"pattern": "alpha", "path": ".", "output_mode": "content"},
                    {"pattern": "beta", "path": ".", "output_mode": "content"}
                ]}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match outcome {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("query 1: alpha"), "{content}");
                assert!(content.contains("query 2: beta"), "{content}");
                assert!(content.contains("alpha"), "{content}");
                assert!(content.contains("beta"), "{content}");
            }
            other => panic!("expected batched results, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn small_budgets_keep_both_query_sections_within_the_byte_cap() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n".repeat(200)).unwrap();
        let outcome = GrepMany::with_cap(512)
            .call(
                json!({"queries": [
                    {"pattern": "alpha", "output_mode": "content"},
                    {"pattern": "beta", "output_mode": "content"}
                ]}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        let ToolOutcome::Ok {
            content, truncated, ..
        } = outcome
        else {
            panic!("expected bounded results, got {outcome:?}");
        };
        assert!(truncated);
        assert!(content.len() <= 512, "{} bytes", content.len());
        assert!(content.contains("===== query 1: alpha ====="), "{content}");
        assert!(content.contains("===== query 2: beta ====="), "{content}");
        assert!(content.contains(":alpha"), "{content}");
        assert!(content.contains(":beta"), "{content}");
    }

    #[tokio::test]
    async fn unicode_and_tiny_budgets_remain_utf8_safe_and_byte_bounded() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("é.txt"), "é".repeat(200)).unwrap();
        for cap in [1, 2, 24, 128, 512] {
            let outcome = GrepMany::with_cap(cap)
                .call(
                    json!({"queries": [{"pattern": "é", "output_mode": "content"}]}),
                    &test_ctx(dir.path()),
                )
                .await
                .unwrap();
            let ToolOutcome::Ok { content, .. } = outcome else {
                panic!("expected bounded results, got {outcome:?}");
            };
            assert!(content.len() <= cap, "cap {cap}: {} bytes", content.len());
            assert!(std::str::from_utf8(content.as_bytes()).is_ok());
        }
    }

    #[tokio::test]
    async fn empty_cancelled_and_oversized_batches_keep_their_contracts() {
        let dir = tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let empty = GrepMany::new()
            .call(json!({"queries": []}), &ctx)
            .await
            .unwrap();
        assert!(matches!(empty, ToolOutcome::Error { .. }));

        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let queries = vec![json!({"pattern": "alpha"}); MAX_QUERIES + 1];
        let outcome = GrepMany::new()
            .call(json!({"queries": queries}), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok {
            content, truncated, ..
        } = outcome
        else {
            panic!("expected bounded results, got {outcome:?}");
        };
        assert!(truncated);
        assert_eq!(content.matches("===== query ").count(), MAX_QUERIES);
        assert!(content.contains("omitted 1 queries"));

        ctx.cancel.cancel();
        let cancelled = GrepMany::new()
            .call(json!({"queries": [{"pattern": "alpha"}]}), &ctx)
            .await
            .unwrap();
        assert!(matches!(cancelled, ToolOutcome::Interrupted));
    }
}
