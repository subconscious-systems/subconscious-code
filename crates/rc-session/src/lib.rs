//! rc-session: transcript persistence (§9, M5).
//!
//! Append-only JSONL per session: one header line (session metadata) followed
//! by one line per [`Turn`]. The store flushes after every append so a crash
//! mid-conversation leaves a valid, replayable file — the worst case is a
//! truncated final line, which [`load`] skips.
//!
//! ```text
//! {"type":"header","id":"…","cwd":"…","model":"…","mode":"default",…}
//! {"type":"user","content":"hi","ts":1000000}
//! {"type":"assistant","text":"hello",…}
//! {"type":"tool_result","call_id":"c1",…}
//! ```
//!
//! Session files live under a caller-chosen directory (e.g. `~/.rc/sessions/`);
//! this crate is I/O, not policy. M10 will layer CAS checkpoints and `/rewind`
//! on top — rewind restores only files the agent touched; Bash side effects
//! (build artifacts, clones) are outside the CAS and not rolled back.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rc_core::{AgentMode, Session, Turn};
use serde::{Deserialize, Serialize};

/// The first line of a session file: enough to reconstruct a fresh [`Session`]
/// on `--resume`/`--continue`. The `mode` and `extra_dirs` are restored so a
/// resumed session picks up where it left off.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionHeader {
    id: String,
    cwd: PathBuf,
    model: String,
    mode: AgentMode,
    #[serde(default)]
    extra_dirs: Vec<PathBuf>,
}

/// An append-only handle to a session's JSONL file. Flushes on every append
/// so a crash leaves a valid prefix (crash recovery, §9).
pub struct SessionStore {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl SessionStore {
    /// Create (or overwrite) a session file at `path`, writing the header.
    /// The caller picks the path — typically `~/.rc/sessions/<id>.jsonl`.
    pub fn create(path: PathBuf, session: &Session) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating session dir {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("creating session file {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        let header = SessionHeader {
            id: session.id.clone(),
            cwd: session.cwd.clone(),
            model: session.model.clone(),
            mode: session.mode,
            extra_dirs: session.extra_dirs.clone(),
        };
        let line = serde_json::to_string(&header)
            .context("serializing session header")?;
        writeln!(writer, "{line}")?;
        writer.flush()?;
        Ok(Self { writer, path })
    }

    /// Append a turn as one JSON line, then flush (crash recovery: the file is
    /// always a valid prefix up to the last completed turn).
    pub fn append_turn(&mut self, turn: &Turn) -> Result<()> {
        let line = serde_json::to_string(turn).context("serializing turn")?;
        writeln!(self.writer, "{line}")?;
        self.writer.flush().context("flushing session file")?;
        Ok(())
    }

