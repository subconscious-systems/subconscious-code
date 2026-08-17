//! Editing `~/.sc/settings.json` from the TUI's `/menu` settings page.
//!
//! Two rules shape this module.
//!
//! **Only file-backed fields are editable.** [`Settings::load`] reads some
//! values (`max_tokens`, `temperature`) *only* from the environment and CLI
//! flags — they have no `settings.json` key at all. Offering those in an
//! editor would write a key the loader never reads: the UI would show a saved
//! value and the behavior would never change. [`EDITABLE`] therefore lists
//! exactly the fields [`SettingsFile`](crate::SettingsFile) actually parses.
//!
//! **Env vars shadow the file.** Per §10.1 precedence is defaults → user file
//! → project file → env → flags, so `SC_BASE_URL` in the environment silently
//! beats anything written here. [`FieldSpec::env_override`] reports that so the
//! page can warn instead of letting the user "save" a value that does nothing.
//!
//! Writes go through a `serde_json::Value` tree rather than serializing a
//! typed struct, so keys this build doesn't know about — a newer sc's settings,
//! a hand-written comment-free block — survive a save untouched.

use std::path::{Path, PathBuf};

use crate::Settings;

/// How a field's value is entered, so the page can pick an editing affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text (a model name, a URL).
    Text,
    /// One of a fixed set, cycled with ←/→ rather than typed.
    Choice(&'static [&'static str]),
    /// The active model: typed like [`FieldKind::Text`], but cycled with ←/→
    /// through the saved roster ([`Settings::models`]) rather than a fixed
    /// list, and typing a new name *adds* it to that roster. The options are
    /// user data, so they can't live in a `&'static` like [`Self::Choice`].
    Model,
    /// A non-negative integer. `0` conventionally means "unlimited"/"off"
    /// across this config — see each field's `help`.
    Number,
    /// A toggle.
    Bool,
}

/// One editable setting: where it lives in the JSON, what shadows it, and how
/// to edit it.
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    /// Display name and stable identity, e.g. `"base_url"`.
    pub name: &'static str,
    /// Key path into `settings.json`, e.g. `["provider", "base_url"]`.
    pub path: &'static [&'static str],
    /// The environment variable that overrides this field, if set.
    pub env: &'static str,
    pub kind: FieldKind,
    /// One-line explanation shown under the selected row.
    pub help: &'static str,
}

/// Ordered by how much they let through, matching the TUI's Shift+Tab cycle.
/// `bypassPermissions` is still *accepted* by `Mode::parse` for existing
/// config files, but isn't offered here — `auto` is the spelling now.
const MODES: &[&str] = &["ask", "default", "acceptEdits", "plan", "auto"];

/// The settings `/menu` can edit — every one a key
/// [`SettingsFile`](crate::SettingsFile) actually reads back.
pub const EDITABLE: &[FieldSpec] = &[
    FieldSpec {
        name: "model",
        path: &["model"],
        env: "SC_MODEL",
        kind: FieldKind::Model,
        help: "Model for the agent loop. ←→ switches saved models, ↵ adds one, d removes.",
    },
    FieldSpec {
        name: "small_model",
        path: &["small_model"],
        env: "SC_SMALL_MODEL",
        kind: FieldKind::Text,
        help: "Model for cheap side tasks.",
    },
    FieldSpec {
        name: "base_url",
        path: &["provider", "base_url"],
        env: "SC_BASE_URL",
        kind: FieldKind::Text,
        help: "OpenAI-compatible endpoint; /chat/completions is appended.",
    },
    FieldSpec {
        name: "default_mode",
        path: &["permissions", "default_mode"],
        env: "SC_DEFAULT_MODE",
        kind: FieldKind::Choice(MODES),
        help: "Permission mode each session starts in.",
    },
    FieldSpec {
        name: "idle_timeout_ms",
        path: &["provider", "idle_timeout_ms"],
        env: "SC_IDLE_TIMEOUT_MS",
        kind: FieldKind::Number,
        help: "Max gap between stream chunks before aborting (0 = off).",
    },
    FieldSpec {
        name: "max_retries",
        path: &["provider", "max_retries"],
        env: "SC_MAX_RETRIES",
        kind: FieldKind::Number,
        help: "Retries on transient 429/5xx (0 = off).",
    },
    FieldSpec {
        name: "request_gzip",
        path: &["provider", "request_gzip"],
        env: "SC_REQUEST_GZIP",
        kind: FieldKind::Bool,
        help: "gzip the request body; the gateway must honor Content-Encoding.",
    },
    FieldSpec {
        name: "tool_result_cap",
        path: &["context", "tool_result_cap"],
        env: "SC_TOOL_RESULT_CAP",
        kind: FieldKind::Number,
        help: "Bytes of a tool result kept in context (0 = unlimited).",
    },
    FieldSpec {
        name: "read_default_limit",
        path: &["context", "read_default_limit"],
        env: "SC_READ_DEFAULT_LIMIT",
        kind: FieldKind::Number,
        help: "Lines Read returns without an explicit limit (0 = whole file).",
    },
    FieldSpec {
        name: "max_iters",
        path: &["context", "max_iters"],
        env: "SC_MAX_ITERS",
        kind: FieldKind::Number,
        help: "Tool-loop iterations per turn; a runaway backstop.",
    },
];

