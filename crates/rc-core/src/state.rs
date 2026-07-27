//! Shared agent state: the read registry.
//!
//! Tracks files the agent has Read (path → (mtime, content hash)) so that
//! `Write`/`Edit` (M2) can enforce "read before mutate" (§6.2/§6.3) — the single
//! rule that prevents confident overwrites of hallucinated content. Defined
//! in rc-core (not rc-tools) because [`tool::ToolCtx`] holds it and multiple
//! tools share it; rc-tools' `Read` populates it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

/// A shared, lockable read registry. Cheap to clone (the inner is an `Arc`).
pub type SharedReadRegistry = std::sync::Arc<Mutex<ReadRegistry>>;

#[derive(Debug, Default, Clone)]
pub struct ReadRegistry {
    entries: HashMap<std::path::PathBuf, (SystemTime, String)>,
}

impl ReadRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a read: path → (mtime, content hash). The hash is computed by the
    /// `Read` tool (blake3, in rc-tools) and stored opaquely here.
    pub fn record(&mut self, path: std::path::PathBuf, mtime: SystemTime, hash: String) {
        self.entries.insert(path, (mtime, hash));
    }

    /// Has this exact path been read at this exact mtime?
    pub fn is_current(&self, path: &Path, mtime: SystemTime) -> bool {
        matches!(self.entries.get(path), Some((m, _)) if *m == mtime)
    }

    /// Has the path been read at all (any version)?
    pub fn has_read(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }
}
