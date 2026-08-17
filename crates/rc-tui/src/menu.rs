//! The `/menu` overlay: projects, their sessions, and an editable settings
//! page.
//!
//! A small state machine over four pages ([`MenuPage`]) plus a uniform row
//! model ([`Row`]), so navigation, selection clamping, and rendering are
//! written once instead of per page. The page is a full-area modal: while it's
//! open the composer keymap is bypassed entirely (see `app::handle_key`), which
//! is what lets plain `↑/↓/←/→/Enter` drive it without colliding with editing
//! keys.
//!
//! **Projects are derived, not stored.** There is no project registry — a
//! "project" is a distinct `cwd` across the session files in `~/.sc/sessions`
//! ([`rc_session::list`]). That means the list is always truthful about where
//! work actually happened, and nothing has to be kept in sync. Directories that
//! no longer exist still appear (with a marker): they are real history, and
//! silently dropping them would look like data loss.
//!
//! Resuming can't happen in-process: a different session means a different
//! `cwd`, hence a different tool set and permission roots, all built in
//! `rc-cli` above this crate. So selection returns an [`Outcome`] that
//! `app::run` propagates out of the TUI for `main.rs` to act on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rc_config::edit::{FieldKind, FieldSpec, EDITABLE};
use rc_config::Settings;
use rc_session::SessionInfo;

/// What the menu asks the host to do after it closes. Anything that changes
/// which session is running has to leave the TUI, since the agent loop and its
/// cwd-scoped tools are constructed a layer up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Resume the session stored at this path.
    Resume(PathBuf),
    /// Start a fresh session with this working directory.
    NewIn(PathBuf),
}

/// Which page is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MenuPage {
    Root,
    Projects,
    /// Sessions belonging to one project directory.
    Sessions(PathBuf),
    Settings,
}

/// A project: one working directory, with the sessions recorded against it.
#[derive(Debug, Clone)]
pub(crate) struct Project {
    pub dir: PathBuf,
    pub sessions: Vec<SessionInfo>,
    /// Most recent session mtime — the sort key and the "last worked on" label.
    pub last: SystemTime,
}

impl Project {
    /// The directory's final component, the name worth reading in a list. Falls
    /// back to the full path for a root-ish directory with no file name.
    pub fn name(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.dir.display().to_string())
    }
}

/// One selectable line. Pages differ in what they list; navigation doesn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Row {
    /// Go to another page.
    Goto(MenuPage),
    Project(PathBuf),
    Session(PathBuf),
    /// Start a fresh session in this project's directory.
    NewSession(PathBuf),
    /// A settings field, by index into [`EDITABLE`].
    Field(usize),
    Close,
}

/// The open menu. `Clone` because it lives in `ViewState`, which render tests
/// clone wholesale.
#[derive(Clone)]
pub(crate) struct MenuState {
    pub page: MenuPage,
    /// Selected row on the current page.
    pub selected: usize,
    /// Sessions grouped into projects, read once when the menu opens. A menu
    /// that re-scanned the disk every frame would stat every session file at
    /// the render rate; the listing is a snapshot and `r` refreshes it.
    pub projects: Vec<Project>,
    /// Resolved settings, re-read after each successful save so the page shows
    /// what the loader would now produce.
    pub settings: Settings,
    /// The in-progress text buffer while editing a field, if any.
    pub editing: Option<String>,
    /// A transient line under the page: a save confirmation or an error.
    pub status: Option<String>,
}

impl MenuState {
    /// Open the menu, reading the session listing from `sessions_dir` and
    /// resolving settings against `project_dir`.
    pub fn new(sessions_dir: &Path, project_dir: &Path) -> Self {
        Self {
            page: MenuPage::Root,
            selected: 0,
            projects: group_projects(rc_session::list(sessions_dir)),
            settings: Settings::load(project_dir),
            editing: None,
            status: None,
        }
    }