    /// The path this store writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open an existing session file in append mode (for `--resume`: keep the
    /// header and all prior turns, append new ones after). The file must already
    /// exist with a valid header — use [`SessionStore::create`] for a fresh
    /// session. Returns an error if the file is missing or empty.
    pub fn open_append(path: PathBuf) -> Result<Self> {
        let file = OpenOptions::new()
            .create(false)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening session file for append: {}", path.display()))?;
        Ok(Self { writer: BufWriter::new(file), path })
    }
}

/// Load a session file: read the header, replay every well-formed turn line
/// into a [`Session`], and skip any truncated/garbled trailing line (the
/// crash-recovery contract — a killed process may leave a partial last line).
pub fn load(path: &Path) -> Result<Session> {
    let file = File::open(path).with_context(|| format!("opening session file {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("session file {} is empty", path.display()))??;
    let header: SessionHeader = serde_json::from_str(&header_line)
        .context("parsing session header (first line)")?;

    let mut session = Session::new(header.id, header.cwd, header.model);
    session.mode = header.mode;
    session.extra_dirs = header.extra_dirs;

    let mut skipped = 0;
    for line in lines {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                // A read error on a later line — stop, keep what we have.
                break;
            }
        };
        match serde_json::from_str::<Turn>(&line) {
            Ok(turn) => session.messages.push(turn),
            Err(_) => {
                // A truncated/garbled final line (crash mid-write): skip it.
                skipped += 1;
            }
        }
    }
    if skipped > 0 {
        tracing::debug!("session load: skipped {skipped} malformed trailing line(s)");
    }
    Ok(session)
}

/// Find the most recently modified `.jsonl` session file in `dir`, for
/// `--continue` (resume the last session). `None` if the dir is empty/absent.
pub fn latest(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                let m = e.metadata().ok()?;
                let mtime = m.modified().ok()?;
                Some((p, mtime))
            } else {
                None
            }
        })
        .max_by_key(|(_, m)| *m)
        .map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::NoteKind;
    use rc_core::ToolCall;
    use rc_core::ToolResultBody;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::tempdir;

    fn sample_session(dir: &Path) -> Session {
        let mut s = Session::new("test-id".into(), dir.to_path_buf(), "mock-model".into());
        s.mode = AgentMode::AcceptEdits;
        s.extra_dirs = vec![PathBuf::from("/tmp/extra")];
        s
    }

    fn sample_turns() -> Vec<Turn> {
        vec![
            Turn::User { content: "hello".into(), ts: UNIX_EPOCH + Duration::from_secs(1000) },
            Turn::Assistant {
                text: "hi there".into(),
                reasoning: None,
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    arguments: r#"{"file_path":"a"}"#.into(),
                }],
                usage: None,
            },
            Turn::ToolResult {
                call_id: "c1".into(),
                tool: "Read".into(),
                result: ToolResultBody::Ok { content: "body".into(), truncated: false },
                duration: Duration::from_millis(42),
            },
            Turn::SystemNote { kind: NoteKind::Notice, text: "note".into() },
        ]
    }

    #[test]
    fn round_trips_a_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test-id.jsonl");
        let session = sample_session(dir.path());
        let turns = sample_turns();

        let mut store = SessionStore::create(path.clone(), &session).unwrap();
        for t in &turns {
            store.append_turn(t).unwrap();
        }
        drop(store);

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.id, "test-id");
        assert_eq!(loaded.model, "mock-model");
        assert_eq!(loaded.cwd, dir.path());
        assert_eq!(loaded.mode, AgentMode::AcceptEdits);
        assert_eq!(loaded.extra_dirs, vec![PathBuf::from("/tmp/extra")]);
        assert_eq!(loaded.messages.len(), turns.len());
        // Spot-check a couple of turns survive structurally.
        assert!(matches!(&loaded.messages[0], Turn::User { content, .. } if content == "hello"));
        assert!(matches!(&loaded.messages[2], Turn::ToolResult { tool, .. } if tool == "Read"));
    }

    #[test]
    fn crash_recovery_skips_truncated_trailing_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("crash.jsonl");
        let session = sample_session(dir.path());
        let turns = sample_turns();

        let mut store = SessionStore::create(path.clone(), &session).unwrap();
        for t in &turns {
            store.append_turn(t).unwrap();
        }
        drop(store);

        // Simulate a crash mid-write: append a partial/garbled line (not
        // truncate-and-write — that would erase the good turns).
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"type\":\"user\",\"content\":\"par").unwrap();
        f.flush().unwrap();
        drop(f);

        let loaded = load(&path).unwrap();
        // The 4 well-formed turns are recovered; the truncated 5th is skipped.
        assert_eq!(loaded.messages.len(), turns.len());
    }

    #[test]
    fn latest_finds_newest_jsonl() {
        let dir = tempdir().unwrap();
        // No sessions yet.
        assert!(latest(dir.path()).is_none());

        let old = dir.path().join("old.jsonl");
        std::fs::write(&old, "{\"type\":\"header\",\"id\":\"old\",\"cwd\":\"/\",\"model\":\"m\",\"mode\":\"default\",\"extra_dirs\":[]}").unwrap();

        // Ensure a measurable mtime gap.
        std::thread::sleep(Duration::from_millis(50));
        let new = dir.path().join("new.jsonl");
        std::fs::write(&new, "{\"type\":\"header\",\"id\":\"new\",\"cwd\":\"/\",\"model\":\"m\",\"mode\":\"default\",\"extra_dirs\":[]}").unwrap();

        let found = latest(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "new.jsonl");
    }

    #[test]
    fn create_makes_parent_dirs() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b/c/session.jsonl");
        let session = sample_session(dir.path());
        let store = SessionStore::create(nested.clone(), &session).unwrap();
        assert!(nested.exists());
        assert_eq!(store.path(), nested);
    }

    #[test]
    fn open_append_keeps_prior_turns_and_adds_new_ones() {
        // A resumed session: write a header + some turns with `create`, drop the
        // store, re-open with `open_append`, add more turns, and confirm `load`
        // sees both batches in order (no truncation, no rewrite).
        let dir = tempdir().unwrap();
        let path = dir.path().join("resume.jsonl");
        let session = sample_session(dir.path());
        let turns = sample_turns();

        let mut store = SessionStore::create(path.clone(), &session).unwrap();
        for t in &turns {
            store.append_turn(t).unwrap();
        }
        drop(store);

        // Re-open in append mode and add one more user turn.
        let mut store = SessionStore::open_append(path.clone()).unwrap();
        store
            .append_turn(&Turn::User {
                content: "after resume".into(),
                ts: UNIX_EPOCH + Duration::from_secs(2000),
            })
            .unwrap();
        drop(store);

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.messages.len(), turns.len() + 1);
        assert!(matches!(loaded.messages.last(), Some(Turn::User { content, .. }) if content == "after resume"));
    }

    #[test]
    fn open_append_errors_on_missing_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        assert!(SessionStore::open_append(missing).is_err());
    }
}
