//! Shared agent state: the read registry.
//!
//! Tracks files the agent has Read (path → (mtime, content hash)) so that
//! `Write`/`Edit` (M2) can enforce "read before mutate" (§6.2/§6.3) — the single
//! rule that prevents confident overwrites of hallucinated content. Defined
//! in rc-core (not rc-tools) because [`tool::ToolCtx`] holds it and multiple
//! tools share it; rc-tools' `Read` populates it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout};
use std::sync::{Arc, Mutex};
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

/// Observable metadata for one background shell. Child ownership, pipe drains,
/// log rotation, and reaping live in one session supervisor thread rather than
/// one thread per command.
#[derive(Debug, Clone)]
pub struct BgShell {
    pub id: String,
    pub log_path: PathBuf,
    pub started: SystemTime,
    pub status: Arc<Mutex<BgShellStatus>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgShellStatus {
    Running,
    Exited(i32),
    Killed,
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
    supervisor: Option<BackgroundSupervisor>,
}

impl ShellState {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            bg_dir: None,
            bg: Vec::new(),
            next_bg: 0,
            supervisor: Some(BackgroundSupervisor::new()),
        }
    }

    /// Transfer a freshly-spawned process and its merged output pipe to the
    /// session supervisor. The public registry retains only cheap metadata.
    pub fn supervise_background(
        &mut self,
        shell: BgShell,
        child: Child,
        stdout: ChildStdout,
        pgid: Option<i32>,
    ) -> std::io::Result<()> {
        let Some(supervisor) = &self.supervisor else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "background supervisor is shut down",
            ));
        };
        supervisor.add(shell.clone(), child, stdout, pgid)?;
        self.bg.retain(|entry| {
            entry
                .status
                .lock()
                .map_or(true, |status| *status == BgShellStatus::Running)
        });
        self.bg.push(shell);
        Ok(())
    }

    /// Kill every background shell. Called on session/runtime shutdown so
    /// spawned servers/dev-runs don't outlive `rc` (std `Child` won't kill on
    /// drop, so this must be explicit). Errors are logged, not fatal.
    pub fn shutdown(&mut self) {
        if let Some(mut supervisor) = self.supervisor.take() {
            supervisor.shutdown();
        }
        self.bg.clear();
    }
}

const BACKGROUND_LOG_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;

enum SupervisorCommand {
    Add {
        shell: BgShell,
        child: Child,
        stdout: ChildStdout,
        pgid: Option<i32>,
    },
    Shutdown(std::sync::mpsc::Sender<()>),
}

struct BackgroundSupervisor {
    sender: std::sync::mpsc::Sender<SupervisorCommand>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for BackgroundSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackgroundSupervisor")
            .finish_non_exhaustive()
    }
}

impl BackgroundSupervisor {
    fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || supervisor_loop(receiver));
        Self {
            sender,
            thread: Some(thread),
        }
    }

    fn add(
        &self,
        shell: BgShell,
        child: Child,
        stdout: ChildStdout,
        pgid: Option<i32>,
    ) -> std::io::Result<()> {
        if let Err(std::sync::mpsc::SendError(SupervisorCommand::Add {
            child: mut unsupervised,
            ..
        })) = self.sender.send(SupervisorCommand::Add {
            shell,
            child,
            stdout,
            pgid,
        }) {
            let _ = unsupervised.kill();
            let _ = unsupervised.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "background supervisor stopped",
            ));
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        let (reply, done) = std::sync::mpsc::channel();
        let _ = self.sender.send(SupervisorCommand::Shutdown(reply));
        let _ = done.recv_timeout(std::time::Duration::from_secs(5));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BackgroundSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct SupervisedProcess {
    shell: BgShell,
    child: Child,
    stdout: ChildStdout,
    pgid: Option<i32>,
    log: SegmentedLog,
}