impl FieldSpec {
    /// This field's currently resolved value, rendered for display. Read from
    /// the fully-resolved [`Settings`] rather than the file, so what the page
    /// shows is what the agent is actually using.
    pub fn current(&self, s: &Settings) -> String {
        match self.name {
            "model" => s.model.clone(),
            "small_model" => s.small_model.clone(),
            "base_url" => s.base_url.clone(),
            // An unset mode resolves to `default`; showing the raw empty
            // string would read as "no value" when the effective mode is
            // `default`, and would leave ←/→ with nothing to cycle from.
            "default_mode" => match s.permissions.default_mode.as_str() {
                "" => "default".to_string(),
                m => m.to_string(),
            },
            "idle_timeout_ms" => s.idle_timeout_ms.to_string(),
            "max_retries" => s.max_retries.to_string(),
            "request_gzip" => s.request_gzip.to_string(),
            "tool_result_cap" => s.context.tool_result_cap.to_string(),
            "read_default_limit" => s.context.read_default_limit.to_string(),
            "max_iters" => s.context.max_iters.to_string(),
            _ => String::new(),
        }
    }

    /// The environment value currently shadowing this field, if any. `Some`
    /// means a save writes the file but changes nothing until the variable is
    /// unset — the page says so rather than pretending the edit took effect.
    pub fn env_override(&self) -> Option<String> {
        std::env::var(self.env).ok().filter(|v| !v.is_empty())
    }

    /// Validate raw input for this field, returning the JSON value to store.
    /// `Err` carries a message fit to show under the editor.
    pub fn parse(&self, raw: &str) -> Result<serde_json::Value, String> {
        let raw = raw.trim();
        match self.kind {
            FieldKind::Text | FieldKind::Model => {
                if raw.is_empty() {
                    return Err(format!("{} cannot be empty", self.name));
                }
                Ok(serde_json::Value::String(raw.to_string()))
            }
            FieldKind::Choice(options) => options
                .iter()
                .find(|o| o.eq_ignore_ascii_case(raw))
                .map(|o| serde_json::Value::String((*o).to_string()))
                .ok_or_else(|| format!("must be one of: {}", options.join(", "))),
            FieldKind::Number => raw
                .parse::<u64>()
                .map(|n| serde_json::Value::Number(n.into()))
                .map_err(|_| "must be a non-negative whole number".to_string()),
            FieldKind::Bool => match raw {
                "true" => Ok(serde_json::Value::Bool(true)),
                "false" => Ok(serde_json::Value::Bool(false)),
                _ => Err("must be true or false".to_string()),
            },
        }
    }

    /// The next value when cycling with ←/→. `delta` is +1 or -1. `Text` is
    /// typed rather than cycled, so it yields `None`; `Model` cycles the saved
    /// roster from `settings`, which is why this needs them.
    pub fn cycle(&self, current: &str, delta: i32, settings: &Settings) -> Option<String> {
        match self.kind {
            FieldKind::Choice(options) => Some(step(options, current, delta)?.to_string()),
            FieldKind::Model => {
                let roster: Vec<&str> = settings.models.iter().map(String::as_str).collect();
                Some(step(&roster, current, delta)?.to_string())
            }
            FieldKind::Bool => Some(if current == "true" { "false" } else { "true" }.to_string()),
            FieldKind::Text | FieldKind::Number => None,
        }
    }
}

/// Step `delta` places through `options`, wrapping. `None` when there's
/// nothing to cycle, so a one-entry roster reports "no other option" rather
/// than pretending to change.
fn step<'a>(options: &[&'a str], current: &str, delta: i32) -> Option<&'a str> {
    if options.len() < 2 {
        return None;
    }
    let i = options.iter().position(|o| *o == current).unwrap_or(0) as i32;
    let n = options.len() as i32;
    Some(options[(i + delta).rem_euclid(n) as usize])
}

/// Add `name` to the saved model roster and make it the active model, writing
/// both keys to `~/.sc/settings.json`. Adding a model the roster already has
/// just selects it, so re-typing an existing name can't duplicate it.
pub fn add_model(name: &str, settings: &Settings) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("model cannot be empty".into());
    }
    let mut roster = settings.models.clone();
    if !roster.iter().any(|m| m == name) {
        roster.push(name.to_string());
    }
    write_model_keys(name, &roster)
}

