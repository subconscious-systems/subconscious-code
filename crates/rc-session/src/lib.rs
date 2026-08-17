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

pub mod rewind;

use anyhow::{Context, Result};
use rc_core::{AgentMode, NoteKind, Session, Turn};
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
        let line = serde_json::to_string(&header).context("serializing session header")?;
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
        Ok(Self {
            writer: BufWriter::new(file),
            path,
        })
    }
}

/// Load a session file: read the header, replay every well-formed turn line
/// into a [`Session`], and skip any truncated/garbled trailing line (the
/// crash-recovery contract — a killed process may leave a partial last line).
pub fn load(path: &Path) -> Result<Session> {
    let file =
        File::open(path).with_context(|| format!("opening session file {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("session file {} is empty", path.display()))??;
    let header: SessionHeader =
        serde_json::from_str(&header_line).context("parsing session header (first line)")?;

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
            Ok(turn) => {
                if let Turn::SystemNote {
                    kind: NoteKind::ModeChange,
                    text,
                } = &turn
                {
                    if let Some(mode) = mode_from_note(text) {
                        session.mode = mode;
                    }
                }
                if let Turn::Assistant {
                    usage: Some(usage), ..
                } = &turn
                {
                    session.total_usage.add(usage);
                }
                session.messages.push(turn);
            }
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

/// Decode append-only mode metadata written after the immutable header.
fn mode_from_note(text: &str) -> Option<AgentMode> {
    match text {
        "default" => Some(AgentMode::Default),
        "accept_edits" | "acceptEdits" => Some(AgentMode::AcceptEdits),
        "plan" => Some(AgentMode::Plan),
        "ask" => Some(AgentMode::Ask),
        "auto" | "bypass_permissions" => Some(AgentMode::Auto),
        _ => None,
    }
}

/// A session file's metadata, for a picker UI (`/menu`): the header fields
/// plus a human label taken from the first user prompt.
///
/// Deliberately **does not** carry a turn count. Building this for every file
/// in the sessions directory has to stay cheap, and this project's whole point
/// is that a session may be enormous — counting turns would mean reading every
/// byte of every session just to draw a menu. Everything here comes from the
/// first few lines ([`HEAD_SCAN_LINES`]).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub cwd: PathBuf,
    pub model: String,
    /// File mtime — "how recently was this worked on", for sorting/display.
    pub modified: std::time::SystemTime,
    /// The session's first user prompt, collapsed to a single line. `None` for
    /// a session whose opening turn isn't a user message (or is unreadable).
    pub first_prompt: Option<String>,
}

/// How many lines into a session file [`list`] looks for the first user turn.
/// The opening turn is line 1 in practice; this is a bound against a
/// pathological file, not a real search depth.
const HEAD_SCAN_LINES: usize = 32;

/// Every readable session in `dir`, newest first.
///
/// Header-only files are skipped for the same reason [`latest`] skips them: a
/// session that died at startup has nothing to resume, and showing it in a
/// picker is an invitation to resume nothing. Unparseable files are skipped
/// rather than failing the whole listing — one corrupt file must not take the
/// menu down.
pub fn list(dir: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionInfo> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let modified = e.metadata().ok()?.modified().ok()?;
            read_info(&path, modified)
        })
        .collect();
    // Newest first: the session you want is nearly always the one you just left.
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Read one session's [`SessionInfo`] from its first few lines. `None` if the
/// header is missing/unparseable or the file holds no turns.
fn read_info(path: &Path, modified: std::time::SystemTime) -> Option<SessionInfo> {
    let file = File::open(path).ok()?;
    let mut lines = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty());

    let header: SessionHeader = serde_json::from_str(&lines.next()?).ok()?;

    // The first user turn is the label. Taking it also proves the file has at
    // least one turn beyond the header (the `latest` "no orphans" rule).
    let mut first_prompt = None;
    let mut has_turn = false;
    for line in lines.take(HEAD_SCAN_LINES) {
        has_turn = true;
        if let Ok(Turn::User { content, .. }) = serde_json::from_str::<Turn>(&line) {
            first_prompt = Some(one_line(&content));
            break;
        }
    }
    if !has_turn {
        return None;
    }

    Some(SessionInfo {
        path: path.to_path_buf(),
        id: header.id,
        cwd: header.cwd,
        model: header.model,
        modified,
        first_prompt,
    })
}