fn supervisor_loop(receiver: std::sync::mpsc::Receiver<SupervisorCommand>) {
    let mut processes: Vec<SupervisedProcess> = Vec::new();
    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(SupervisorCommand::Add {
                shell,
                child,
                stdout,
                pgid,
            }) => add_supervised(&mut processes, shell, child, stdout, pgid),
            Ok(SupervisorCommand::Shutdown(reply)) => {
                for process in &mut processes {
                    kill_supervised(process);
                }
                let _ = reply.send(());
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                for process in &mut processes {
                    kill_supervised(process);
                }
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        while let Ok(command) = receiver.try_recv() {
            match command {
                SupervisorCommand::Add {
                    shell,
                    child,
                    stdout,
                    pgid,
                } => add_supervised(&mut processes, shell, child, stdout, pgid),
                SupervisorCommand::Shutdown(reply) => {
                    for process in &mut processes {
                        kill_supervised(process);
                    }
                    let _ = reply.send(());
                    return;
                }
            }
        }
        processes.retain_mut(|process| {
            if drain_supervised(process) {
                let _ = process.log.flush();
            }
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    // A background command owns its whole process group. Once
                    // the leader exits, terminate daemonized descendants and
                    // reap the leader so neither orphans nor zombies remain.
                    kill_group(process.pgid);
                    let _ = drain_supervised(process);
                    let code = status.code().unwrap_or(-1);
                    let _ = writeln!(process.log, "\n[process exited: {code}]");
                    // Publish the final segment before advertising Exited. This
                    // gives readers a clean happens-before boundary for both
                    // the rotation marker and the process-exit marker.
                    let _ = process.log.flush();
                    if let Ok(mut state) = process.shell.status.lock() {
                        *state = BgShellStatus::Exited(code);
                    }
                    false
                }
                Ok(None) => true,
                Err(error) => {
                    let _ = writeln!(process.log, "\n[process supervision error: {error}]");
                    kill_supervised(process);
                    false
                }
            }
        });
    }
}

fn add_supervised(
    processes: &mut Vec<SupervisedProcess>,
    shell: BgShell,
    mut child: Child,
    stdout: ChildStdout,
    pgid: Option<i32>,
) {
    let log = match set_nonblocking(&stdout).and_then(|()| SegmentedLog::new(&shell.log_path)) {
        Ok(log) => log,
        Err(error) => {
            kill_group(pgid);
            let _ = child.kill();
            let _ = child.wait();
            if let Ok(mut state) = shell.status.lock() {
                *state = BgShellStatus::Killed;
            }
            tracing::warn!(id = %shell.id, "could not supervise background shell: {error}");
            return;
        }
    };
    processes.push(SupervisedProcess {
        shell,
        child,
        stdout,
        pgid,
        log,
    });
}

fn drain_supervised(process: &mut SupervisedProcess) -> bool {
    let mut bytes = [0u8; 16 * 1024];
    let mut wrote = false;
    loop {
        match process.stdout.read(&mut bytes) {
            Ok(0) => return wrote,
            Ok(read) => {
                let _ = process.log.write_all(&bytes[..read]);
                wrote = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return wrote,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return wrote,
        }
    }
}

fn kill_supervised(process: &mut SupervisedProcess) {
    kill_group(process.pgid);
    let _ = process.child.kill();
    let _ = process.child.wait();
    let _ = drain_supervised(process);
    let _ = writeln!(process.log, "\n[process killed during session shutdown]");
    if let Ok(mut state) = process.shell.status.lock() {
        *state = BgShellStatus::Killed;
    }
}

fn kill_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        // SAFETY: this process group was created with setsid before exec.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

fn set_nonblocking(stdout: &ChildStdout) -> std::io::Result<()> {
    let fd = stdout.as_raw_fd();
    // SAFETY: fcntl operates on the owned pipe fd and neither call aliases
    // memory. The fd remains alive for the supervisor's lifetime.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

struct SegmentedLog {
    path: PathBuf,
    writer: Option<std::io::BufWriter<std::fs::File>>,
    written: u64,
}

impl SegmentedLog {
    fn new(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            writer: Some(std::io::BufWriter::new(file)),
            written,
        })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }
        let rotated = PathBuf::from(format!("{}.1", self.path.display()));
        let _ = std::fs::remove_file(&rotated);
        std::fs::rename(&self.path, &rotated)?;
        let mut file = std::io::BufWriter::new(std::fs::File::create(&self.path)?);
        let marker = format!("[log rotated; previous segment: {}]\n", rotated.display());
        file.write_all(marker.as_bytes())?;
        self.written = marker.len() as u64;
        self.writer = Some(file);
        Ok(())
    }
}

impl Write for SegmentedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written > 0
            && self.written.saturating_add(bytes.len() as u64) > BACKGROUND_LOG_SEGMENT_BYTES
        {
            self.rotate()?;
        }
        let written = self.writer.as_mut().expect("log writer").write(bytes)?;
        self.written = self.written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.as_mut().expect("log writer").flush()
    }
}

impl Drop for ShellState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A shared, lockable change journal (the `/rewind` backing store, M7/§9.2).
pub type SharedChangeJournal = std::sync::Arc<Mutex<ChangeJournal>>;

/// One recorded file change: the path, its prior contents (`None` = the file
/// didn't exist before the agent created it), and the turn it happened in.
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    pub path: PathBuf,
    pub prior: Option<FileSnapshot>,
    pub turn: u64,
}

/// One deduplicated rewind payload. The normal path stores bytes once in a
/// content-addressed temporary directory; `Inline` is a fail-safe if the host
/// cannot create or write that directory, so rewind never silently disappears.
#[derive(Debug, Clone)]
pub enum FileSnapshot {
    Cas { path: PathBuf, len: u64 },
    Inline(Arc<[u8]>),
}