/// Drop `name` from the saved roster. The active model is left alone even when
/// it's the one removed — silently switching the model out from under a
/// running session would be a surprising side effect of tidying a list. The
/// last entry can't be removed, since an empty roster leaves ←/→ dead.
pub fn remove_model(name: &str, settings: &Settings) -> Result<PathBuf, String> {
    if settings.models.len() < 2 {
        return Err("can't remove the only saved model".into());
    }
    let roster: Vec<String> = settings
        .models
        .iter()
        .filter(|m| *m != name)
        .cloned()
        .collect();
    if roster.len() == settings.models.len() {
        return Err(format!("{name} is not in the saved list"));
    }
    write_model_keys(&settings.model, &roster)
}

/// Write `model` + `models` together, so the active model and the roster can
/// never disagree on disk.
fn write_model_keys(model: &str, roster: &[String]) -> Result<PathBuf, String> {
    let model_spec = EDITABLE
        .iter()
        .find(|f| f.name == "model")
        .expect("model field exists");
    set_user_setting(model_spec, serde_json::Value::String(model.to_string()))?;
    set_raw(&["models"], serde_json::json!(roster))
}

/// `~/.sc/settings.json` — the file the settings page writes. The same path
/// [`Settings::load`] reads as its user layer.
pub fn user_settings_file() -> Option<PathBuf> {
    crate::user_settings_path()
}

/// Write `value` at `spec`'s key path in `~/.sc/settings.json`, creating the
/// file (and any missing parent objects) as needed, and return the path
/// written.
///
/// The existing file is edited as a JSON tree, so unrecognized keys survive.
/// A file that exists but doesn't parse is an error rather than something to
/// overwrite — clobbering a config the user hand-wrote (and losing whatever
/// was in it) is worse than refusing to save.
pub fn set_user_setting(spec: &FieldSpec, value: serde_json::Value) -> Result<PathBuf, String> {
    set_raw(spec.path, value)
}