/// Collapse a prompt to a single display line: newlines and runs of whitespace
/// become single spaces, so a pasted multi-line prompt can't break the menu's
/// row layout. Length is the caller's business (it knows the column width).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Find the most recently modified `.jsonl` session file in `dir`, for
/// `--continue` (resume the last session). `None` if the dir is empty/absent.
///
/// Files holding only a header and no turns are skipped: a session that died
/// during startup (or one the user opened and immediately quit) has nothing to
/// resume, and picking it as "the last session" would silently strip the history
/// the user actually meant to continue.
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
        .filter(|(p, _)| has_turns(p))
        .max_by_key(|(_, m)| *m)
        .map(|(p, _)| p)
}

/// Does this session file hold at least one turn line beyond the header? Cheap:
/// stops at the second line rather than parsing the file.
fn has_turns(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .nth(1)
        .is_some()
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
            Turn::User {
                content: "hello".into(),
                ts: UNIX_EPOCH + Duration::from_secs(1000),
            },
            Turn::Assistant {
                text: "hi there".into(),
                reasoning: None,
                calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    arguments: r#"{"file_path":"a"}"#.into(),
                }],
                usage: Some(rc_core::Usage {
                    prompt_tokens: 40,
                    completion_tokens: 3,
                    total_tokens: 43,
                    prompt_tokens_details: None,
                }),
            },
            Turn::ToolResult {
                call_id: "c1".into(),
                tool: "Read".into(),
                result: ToolResultBody::Ok {
                    content: "body".into(),
                    truncated: false,
                },
                duration: Duration::from_millis(42),
            },
            Turn::SystemNote {
                kind: NoteKind::Notice,
                text: "note".into(),
            },
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
        assert_eq!(loaded.total_usage.prompt_tokens, 40);
        assert_eq!(loaded.total_usage.total_tokens, 43);
        // Spot-check a couple of turns survive structurally.
        assert!(
            matches!(&loaded.messages[0], Turn::User { content, .. } if content.as_ref() == "hello")
        );
        assert!(matches!(&loaded.messages[2], Turn::ToolResult { tool, .. } if tool == "Read"));
    }

    #[test]
    fn latest_mode_change_note_overrides_the_immutable_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mode-resume.jsonl");
        let session = sample_session(dir.path());
        let mut store = SessionStore::create(path.clone(), &session).unwrap();
        store
            .append_turn(&Turn::SystemNote {
                kind: NoteKind::ModeChange,
                text: "plan".into(),
            })
            .unwrap();
        store
            .append_turn(&Turn::SystemNote {
                kind: NoteKind::ModeChange,
                text: "auto".into(),
            })
            .unwrap();
        drop(store);

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.mode, AgentMode::Auto);
        assert_eq!(
            loaded.messages.len(),
            2,
            "metadata remains append-only history"
        );
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
        f.write_all(b"{\"type\":\"user\",\"content\":\"par")
            .unwrap();
        f.flush().unwrap();
        drop(f);

        let loaded = load(&path).unwrap();
        // The 4 well-formed turns are recovered; the truncated 5th is skipped.
        assert_eq!(loaded.messages.len(), turns.len());
    }

    /// Write a session file with a header and, optionally, one turn.
    fn write_session_file(dir: &Path, name: &str, with_turn: bool) -> PathBuf {
        let path = dir.join(name);
        let session = sample_session(dir);
        let mut store = SessionStore::create(path.clone(), &session).unwrap();
        if with_turn {
            store.append_turn(&sample_turns()[0]).unwrap();
        }
        path
    }

    #[test]
    fn latest_finds_newest_jsonl() {
        let dir = tempdir().unwrap();
        // No sessions yet.
        assert!(latest(dir.path()).is_none());

        write_session_file(dir.path(), "old.jsonl", true);

        // Ensure a measurable mtime gap.
        std::thread::sleep(Duration::from_millis(50));
        write_session_file(dir.path(), "new.jsonl", true);

        let found = latest(dir.path()).unwrap();
        assert_eq!(found.file_name().unwrap(), "new.jsonl");
    }

    /// A header-only file is an aborted session — startup failed, or the user
    /// quit before saying anything. `--continue` must skip it and resume the
    /// newest session that actually has history, not silently start blank.
    #[test]
    fn latest_skips_turnless_orphan_files() {
        let dir = tempdir().unwrap();
        write_session_file(dir.path(), "real.jsonl", true);

        std::thread::sleep(Duration::from_millis(50));
        write_session_file(dir.path(), "orphan.jsonl", false);

        let found = latest(dir.path()).expect("the real session is still found");
        assert_eq!(
            found.file_name().unwrap(),
            "real.jsonl",
            "the newer header-only orphan must not win"
        );
    }

    #[test]
    fn latest_is_none_when_every_file_is_an_orphan() {
        let dir = tempdir().unwrap();
        write_session_file(dir.path(), "a.jsonl", false);
        write_session_file(dir.path(), "b.jsonl", false);
        assert!(latest(dir.path()).is_none(), "nothing resumable here");
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
        assert!(
            matches!(loaded.messages.last(), Some(Turn::User { content, .. }) if content.as_ref() == "after resume")
        );
    }

    #[test]
    fn open_append_errors_on_missing_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        assert!(SessionStore::open_append(missing).is_err());
    }

    /// Write a session file with `id`/`cwd` and one user turn, for the
    /// `list`/project-grouping tests.
    fn write_session(dir: &Path, id: &str, cwd: &Path, prompt: &str) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        let mut session = Session::new(id.into(), cwd.to_path_buf(), "mock-model".into());
        session.mode = AgentMode::Default;
        let mut store = SessionStore::create(path.clone(), &session).unwrap();
        store
            .append_turn(&Turn::User {
                content: prompt.into(),
                ts: UNIX_EPOCH + Duration::from_secs(1000),
            })
            .unwrap();
        path
    }

    /// `list` reports each session's header plus a first-prompt label, which is
    /// what the `/menu` picker shows for a row.
    #[test]
    fn list_reports_header_and_first_prompt() {
        let dir = tempdir().unwrap();
        let proj = PathBuf::from("/tmp/project-a");
        write_session(dir.path(), "s1", &proj, "add a rotating logo");

        let infos = list(dir.path());
        assert_eq!(infos.len(), 1, "one session: {infos:?}");
        assert_eq!(infos[0].id, "s1");
        assert_eq!(infos[0].cwd, proj);
        assert_eq!(
            infos[0].first_prompt.as_deref(),
            Some("add a rotating logo")
        );
    }

    /// A multi-line prompt collapses to one display line — a pasted block must
    /// not break the picker's row layout.
    #[test]
    fn list_collapses_a_multiline_prompt_to_one_line() {
        let dir = tempdir().unwrap();
        write_session(
            dir.path(),
            "s1",
            Path::new("/tmp/p"),
            "first line\n\nsecond   line",
        );

        let infos = list(dir.path());
        assert_eq!(
            infos[0].first_prompt.as_deref(),
            Some("first line second line")
        );
    }

    /// Header-only sessions (died at startup) are skipped, exactly as `latest`
    /// skips them — offering one in a picker resumes nothing.
    #[test]
    fn list_skips_sessions_with_no_turns() {
        let dir = tempdir().unwrap();
        let session = sample_session(dir.path());
        SessionStore::create(dir.path().join("orphan.jsonl"), &session).unwrap();
        write_session(dir.path(), "real", Path::new("/tmp/p"), "hi");

        let infos = list(dir.path());
        assert_eq!(infos.len(), 1, "only the session with turns: {infos:?}");
        assert_eq!(infos[0].id, "real");
    }

    /// One corrupt file must not take the whole listing down — the menu still
    /// shows every session it can read.
    #[test]
    fn list_skips_unreadable_files_without_failing() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("garbage.jsonl"), "not json\nnor this\n").unwrap();
        write_session(dir.path(), "good", Path::new("/tmp/p"), "hi");

        let infos = list(dir.path());
        assert_eq!(infos.len(), 1, "the readable one survives: {infos:?}");
        assert_eq!(infos[0].id, "good");
    }

    /// An absent sessions directory lists empty rather than erroring — a
    /// first run has no `~/.sc/sessions` yet and must still open the menu.
    #[test]
    fn list_of_a_missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        assert!(list(&dir.path().join("nope")).is_empty());
    }
}