impl FileSnapshot {
    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Cas { path, len } => {
                let bytes = std::fs::read(path)?;
                if bytes.len() as u64 != *len {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "rewind snapshot length mismatch",
                    ));
                }
                Ok(bytes)
            }
            Self::Inline(bytes) => Ok(bytes.to_vec()),
        }
    }

    pub fn inline(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self::Inline(bytes.into())
    }
}

/// The `/rewind` backing store: a per-turn journal of pre-mutation file contents.
/// `Write`/`Edit` record here *before* they mutate; `/rewind n` restores the last
/// `n` turns' records. Bash file side-effects are deliberately not journaled
/// (§9.2 — they're outside the CAS and not rolled back).
#[derive(Debug)]
pub struct ChangeJournal {
    records: Vec<ChangeRecord>,
    /// The current (last completed) turn. Incremented by the agent loop at the
    /// top of each turn, before any tools run.
    turn: u64,
    storage: ChangeStorage,
}

#[derive(Debug)]
enum ChangeStorage {
    Temporary(Option<tempfile::TempDir>),
    Durable { root: PathBuf, journal: PathBuf },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DurableJournalEntry {
    Turn {
        turn: u64,
    },
    Change {
        path: PathBuf,
        prior_exists: bool,
        digest: Option<String>,
        len: Option<u64>,
        turn: u64,
    },
}

impl Default for ChangeJournal {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            turn: 0,
            storage: ChangeStorage::Temporary(
                tempfile::Builder::new().prefix("sc-rewind-").tempdir().ok(),
            ),
        }
    }
}