/// [`set_user_setting`] by key path, for keys with no [`FieldSpec`] of their
/// own (the `models` roster).
fn set_raw(key_path: &[&str], value: serde_json::Value) -> Result<PathBuf, String> {
    let path = user_settings_file().ok_or("HOME is not set; cannot locate ~/.sc/settings.json")?;
    let mut root = read_tree(&path)?;

    // Walk the key path, creating intermediate objects, and set the leaf.
    let mut node = &mut root;
    let (leaf, parents) = key_path.split_last().expect("every field has a key path");
    for key in parents {
        node = node
            .as_object_mut()
            .ok_or_else(|| format!("{}: `{key}` is not an object", path.display()))?
            .entry(*key)
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
    }
    node.as_object_mut()
        .ok_or_else(|| format!("{}: `{leaf}`'s parent is not an object", path.display()))?
        .insert((*leaf).to_string(), value);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let text =
        serde_json::to_string_pretty(&root).map_err(|e| format!("serializing settings: {e}"))?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

/// The settings file as a JSON tree: an absent file is an empty object (the
/// first save creates it), a present-but-unparseable one is an error.
fn read_tree(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::Value::Object(Default::default()))
        }
        Err(e) => Err(format!("reading {}: {e}", path.display())),
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => {
            Ok(serde_json::Value::Object(Default::default()))
        }
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}); fix it by hand first",
                path.display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> &'static FieldSpec {
        EDITABLE
            .iter()
            .find(|f| f.name == name)
            .expect("field exists")
    }

    /// Every editable field must correspond to a key the loader actually reads
    /// back from `settings.json`. This is the module's core invariant: an
    /// entry here whose key the loader ignores would save silently and never
    /// take effect.
    #[test]
    fn every_editable_field_has_a_known_key_path() {
        let known: &[&[&str]] = &[
            &["model"],
            &["small_model"],
            &["provider", "base_url"],
            &["provider", "idle_timeout_ms"],
            &["provider", "max_retries"],
            &["provider", "request_gzip"],
            &["permissions", "default_mode"],
            &["context", "tool_result_cap"],
            &["context", "read_default_limit"],
            &["context", "max_iters"],
        ];
        for f in EDITABLE {
            assert!(
                known.contains(&f.path),
                "{} writes {:?}, which SettingsFile does not parse",
                f.name,
                f.path
            );
        }
    }

    #[test]
    fn number_field_rejects_junk_and_accepts_zero() {
        let s = spec("max_retries");
        assert!(s.parse("abc").is_err());
        assert!(s.parse("-1").is_err());
        assert_eq!(s.parse("0").unwrap(), serde_json::json!(0));
        assert_eq!(s.parse(" 7 ").unwrap(), serde_json::json!(7));
    }

    #[test]
    fn text_field_rejects_empty() {
        assert!(spec("model").parse("   ").is_err());
        assert_eq!(
            spec("model").parse("gw-glm-5.2").unwrap(),
            serde_json::json!("gw-glm-5.2")
        );
    }

    /// A choice field only accepts its listed options, case-insensitively, and
    /// normalizes to the canonical spelling the loader expects.
    #[test]
    fn choice_field_validates_and_normalizes() {
        let s = spec("default_mode");
        assert!(s.parse("nonsense").is_err());
        assert_eq!(
            s.parse("acceptedits").unwrap(),
            serde_json::json!("acceptEdits")
        );
    }

    /// Settings with a given model roster, for the picker tests.
    fn settings_with_models(model: &str, models: &[&str]) -> Settings {
        let mut s = Settings::load(Path::new("/nonexistent-project-dir"));
        s.model = model.to_string();
        s.models = models.iter().map(|m| m.to_string()).collect();
        s
    }

    #[test]
    fn choice_and_bool_cycle_both_directions() {
        let s = settings_with_models("a", &["a"]);
        let mode = spec("default_mode");
        assert_eq!(mode.cycle("default", 1, &s).as_deref(), Some("acceptEdits"));
        // Wraps around rather than sticking at the ends. `ask` sits one step
        // *below* `default` (it confirms more, not less).
        assert_eq!(mode.cycle("default", -1, &s).as_deref(), Some("ask"));
        assert_eq!(
            mode.cycle("auto", 1, &s).as_deref(),
            Some("ask"),
            "wraps past the end"
        );
        assert_eq!(
            spec("request_gzip").cycle("false", 1, &s).as_deref(),
            Some("true")
        );
        // A free-text field has nothing to cycle.
        assert_eq!(spec("small_model").cycle("x", 1, &s), None);
    }

    /// The model field cycles the *saved roster*, wrapping in both directions
    /// — this is what ←/→ does on the settings page.
    #[test]
    fn model_cycles_through_the_saved_roster() {
        let s = settings_with_models("one", &["one", "two", "three"]);
        let model = spec("model");
        assert_eq!(model.cycle("one", 1, &s).as_deref(), Some("two"));
        assert_eq!(
            model.cycle("three", 1, &s).as_deref(),
            Some("one"),
            "wraps forward"
        );
        assert_eq!(
            model.cycle("one", -1, &s).as_deref(),
            Some("three"),
            "wraps backward"
        );
    }

    /// With one saved model there is nothing to switch to, and the UI should
    /// be told that rather than shown a no-op "change".
    #[test]
    fn model_with_a_single_entry_does_not_cycle() {
        let s = settings_with_models("only", &["only"]);
        assert_eq!(spec("model").cycle("only", 1, &s), None);
    }

    /// A model not in the roster (set via `SC_MODEL`, say) still cycles rather
    /// than dead-ending — it just starts from the list's head.
    #[test]
    fn model_not_in_the_roster_still_cycles() {
        let s = settings_with_models("from-env", &["a", "b"]);
        assert_eq!(spec("model").cycle("from-env", 1, &s).as_deref(), Some("b"));
    }

    /// A save must not drop keys this build doesn't know about, nor siblings
    /// under the same parent object.
    #[test]
    fn writing_preserves_unknown_keys_and_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"provider":{"base_url":"https://old","api_key_env":"CUSTOM_KEY"},"future_key":42}"#,
        )
        .unwrap();

        // Exercise the tree edit directly (the public entry point resolves
        // HOME, which a test must not depend on).
        let mut root = read_tree(&path).unwrap();
        root.as_object_mut()
            .unwrap()
            .get_mut("provider")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("base_url".into(), serde_json::json!("https://new"));
        std::fs::write(&path, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        let back: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["provider"]["base_url"], "https://new");
        assert_eq!(
            back["provider"]["api_key_env"], "CUSTOM_KEY",
            "sibling survived"
        );
        assert_eq!(back["future_key"], 42, "unknown key survived");
    }

    /// An absent or blank file starts from an empty object; a malformed one
    /// refuses rather than clobbering whatever the user wrote.
    #[test]
    fn read_tree_handles_missing_blank_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_tree(&dir.path().join("nope.json"))
            .unwrap()
            .is_object());

        let blank = dir.path().join("blank.json");
        std::fs::write(&blank, "  \n").unwrap();
        assert!(read_tree(&blank).unwrap().is_object());

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert!(
            read_tree(&bad).is_err(),
            "malformed file must not be silently overwritten"
        );
    }
}
