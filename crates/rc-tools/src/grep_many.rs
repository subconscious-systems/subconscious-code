//! Batched content searches. `GrepMany` removes a model round trip when several
//! independent patterns or roots are already known, while retaining `Grep`'s
//! path checks, gitignore handling, binary filtering, and output modes.

use crate::grep::Grep;
use crate::util::{cap_output, params_schema};
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
are labeled by query and share one bounded output budget."
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
        let per_query_cap = self.cap.saturating_sub(1024) / queries.len().max(1);
        let grep = Grep::with_cap(per_query_cap);
        let mut output = String::new();
        let mut truncated = omitted > 0;

        for (index, query) in queries.into_iter().enumerate() {
            if ctx.cancel.is_cancelled() {
                return Ok(ToolOutcome::Interrupted);
            }
            let label = format!("===== query {}: {} =====\n", index + 1, query.pattern);
            output.push_str(&label);
            match grep.call(json!(query), ctx).await? {
                ToolOutcome::Ok {
                    content,
                    truncated: query_truncated,
                    ..
                } => {
                    truncated |= query_truncated;
                    output.push_str(&content);
                }
                ToolOutcome::Error { message, .. } => {
                    output.push_str(&format!("<error: {message}>\n"));
                }
                ToolOutcome::Denied { reason } => {
                    output.push_str(&format!("<denied: {reason}>\n"));
                }
                ToolOutcome::Interrupted => return Ok(ToolOutcome::Interrupted),
            }
            if !output.ends_with('\n') {
                output.push('\n');
            }
        }
        if omitted > 0 {
            output.push_str(&format!(
                "[omitted {omitted} queries beyond limit {MAX_QUERIES}]\n"
            ));
        }
        let (hard_truncated, output) = cap_output(
            &output,
            self.cap,
            self.cap.saturating_mul(3) / 4,
            self.cap / 4,
        );
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
}
