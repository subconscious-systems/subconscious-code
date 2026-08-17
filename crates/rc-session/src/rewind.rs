//! `/rewind` (M7/§9.2): restore files the agent touched via `Write`/`Edit`.
//!
//! The change journal ([`rc_core::state::ChangeJournal`]) records a pre-mutation
//! snapshot of every file `Write`/`Edit` overwrites, stamped with the turn it
//! happened in. `/rewind n` pops the last `n` turns of records and restores
//! each file to its prior contents (or deletes it if the agent created it).
//!
//! Bash file side-effects are deliberately outside the journal — `mkdir`,
//! `rm`, `sed -i`, build artifacts, clones, etc. are not rolled back (§9.2).
//! The conversation transcript is append-only, so `/rewind` restores *files*
//! only; the turns themselves stay in history with a `SystemNote` marker.

use std::path::PathBuf;

use anyhow::Result;
use rc_core::Session;

/// The outcome of a `/rewind`: which files were restored and how many turns
/// of changes were rolled back.
#[derive(Debug, Clone)]
pub struct RewindReport {
    /// Files restored (to prior contents) or removed (agent-created), in
    /// application order.
    pub restored: Vec<PathBuf>,
    /// The number of turns of changes that were rolled back.
    pub turns: usize,
}

/// Restore a list of change records (already popped from the journal, most
/// recent first). For each: write back the prior contents, or delete the file
/// if `prior` is `None` (the agent created it). Errors per-file are collected
/// rather than aborting the whole rewind — a partial rewind is better than
/// none, and a missing file (already deleted out of band) is not an error.
pub(crate) fn restore_files(records: &[rc_core::state::ChangeRecord]) -> Vec<PathBuf> {
    let mut restored = Vec::new();
    for r in records {
        match &r.prior {
            Some(bytes) => {
                if let Some(parent) = r.path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::write(&r.path, bytes).is_ok() {
                    restored.push(r.path.clone());
                }
            }
            None => {
                // The agent created this file; removing restores the pre-turn
                // state. A missing file is a no-op (already gone).
                if std::fs::remove_file(&r.path).is_ok() || !r.path.exists() {
                    restored.push(r.path.clone());
                }
            }
        }
    }
    restored
}

/// Roll back the last `n` turns of agent file changes for `session`, restoring
/// files from the change journal. Returns what was restored. Does not mutate
/// the conversation transcript.
pub fn rewind_session(session: &mut Session, n: usize) -> Result<RewindReport> {
    let records: Vec<rc_core::state::ChangeRecord> = {
        let mut journal = session
            .change_journal
            .lock()
            .map_err(|_| anyhow::anyhow!("change journal poisoned"))?;
        journal.rewind(n)
    };
    if records.is_empty() {
        return Ok(RewindReport {
            restored: Vec::new(),
            turns: n,
        });
    }
    let restored = restore_files(&records);
    Ok(RewindReport { restored, turns: n })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::state::{ChangeJournal, ChangeRecord};
    use rc_core::Session;
    use std::fs;
    use tempfile::tempdir;

    fn session_with_journal(dir: &std::path::Path) -> Session {
        let mut s = Session::new("test".into(), dir.to_path_buf(), "m".into());
        // Session::new already allocates a fresh change_journal.
        let _ = &mut s;
        s
    }

    #[test]
    fn rewind_restores_an_edited_file_to_prior_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, "original").unwrap();
        let mut session = session_with_journal(dir.path());
        // Simulate one agent turn: record the prior, then overwrite.
        session.change_journal.lock().unwrap().advance_turn();
        session
            .change_journal
            .lock()
            .unwrap()
            .record(path.clone(), Some(b"original".to_vec()));
        fs::write(&path, "changed").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "changed");

        let report = rewind_session(&mut session, 1).unwrap();
        assert_eq!(report.restored, vec![path.clone()]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn rewind_deletes_an_agent_created_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let mut session = session_with_journal(dir.path());
        session.change_journal.lock().unwrap().advance_turn();
        session
            .change_journal
            .lock()
            .unwrap()
            .record(path.clone(), None);
        fs::write(&path, "created").unwrap();
        assert!(path.exists());

        let report = rewind_session(&mut session, 1).unwrap();
        assert_eq!(report.restored, vec![path.clone()]);
        assert!(
            !path.exists(),
            "agent-created file should be removed on rewind"
        );
    }

    #[test]
    fn rewind_only_touches_the_last_n_turns() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, "a0").unwrap();
        fs::write(&b, "b0").unwrap();
        let mut session = session_with_journal(dir.path());

        // Turn 1: change a.
        session.change_journal.lock().unwrap().advance_turn();
        session
            .change_journal
            .lock()
            .unwrap()
            .record(a.clone(), Some(b"a0".to_vec()));
        fs::write(&a, "a1").unwrap();

        // Turn 2: change b.
        session.change_journal.lock().unwrap().advance_turn();
        session
            .change_journal
            .lock()
            .unwrap()
            .record(b.clone(), Some(b"b0".to_vec()));
        fs::write(&b, "b1").unwrap();

        // Rewind only the last 1 turn → b restored, a stays changed.
        let report = rewind_session(&mut session, 1).unwrap();
        assert_eq!(report.restored, vec![b.clone()]);
        assert_eq!(fs::read_to_string(&a).unwrap(), "a1");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b0");

        // Rewind the remaining 1 turn → a restored.
        let report = rewind_session(&mut session, 1).unwrap();
        assert_eq!(report.restored, vec![a.clone()]);
        assert_eq!(fs::read_to_string(&a).unwrap(), "a0");
    }

    #[test]
    fn rewind_with_no_changes_is_a_noop() {
        let dir = tempdir().unwrap();
        let mut session = session_with_journal(dir.path());
        session.change_journal.lock().unwrap().advance_turn();
        let report = rewind_session(&mut session, 1).unwrap();
        assert!(report.restored.is_empty());
    }

    #[test]
    fn restore_files_handles_redundant_records_for_one_path() {
        // File created then edited in the same window: create (prior=None),
        // edit (prior=v_created). Reversed by journal::rewind → edit then
        // create → delete. Net: file absent (its pre-window state).
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.txt");
        let records = vec![
            ChangeRecord {
                path: path.clone(),
                prior: Some(b"created".to_vec()),
                turn: 1,
            },
            ChangeRecord {
                path: path.clone(),
                prior: None,
                turn: 1,
            },
        ];
        fs::write(&path, "edited").unwrap();
        let restored = restore_files(&records);
        assert_eq!(restored, vec![path.clone(), path.clone()]);
        assert!(!path.exists());
        let _ = ChangeJournal::new();
    }
}