    /// The current page's rows, in display order.
    pub fn rows(&self) -> Vec<Row> {
        match &self.page {
            MenuPage::Root => vec![
                Row::Goto(MenuPage::Projects),
                Row::Goto(MenuPage::Settings),
                Row::Close,
            ],
            MenuPage::Projects => self
                .projects
                .iter()
                .map(|p| Row::Project(p.dir.clone()))
                .collect(),
            MenuPage::Sessions(dir) => {
                let mut rows: Vec<Row> = self
                    .project(dir)
                    .map(|p| {
                        p.sessions
                            .iter()
                            .map(|s| Row::Session(s.path.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                rows.push(Row::NewSession(dir.clone()));
                rows
            }
            MenuPage::Settings => (0..EDITABLE.len()).map(Row::Field).collect(),
        }
    }

    pub fn project(&self, dir: &Path) -> Option<&Project> {
        self.projects.iter().find(|p| p.dir == dir)
    }

    /// The selected row, if the page has any.
    pub fn current_row(&self) -> Option<Row> {
        self.rows().get(self.selected).cloned()
    }

    /// Move the selection by `delta`, wrapping at both ends so holding a
    /// direction never dead-ends.
    pub fn move_selection(&mut self, delta: i32) {
        let n = self.rows().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as i32 + delta).rem_euclid(n as i32) as usize;
    }

    /// Switch pages, resetting the selection and clearing transient state.
    pub fn goto(&mut self, page: MenuPage) {
        self.page = page;
        self.selected = 0;
        self.editing = None;
        self.status = None;
    }

    /// Go up one level. `None` from the root means "close the menu".
    pub fn back(&mut self) -> bool {
        match &self.page {
            MenuPage::Root => false,
            MenuPage::Projects | MenuPage::Settings => {
                self.goto(MenuPage::Root);
                true
            }
            MenuPage::Sessions(_) => {
                self.goto(MenuPage::Projects);
                true
            }
        }
    }

    /// Re-read the session listing and settings from disk.
    pub fn refresh(&mut self, sessions_dir: &Path, project_dir: &Path) {
        self.projects = group_projects(rc_session::list(sessions_dir));
        self.settings = Settings::load(project_dir);
        let n = self.rows().len();
        self.selected = self.selected.min(n.saturating_sub(1));
        self.status = Some("refreshed".into());
    }

    /// The settings field under the cursor, if the settings page is open.
    pub fn current_field(&self) -> Option<&'static FieldSpec> {
        match self.current_row() {
            Some(Row::Field(i)) => EDITABLE.get(i),
            _ => None,
        }
    }

    /// Begin editing the selected field. A `Choice`/`Bool` field has nothing to
    /// type — those cycle with ←/→ — so this only opens a buffer for the typed
    /// kinds.
    pub fn begin_edit(&mut self) {
        let Some(field) = self.current_field() else {
            return;
        };
        if matches!(field.kind, FieldKind::Choice(_) | FieldKind::Bool) {
            return;
        }
        // A model is typed to *add* one, so start from an empty buffer rather
        // than the current name — the common case is entering something new,
        // and pre-filling would mean clearing it first every time.
        self.editing = Some(match field.kind {
            FieldKind::Model => String::new(),
            _ => field.current(&self.settings),
        });
        self.status = None;
    }

    /// Commit `value` to the selected field, writing `~/.sc/settings.json`.
    /// Sets [`Self::status`] either way; reloads settings on success so the
    /// page reflects the new resolved state.
    pub fn commit(&mut self, value: &str, project_dir: &Path) {
        let Some(field) = self.current_field() else {
            return;
        };
        // Typing a model both selects it and remembers it, so the roster grows
        // as you use it instead of needing separate "add" and "switch" steps.
        let written = match field.kind {
            FieldKind::Model => field
                .parse(value)
                .and_then(|_| rc_config::edit::add_model(value, &self.settings)),
            _ => field
                .parse(value)
                .and_then(|v| rc_config::edit::set_user_setting(field, v)),
        };
        match written {
            Ok(path) => {
                self.editing = None;
                self.settings = Settings::load(project_dir);
                // A saved value that an env var outranks would look like a
                // no-op; say so rather than let the user think it took effect.
                self.status = Some(match field.env_override() {
                    Some(_) => format!(
                        "saved to {} — but ${} overrides it in this shell",
                        path.display(),
                        field.env
                    ),
                    None => format!("saved to {}", path.display()),
                });
            }
            Err(e) => self.status = Some(e),
        }
    }

    /// Cycle a `Choice`/`Bool` field and save immediately — there's no text to
    /// confirm, so a keypress is the whole interaction.
    pub fn cycle_current(&mut self, delta: i32, project_dir: &Path) {
        let Some(field) = self.current_field() else {
            return;
        };
        let current = field.current(&self.settings);
        let Some(next) = field.cycle(&current, delta, &self.settings) else {
            // Nothing to cycle to. For a model roster of one that's worth
            // saying, since ←/→ visibly doing nothing reads as a bug.
            if field.kind == FieldKind::Model {
                self.status = Some("only one saved model — press ↵ to add another".into());
            }
            return;
        };
        self.commit(&next, project_dir);
    }

    /// Remove the model under the cursor from the saved roster (the `d` key on
    /// the settings page). A no-op on any other field.
    pub fn remove_current_model(&mut self, project_dir: &Path) {
        let Some(field) = self.current_field() else {
            return;
        };
        if field.kind != FieldKind::Model {
            return;
        }
        let current = field.current(&self.settings);
        match rc_config::edit::remove_model(&current, &self.settings) {
            Ok(_) => {
                self.settings = Settings::load(project_dir);
                self.status = Some(format!("removed {current} from the saved list"));
            }
            Err(e) => self.status = Some(e),
        }
    }
}

/// Group sessions by their working directory, newest project first.
///
/// `BTreeMap` keyed by path gives a deterministic grouping; the final sort is
/// by recency, which is the order a picker wants.
pub(crate) fn group_projects(sessions: Vec<SessionInfo>) -> Vec<Project> {
    let mut by_dir: BTreeMap<PathBuf, Vec<SessionInfo>> = BTreeMap::new();
    for s in sessions {
        by_dir.entry(s.cwd.clone()).or_default().push(s);
    }
    let mut projects: Vec<Project> = by_dir
        .into_iter()
        .map(|(dir, mut sessions)| {
            sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
            let last = sessions
                .first()
                .map(|s| s.modified)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Project {
                dir,
                sessions,
                last,
            }
        })
        .collect();
    projects.sort_by(|a, b| b.last.cmp(&a.last));
    projects
}

/// A compact "how long ago" for a timestamp — the menu's recency column.
/// Deliberately coarse: the useful distinction is "just now" vs "last week",
/// not exact minutes.
pub(crate) fn ago(t: SystemTime, now: SystemTime) -> String {
    let Ok(d) = now.duration_since(t) else {
        return "just now".into();
    };
    let secs = d.as_secs();
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn info(id: &str, cwd: &str, secs: u64, prompt: &str) -> SessionInfo {
        SessionInfo {
            path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            id: id.into(),
            cwd: PathBuf::from(cwd),
            model: "m".into(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            first_prompt: Some(prompt.into()),
        }
    }

    /// Sessions collapse into one entry per directory, newest project first,
    /// and each project's sessions are newest first too.
    #[test]
    fn groups_sessions_by_directory_newest_first() {
        let projects = group_projects(vec![
            info("a", "/work/one", 100, "older one"),
            info("b", "/work/two", 500, "newest overall"),
            info("c", "/work/one", 300, "newer one"),
        ]);

        assert_eq!(projects.len(), 2, "two distinct directories");
        assert_eq!(
            projects[0].dir,
            PathBuf::from("/work/two"),
            "most recent project leads"
        );
        assert_eq!(projects[1].dir, PathBuf::from("/work/one"));
        let ids: Vec<&str> = projects[1].sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["c", "a"],
            "sessions newest first within a project"
        );
    }

    #[test]
    fn project_name_is_the_directory_leaf() {
        let projects = group_projects(vec![info("a", "/home/d/subconscious-code", 1, "x")]);
        assert_eq!(projects[0].name(), "subconscious-code");
    }

    /// Selection wraps at both ends, so holding a direction never dead-ends.
    #[test]
    fn selection_wraps_in_both_directions() {
        let mut m = state_with(vec![info("a", "/w/one", 1, "x")]);
        m.goto(MenuPage::Root);
        let n = m.rows().len();
        assert!(n >= 2);

        m.move_selection(-1);
        assert_eq!(m.selected, n - 1, "up from the top wraps to the bottom");
        m.move_selection(1);
        assert_eq!(m.selected, 0, "and back around");
    }

    /// The sessions page always offers "new session here" after the existing
    /// ones — a project with history must still be startable fresh.
    #[test]
    fn sessions_page_offers_a_new_session_row() {
        let mut m = state_with(vec![
            info("a", "/w/one", 1, "x"),
            info("b", "/w/one", 2, "y"),
        ]);
        m.goto(MenuPage::Sessions(PathBuf::from("/w/one")));

        let rows = m.rows();
        assert_eq!(rows.len(), 3, "two sessions + new: {rows:?}");
        assert_eq!(rows[2], Row::NewSession(PathBuf::from("/w/one")));
    }

    /// Back walks Sessions → Projects → Root, then reports "nothing left to
    /// pop" so the caller closes the menu.
    #[test]
    fn back_walks_up_then_signals_close() {
        let mut m = state_with(vec![info("a", "/w/one", 1, "x")]);
        m.goto(MenuPage::Sessions(PathBuf::from("/w/one")));

        assert!(m.back());
        assert_eq!(m.page, MenuPage::Projects);
        assert!(m.back());
        assert_eq!(m.page, MenuPage::Root);
        assert!(!m.back(), "root has nowhere to go; the caller closes");
    }

    /// Typed fields open an edit buffer; cycled ones don't (←/→ handles those).
    #[test]
    fn begin_edit_only_opens_a_buffer_for_typed_fields() {
        let mut m = state_with(vec![]);
        m.goto(MenuPage::Settings);

        let text_idx = EDITABLE
            .iter()
            .position(|f| f.kind == FieldKind::Text)
            .unwrap();
        m.selected = text_idx;
        m.begin_edit();
        assert!(m.editing.is_some(), "a text field opens an editor");

        let choice_idx = EDITABLE
            .iter()
            .position(|f| matches!(f.kind, FieldKind::Choice(_)))
            .unwrap();
        m.goto(MenuPage::Settings);
        m.selected = choice_idx;
        m.begin_edit();
        assert!(
            m.editing.is_none(),
            "a choice field cycles instead of opening an editor"
        );
    }

    /// Opening the model editor starts empty (you're adding a name, not
    /// correcting one), while other text fields start from their value.
    #[test]
    fn model_editor_starts_empty_but_text_fields_prefill() {
        let mut m = state_with(vec![]);
        m.goto(MenuPage::Settings);

        m.selected = EDITABLE
            .iter()
            .position(|f| f.kind == FieldKind::Model)
            .unwrap();
        m.begin_edit();
        assert_eq!(
            m.editing.as_deref(),
            Some(""),
            "adding a model starts from a blank buffer"
        );

        m.goto(MenuPage::Settings);
        m.selected = EDITABLE
            .iter()
            .position(|f| f.kind == FieldKind::Text)
            .unwrap();
        m.begin_edit();
        assert!(
            m.editing.as_deref().is_some_and(|b| !b.is_empty()),
            "a plain text field prefills its current value for editing"
        );
    }

    /// Cycling a one-entry roster says so rather than silently doing nothing —
    /// a dead ←/→ reads as a bug.
    #[test]
    fn cycling_a_lone_model_explains_itself() {
        let mut m = state_with(vec![]);
        m.goto(MenuPage::Settings);
        m.selected = EDITABLE
            .iter()
            .position(|f| f.kind == FieldKind::Model)
            .unwrap();
        m.settings.models = vec![m.settings.model.clone()];

        m.cycle_current(1, Path::new("/nonexistent-project-dir"));
        assert!(
            m.status
                .as_deref()
                .is_some_and(|s| s.contains("only one saved model")),
            "expected an explanation, got {:?}",
            m.status
        );
    }

    /// `d` only means "remove a model" on the model row; on any other field it
    /// must not touch settings.
    #[test]
    fn remove_model_is_a_noop_on_other_fields() {
        let mut m = state_with(vec![]);
        m.goto(MenuPage::Settings);
        m.selected = EDITABLE
            .iter()
            .position(|f| f.kind == FieldKind::Number)
            .unwrap();

        m.remove_current_model(Path::new("/nonexistent-project-dir"));
        assert!(
            m.status.is_none(),
            "no action on a non-model field, got {:?}",
            m.status
        );
    }

    #[test]
    fn ago_reads_coarsely() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(ago(base, base), "just now");
        assert_eq!(ago(base, base + Duration::from_secs(120)), "2m ago");
        assert_eq!(ago(base, base + Duration::from_secs(7200)), "2h ago");
        assert_eq!(ago(base, base + Duration::from_secs(172_800)), "2d ago");
        // A file dated in the future (clock skew) must not panic.
        assert_eq!(ago(base + Duration::from_secs(60), base), "just now");
    }

    /// Build a menu without touching the real `~/.sc` — `MenuState::new` reads
    /// the disk, which a unit test must not depend on.
    fn state_with(sessions: Vec<SessionInfo>) -> MenuState {
        MenuState {
            page: MenuPage::Root,
            selected: 0,
            projects: group_projects(sessions),
            settings: Settings::load(Path::new("/nonexistent-project-dir")),
            editing: None,
            status: None,
        }
    }
}