impl ChangeJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a restart-safe rewind journal rooted at `root`. Blobs are stored
    /// once by content hash and JSONL stores only references. A valid prefix is
    /// recovered after a crash; a malformed trailing record is ignored.
    pub fn durable(root: PathBuf) -> std::io::Result<Self> {
        let blobs = root.join("blobs");
        std::fs::create_dir_all(&blobs)?;
        let journal = root.join("rewind.jsonl");
        let mut records = Vec::new();
        let mut turn = 0u64;
        if let Ok(file) = std::fs::File::open(&journal) {
            use std::io::BufRead;
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                match serde_json::from_str::<DurableJournalEntry>(&line) {
                    Ok(DurableJournalEntry::Turn { turn: saved }) => turn = saved,
                    Ok(DurableJournalEntry::Change {
                        path,
                        prior_exists,
                        digest,
                        len,
                        turn: record_turn,
                    }) => {
                        turn = turn.max(record_turn);
                        let prior = digest.zip(len).map(|(digest, len)| FileSnapshot::Cas {
                            path: blobs.join(digest),
                            len,
                        });
                        // A durable CAS write that failed is not replayable.
                        // Skip it after restart rather than misreading it as a
                        // newly-created file and deleting user data on rewind.
                        if prior_exists && prior.is_none() {
                            continue;
                        }
                        records.push(ChangeRecord {
                            path,
                            prior,
                            turn: record_turn,
                        });
                    }
                    Err(_) => break,
                }
            }
        }
        Ok(Self {
            records,
            turn,
            storage: ChangeStorage::Durable { root, journal },
        })
    }

    /// Advance the turn counter — called by the agent loop at the top of each
    /// turn, so records made this turn are stamped with the new turn number.
    pub fn advance_turn(&mut self) {
        self.turn += 1;
        self.append_durable(&DurableJournalEntry::Turn { turn: self.turn });
    }

    /// The current turn number.
    pub fn turn(&self) -> u64 {
        self.turn
    }

    /// Record a pre-mutation snapshot of `path` for the current turn. `prior`
    /// is the file's contents before the change, or `None` if it didn't exist.
    pub fn record(&mut self, path: PathBuf, prior: Option<Arc<[u8]>>) {
        let prior_exists = prior.is_some();
        let prior = prior.map(|bytes| self.store_snapshot(bytes));
        let record = ChangeRecord {
            path: path.clone(),
            prior,
            turn: self.turn,
        };
        let (digest, len) = snapshot_reference(record.prior.as_ref());
        self.append_durable(&DurableJournalEntry::Change {
            path,
            prior_exists,
            digest,
            len,
            turn: self.turn,
        });
        self.records.push(record);
    }

    fn store_snapshot(&self, bytes: Arc<[u8]>) -> FileSnapshot {
        let directory = match &self.storage {
            ChangeStorage::Temporary(Some(cas_dir)) => cas_dir.path().to_path_buf(),
            ChangeStorage::Temporary(None) => return FileSnapshot::Inline(bytes),
            ChangeStorage::Durable { root, .. } => root.join("blobs"),
        };
        let digest = blake3::hash(&bytes).to_hex();
        let path = directory.join(digest.as_str());
        if (path.exists() || std::fs::write(&path, &bytes).is_ok())
            && std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64)
        {
            FileSnapshot::Cas {
                path,
                len: bytes.len() as u64,
            }
        } else {
            FileSnapshot::Inline(bytes)
        }
    }

    fn append_durable(&self, entry: &DurableJournalEntry) {
        let ChangeStorage::Durable { journal, .. } = &self.storage else {
            return;
        };
        use std::io::Write;
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(journal)?;
            serde_json::to_writer(&mut file, entry).map_err(std::io::Error::other)?;
            file.write_all(b"\n")?;
            file.flush()
        })();
        if let Err(error) = result {
            tracing::warn!(path = %journal.display(), "durable rewind append failed: {error}");
        }
    }

    fn rewrite_durable(&self) {
        let ChangeStorage::Durable { journal, .. } = &self.storage else {
            return;
        };
        use std::io::Write;
        let tmp = journal.with_extension(format!("next-{}", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            serde_json::to_writer(&mut file, &DurableJournalEntry::Turn { turn: self.turn })
                .map_err(std::io::Error::other)?;
            file.write_all(b"\n")?;
            for record in &self.records {
                let (digest, len) = snapshot_reference(record.prior.as_ref());
                serde_json::to_writer(
                    &mut file,
                    &DurableJournalEntry::Change {
                        path: record.path.clone(),
                        prior_exists: record.prior.is_some(),
                        digest,
                        len,
                        turn: record.turn,
                    },
                )
                .map_err(std::io::Error::other)?;
                file.write_all(b"\n")?;
            }
            file.flush()?;
            std::fs::rename(&tmp, journal)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!(path = %journal.display(), "durable rewind rewrite failed: {error}");
        }
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
        self.rewrite_durable();
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

fn snapshot_reference(snapshot: Option<&FileSnapshot>) -> (Option<String>, Option<u64>) {
    match snapshot {
        Some(FileSnapshot::Cas { path, len }) => (
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            Some(*len),
        ),
        _ => (None, None),
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
        j.record(PathBuf::from("a"), Some(b"old-a".to_vec().into()));
        j.record(PathBuf::from("b"), None);
        j.advance_turn(); // turn 2
        j.record(PathBuf::from("c"), Some(b"old-c".to_vec().into()));
        j.advance_turn(); // turn 3
        j.record(PathBuf::from("d"), Some(b"old-d".to_vec().into()));
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
        j.record(PathBuf::from("a"), Some(Vec::<u8>::new().into()));
        assert!(j.rewind(0).is_empty());
        assert_eq!(j.len(), 1);
    }

    #[test]
    fn rewind_clamps_to_available_turns() {
        let mut j = ChangeJournal::new();
        j.advance_turn();
        j.record(PathBuf::from("a"), Some(Vec::<u8>::new().into()));
        // Asking for 99 turns only yields what exists.
        let r = j.rewind(99);
        assert_eq!(r.len(), 1);
        assert!(j.is_empty());
    }

    #[test]
    fn durable_journal_survives_restart_and_deduplicates_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("rewind");
        let path = dir.path().join("file.txt");
        {
            let mut journal = ChangeJournal::durable(root.clone()).unwrap();
            journal.advance_turn();
            journal.record(path.clone(), Some(b"original".to_vec().into()));
            journal.record(
                dir.path().join("other.txt"),
                Some(b"original".to_vec().into()),
            );
        }
        assert_eq!(std::fs::read_dir(root.join("blobs")).unwrap().count(), 1);

        let mut resumed = ChangeJournal::durable(root).unwrap();
        assert_eq!(resumed.turn(), 1);
        let records = resumed.rewind(1);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].path, path);
        assert_eq!(
            records[1].prior.as_ref().unwrap().read().unwrap(),
            b"original"
        );
    }

    #[test]
    fn identical_rewind_payloads_share_one_cas_object() {
        let mut journal = ChangeJournal::new();
        journal.advance_turn();
        let bytes: Arc<[u8]> = Arc::from(b"same large file contents".as_slice());
        journal.record(PathBuf::from("a"), Some(bytes.clone()));
        journal.record(PathBuf::from("b"), Some(bytes));
        let records = journal.rewind(1);
        let paths = records
            .iter()
            .map(|record| match record.prior.as_ref().unwrap() {
                FileSnapshot::Cas { path, .. } => path.clone(),
                FileSnapshot::Inline(_) => panic!("temp CAS should be available in tests"),
            })
            .collect::<Vec<_>>();
        assert_eq!(paths[0], paths[1]);
        assert_eq!(
            std::fs::read(&paths[0]).unwrap(),
            b"same large file contents"
        );
    }
}
