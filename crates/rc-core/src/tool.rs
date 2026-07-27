//! The `Tool` trait, concurrency classes, outcomes, and per-call context (§6).

use crate::state::SharedReadRegistry;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// How a tool may run relative to others in the same batch (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    /// Pure reads — run concurrently, bounded by a semaphore (default 8).
    Parallel,
    /// FS mutations — serialize among themselves, in model order.
    SerialWrite,
    /// Run alone; drain everything else first.
    Exclusive,
}

/// A tool's outcome. Rendered to the `role:tool` message content by the loop.
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Ok {
        content: String,
        truncated: bool,
        /// M1 leaves artifacts empty; the TUI uses them for diff previews (M2).
        artifacts: Vec<Artifact>,
    },
    Denied {
        reason: String,
    },
    Error {
        message: String,
        retryable: bool,
    },
    Interrupted,
}

impl ToolOutcome {
    pub fn ok(content: String) -> Self {
        Self::Ok { content, truncated: false, artifacts: Vec::new() }
    }
    pub fn error(message: String) -> Self {
        Self::Error { message, retryable: false }
    }
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub kind: String,
    pub path: Option<PathBuf>,
}

#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// Per-call context. Cheap to clone (the registry + cancel token are `Arc`s);
/// the loop clones it into each concurrently-spawned tool (§4.3).
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub allowed_roots: Vec<PathBuf>,
    pub cancel: CancellationToken,
    pub read_registry: SharedReadRegistry,
}

/// The tool trait (§6). Schemas are generated once and the registry caches the
/// canonical on-wire bytes (§4.6). `permission_key` is M3 and omitted here.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for `parameters` (e.g. via `schemars` in rc-tools).
    fn schema(&self) -> Value;
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }
    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError>;
}
