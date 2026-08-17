//! Shared agent state: the read registry.
//!
//! Tracks files the agent has Read (path → (mtime, content hash)) so that
//! `Write`/`Edit` (M2) can enforce "read before mutate" (§6.2/§6.3) — the single
//! rule that prevents confident overwrites of hallucinated content. Defined
//! in rc-core (not rc-tools) because [`tool::ToolCtx`] holds it and multiple
//! tools share it; rc-tools' `Read` populates it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
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

    /// Has the path been read at all (any version)?
    pub fn has_read(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// The recorded (mtime, hash) for a path, if it's been read.
    pub fn get(&self, path: &Path) -> Option<&(SystemTime, String)> {
        self.entries.get(path)
    }
}

// ---- M7: shared shell state + change journal --------------------------------

/// A shared, lockable shell state. Cheap to clone (the inner is an `Arc`).
///
/// Holds the session's live working directory (so `cd` persists across Bash
/// calls, M7) and the registry of background shells. `std::process::Child` (not
/// tokio's) is held on purpose: it keeps rc-core free of the tokio `process`
/// feature, and background shells are fire-and-forget + explicit-kill, not async.
pub type SharedShellState = std::sync::Arc<Mutex<ShellState>>;

/// One background shell: its id, output log, the held child process, and start
/// time. The child is held so it can be killed on session shutdown (std
/// `Child` has no kill-on-drop).
#[derive(Debug)]
pub struct BgShell {
    pub id: String,
    pub log_path: PathBuf,
    pub child: Child,
    pub started: SystemTime,
}

/// The live shell state for a session (M7).
#[derive(Debug)]
pub struct ShellState {
    /// The current working directory. Bash updates this after a successful,
    /// contained `cd`; all tools run here.
    pub cwd: PathBuf,
    /// Where background-shell logs are written (under `~/.rc/bg/<session>/`), so
    /// the agent can `Read` them. `None` disables background shells.
    pub bg_dir: Option<PathBuf>,
    /// Running background shells, newest appended.
    pub bg: Vec<BgShell>,
    /// Monotonic id counter for the next background shell (`bg-<n>`).
    pub next_bg: u64,
}

impl ShellState {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            bg_dir: None,
            bg: Vec::new(),
            next_bg: 0,
        }
    }

    /// Kill every background shell. Called on session/runtime shutdown so
    /// spawned servers/dev-runs don't outlive `rc` (std `Child` won't kill on
    /// drop, so this must be explicit). Errors are logged, not fatal.
    pub fn shutdown(&mut self) {
        for mut shell in std::mem::take(&mut self.bg) {
            // `kill` sends SIGTERM (Unix) / TerminateProcess (Windows); reap to
            // avoid a zombie. A child that already exited is a no-op.
            let _ = shell.child.kill();
            let _ = shell.child.try_wait();
        }
    }
}

/// A shared, lockable change journal (the `/rewind` backing store, M7/§9.2).
pub type SharedChangeJournal = std::sync::Arc<Mutex<ChangeJournal>>;

/// One recorded file change: the path, its prior contents (`None` = the file
/// didn't exist before the agent created it), and the turn it happened in.
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    pub path: PathBuf,
    pub prior: Option<Vec<u8>>,
    pub turn: u64,
}

/// The `/rewind` backing store: a per-turn journal of pre-mutation file contents.
/// `Write`/`Edit` record here *before* they mutate; `/rewind n` restores the last
/// `n` turns' records. Bash file side-effects are deliberately not journaled
/// (§9.2 — they're outside the CAS and not rolled back).
#[derive(Debug, Default)]
pub struct ChangeJournal {
    records: Vec<ChangeRecord>,
    /// The current (last completed) turn. Incremented by the agent loop at the
    /// top of each turn, before any tools run.
    turn: u64,
}

impl ChangeJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance the turn counter — called by the agent loop at the top of each
    /// turn, so records made this turn are stamped with the new turn number.
    pub fn advance_turn(&mut self) {
        self.turn += 1;
    }

    /// The current turn number.
    pub fn turn(&self) -> u64 {
        self.turn
    }

    /// Record a pre-mutation snapshot of `path` for the current turn. `prior`
    /// is the file's contents before the change, or `None` if it didn't exist.
    pub fn record(&mut self, path: PathBuf, prior: Option<Vec<u8>>) {
        self.records.push(ChangeRecord {
            path,
            prior,
            turn: self.turn,
        });
    }

    /// Pop and return every record from the last `n` turns (most-recent turn
    /// first, so the caller restores in reverse order), and move the turn
    /// pointer back by `n` (so a subsequent `/rewind` steps further back).
    /// `n` is clamped to the current turn. Records older than the window stay.
    pub fn rewind(&mut self, n: usize) -> Vec<ChangeRecord> {
        if n == 0 || self.records.is_empty() {
            return Vec::new();
        }
        // The earliest turn to undo: turn > (current_turn - n).
        let earliest = self.turn.saturating_sub(n as u64) + 1;
        // Partition out the records in the window (MSRV 1.75: no Vec::extract_if).
        let mut taken: Vec<ChangeRecord> = Vec::new();
        let mut keep: Vec<ChangeRecord> = Vec::new();
        for r in std::mem::take(&mut self.records) {
            if r.turn >= earliest {
                taken.push(r);
            } else {
                keep.push(r);
            }
        }
        self.records = keep;
        // Step the turn pointer back so the next `/rewind` continues backward.
        self.turn = self.turn.saturating_sub(n as u64);
        // Restore in reverse chronological order (newest change first).
        taken.reverse();
        taken
    }

    /// How many records are journaled (test/diagnostic).
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewind_pops_only_the_last_n_turns_in_reverse_order() {
        let mut j = ChangeJournal::new();
        // Turn 1: change a, b. Turn 2: change c. Turn 3: change d.
        j.advance_turn(); // turn 1
        j.record(PathBuf::from("a"), Some(b"old-a".to_vec()));
        j.record(PathBuf::from("b"), None);
        j.advance_turn(); // turn 2
        j.record(PathBuf::from("c"), Some(b"old-c".to_vec()));
        j.advance_turn(); // turn 3
        j.record(PathBuf::from("d"), Some(b"old-d".to_vec()));
        assert_eq!(j.turn(), 3);

        // rewind(1) → only turn 3 (d), newest first.
        let r1 = j.rewind(1);
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].path, PathBuf::from("d"));
        // a, b, c remain.
        assert_eq!(j.len(), 3);

        // rewind(2) now → turns 1 and 2 (c, b, a) — newest turn first.
        let r2 = j.rewind(2);
        assert_eq!(
            r2.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("c"), PathBuf::from("b"), PathBuf::from("a")]
        );
        assert!(j.is_empty());
    }

    #[test]
    fn rewind_zero_is_a_noop() {
        let mut j = ChangeJournal::new();
        j.advance_turn();
        j.record(PathBuf::from("a"), Some(vec![]));
        assert!(j.rewind(0).is_empty());
        assert_eq!(j.len(), 1);
    }

    #[test]
    fn rewind_clamps_to_available_turns() {
        let mut j = ChangeJournal::new();
        j.advance_turn();
        j.record(PathBuf::from("a"), Some(vec![]));
        // Asking for 99 turns only yields what exists.
        let r = j.rewind(99);
        assert_eq!(r.len(), 1);
        assert!(j.is_empty());
    }
}
