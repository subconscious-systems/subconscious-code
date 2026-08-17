//! Layered settings (§10).
//!
//! M0 implemented a useful subset of the §10.1 precedence stack:
//!   compiled defaults → user (`~/.sc/settings.json`) → project
//!   (`./.sc/settings.json`) → env vars. CLI flags (applied in rc-cli) override
//!   on top. Enterprise/locked settings, JSON Schema validation, and hot-reload
//!   land in later milestones (G1/G4/G5).
//!
//! The API key is never stored in a settings file — it is resolved from the
//! env var named by `provider.api_key_env` (default `SC_API_KEY`). `sc doctor`
//! (G7) will complain loudly if a key-shaped string appears in any file.
//!
//! M3 adds the `permissions` block (§7.1/§10.2): allow/ask/deny rule lists,
//! `defaultMode`, and `additionalDirectories`.
//!
//! M8 adds the `context` block: the per-item truncation caps. Subconscious Code
//! ships them **unlimited** by default — that is the product. They exist so a
//! user on a small-context model can dial them back in.

use std::path::{Path, PathBuf};

pub mod edit;

/// Resolved settings ready to drive a chat client + the permission engine.
#[derive(Debug, Clone)]
pub struct Settings {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    /// Model names saved to switch between, from `settings.json`'s `models`
    /// array. A roster for the UI, not a behavior setting: nothing in the
    /// agent reads it, and [`Settings::model`] is always the one in use.
    /// Guaranteed to contain [`Settings::model`] — the loader appends it if the
    /// file's list omits it, so a picker always has a current entry.
    pub models: Vec<String>,
    pub small_model: String,
    /// Total request timeout in ms (0 = **off**). Env `SC_TIMEOUT_MS`. Off by
    /// default: a total budget covers the upload, so on a huge request it can
    /// expire mid-upload and trigger a pointless retry. Liveness is enforced by
    /// `idle_timeout_ms` instead.
    pub timeout_ms: u64,
    /// T2: max gap between stream chunks before the turn aborts (0 = off). Env
    /// `SC_IDLE_TIMEOUT_MS`. This is the real liveness check now that
    /// `timeout_ms` defaults off.
    pub idle_timeout_ms: u64,
    /// Retry: max retries on transient HTTP errors 429/5xx (0 = off). Env
    /// `SC_MAX_RETRIES`.
    pub max_retries: u32,
    /// Retry: base backoff (ms) for the first retry; doubles each attempt. Env
    /// `SC_RETRY_BASE_MS`.
    pub retry_base_ms: u64,
    /// Retry: cap (ms) on the backoff between retries. Env `SC_RETRY_MAX_MS`.
    pub retry_max_ms: u64,
    /// T3: wall-clock budget for a turn in ms (0 = off). Env `SC_TURN_TIMEOUT_MS`.
    pub turn_timeout_ms: u64,
    /// M4: per-response completion-token cap (0 = provider default). Env
    /// `SC_MAX_TOKENS`. Bounds each reply's length.
    pub max_tokens: u32,
    /// Sampling temperature (None = provider default). Env `SC_TEMPERATURE`.
    pub temperature: Option<f32>,
    pub permissions: PermissionsConfig,
    /// M7: opt-in kernel sandbox for `Bash` (§7.6). Off by default.
    pub sandbox: SandboxConfig,
    /// M8: per-item context caps. Unlimited by default.
    pub context: ContextConfig,
    /// M8: gzip the request body (`Content-Encoding: gzip`). Off by default —
    /// the gateway must advertise support. Env `SC_REQUEST_GZIP`.
    pub request_gzip: bool,
}

/// The `permissions` block (§7.1/§10.2). Rule strings are parsed by rc-perm.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    /// "default" | "acceptEdits" | "plan" | "bypassPermissions".
    pub default_mode: String,
    /// Extra directories the agent may touch, beyond the cwd.
    pub additional_directories: Vec<String>,
}

/// The `sandbox` block (§7.6). Opt-in kernel confinement for `Bash`: deny
/// writes outside the workspace roots (+ allow `/tmp`) and, unless
/// `allow_net`, deny network syscalls. Linux applies Landlock+seccomp; other
/// platforms no-op. Off by default so `cargo`/`npm`/`git` keep working.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Enable confinement for every approved `Bash` command. Env `SC_SANDBOX`.
    pub enabled: bool,
    /// Allow network syscalls under confinement. Env `SC_SANDBOX_NET`.
    pub allow_net: bool,
}

/// The `context` block: every per-item truncation cap in the harness, in bytes
/// (or lines/paths where noted). **`0` means unlimited** everywhere.
///
/// Subconscious Code's thesis is that the model can take the whole thing, so
/// the shipped defaults are all unlimited. The truncation code paths remain in
/// place behind the `0` check, both so a small-context model can be served and
/// so the §8.5 microcompaction seam stays available for future strategies.
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Bytes of an `@file` mention inlined into the prompt (0 = whole file).
    pub inline_file_cap: usize,
    /// Bytes of a tool-result body kept in the conversation (0 = all of it).
    pub tool_result_cap: usize,
    /// Bytes of combined `Bash` stdout+stderr kept (0 = all of it).
    pub bash_output_cap: usize,
    /// Bytes of `Grep` output kept (0 = all of it).
    pub grep_output_cap: usize,
    /// Lines `Read` returns when the call omits `limit` (0 = the whole file).
    pub read_default_limit: u32,
    /// Max chars of a single line before `Read` elides the middle (0 = never).
    pub read_max_line_chars: usize,
    /// Max paths `Glob` returns (0 = all matches).
    pub glob_cap: usize,
    /// Tool-loop iterations allowed in one turn. Not a context limit — a
    /// runaway backstop. 0 = unlimited (not recommended).
    pub max_iters: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            inline_file_cap: 0,
            tool_result_cap: 0,
            bash_output_cap: 0,
            grep_output_cap: 0,
            read_default_limit: 0,
            read_max_line_chars: 0,
            glob_cap: 0,
            // The one deliberate non-zero default: unlimited iterations turns a
            // confused model into an unbounded spend. 1000 is far above any
            // legitimate task and still terminates.
            max_iters: 1000,
        }
    }
}

/// On-disk shape of a settings file. Unknown keys are ignored (forward-compat).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct SettingsFile {
    provider: Option<ProviderFile>,
    model: Option<String>,
    /// Saved model names to switch between (the `/menu` settings page adds to
    /// this list). Purely a convenience roster — the model actually used is
    /// `model`.
    models: Option<Vec<String>>,
    small_model: Option<String>,
    permissions: Option<PermissionsConfig>,
    sandbox: Option<SandboxConfig>,
    context: Option<ContextConfig>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ProviderFile {
    base_url: Option<String>,
    api_key_env: Option<String>,
    timeout_ms: Option<u64>,
    idle_timeout_ms: Option<u64>,
    max_retries: Option<u32>,
    request_gzip: Option<bool>,
}

// Defaults from §10.2. All overridable via env (§5.6 G3) or settings files.
const DEFAULT_BASE_URL: &str = "https://api-dev.subconscious.dev/v1";
const DEFAULT_MODEL: &str = "subconscious/glm-5.2";
const DEFAULT_SMALL_MODEL: &str = "subconscious/glm-5.2";
/// Off: see [`Settings::timeout_ms`].
const DEFAULT_TIMEOUT_MS: u64 = 0;
/// The liveness backstop that replaces the total timeout. Two minutes with no
/// stream chunk means something is wrong.
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 120_000;
/// Transient 429/5xx happen; the body is a refcounted `Bytes` so a retry is
/// cheap even on a huge request.
const DEFAULT_MAX_RETRIES: u32 = 2;

/// Non-fatal problems found while loading settings: a malformed
/// `settings.json` parse error, or a secret-shaped string in one. `Settings::load`
/// drops these; `Settings::load_with_report` collects them so `sc doctor` can
/// surface them instead of silently ignoring a typo'd config.
#[derive(Debug, Default, Clone)]
pub struct LoadReport {
    /// Human-readable warnings (parse errors, secret-scan hits), in load order.
    pub warnings: Vec<String>,
}

impl LoadReport {
    /// No warnings were recorded.
    pub fn is_empty(&self) -> bool {
        self.warnings.is_empty()
    }
}

impl Settings {
    /// Load settings with M0 precedence: defaults → user → project → env.
    /// Parse errors are reported via the returned [`LoadReport`] rather than
    /// failing the whole load — a typo in `~/.sc/settings.json` shouldn't make
    /// `sc` unusable, but it should be visible (e.g. in `sc doctor`).
    pub fn load(project_dir: &Path) -> Self {
        let mut report = LoadReport::default();
        let s = Self::load_with_report(project_dir, &mut report);
        // The report is dropped here; `sc doctor` calls `load_with_report`
        // directly to surface warnings. A normal `load` just proceeds.
        let _ = report;
        s
    }

    /// Load settings and capture any file parse errors / secret-scan hits into
    /// `report`. Used by `sc doctor` to surface a malformed `settings.json`
    /// instead of silently ignoring it.
    pub fn load_with_report(project_dir: &Path, report: &mut LoadReport) -> Self {
        let mut base_url = DEFAULT_BASE_URL.to_string();
        let mut api_key_env = "SC_API_KEY".to_string();
        let mut timeout_ms = DEFAULT_TIMEOUT_MS;
        let mut idle_timeout_ms: u64 = DEFAULT_IDLE_TIMEOUT_MS;
        let mut max_retries: u32 = DEFAULT_MAX_RETRIES;
        let mut retry_base_ms: u64 = 200;
        let mut retry_max_ms: u64 = 10_000;
        let mut turn_timeout_ms: u64 = 0;
        let mut max_tokens: u32 = 0;
        let mut temperature: Option<f32> = None;
        let mut model = DEFAULT_MODEL.to_string();
        let mut models: Vec<String> = Vec::new();
        let mut small_model = DEFAULT_SMALL_MODEL.to_string();
        let mut permissions = PermissionsConfig::default();
        let mut sandbox = SandboxConfig::default();
        let mut context = ContextConfig::default();
        let mut request_gzip = false;

        // Later layers override earlier ones. User before project so a
        // committed project file beats a user global — matches §10.1 (project
        // is higher precedence than user).
        let layers: Vec<Option<PathBuf>> =
            vec![user_settings_path(), project_settings_path(project_dir)];
        for path in layers.into_iter().flatten() {
            match read_settings(&path) {
                Ok(file) => {
                    if let Some(warning) = scan_for_secret(&path) {
                        report.warnings.push(warning);
                    }
                    if let Some(p) = file.provider {
                        if let Some(u) = p.base_url {
                            base_url = u;
                        }
                        if let Some(e) = p.api_key_env {
                            api_key_env = e;
                        }
                        if let Some(t) = p.timeout_ms {
                            timeout_ms = t;
                        }
                        if let Some(t) = p.idle_timeout_ms {
                            idle_timeout_ms = t;
                        }
                        if let Some(r) = p.max_retries {
                            max_retries = r;
                        }
                        if let Some(g) = p.request_gzip {
                            request_gzip = g;
                        }
                    }
                    if let Some(m) = file.model {
                        model = m;
                    }
                    if let Some(m) = file.models {
                        models = m;
                    }
                    if let Some(s) = file.small_model {
                        small_model = s;
                    }
                    if let Some(p) = file.permissions {
                        permissions = p;
                    }
                    if let Some(s) = file.sandbox {
                        sandbox = s;
                    }
                    if let Some(c) = file.context {
                        context = c;
                    }
                }
                Err(e) => report.warnings.push(e),
            }
        }

        // Env wins over files (§10.1).
        if let Ok(v) = std::env::var("SC_BASE_URL") {
            if !v.is_empty() {
                base_url = v;
            }
        }
        if let Ok(v) = std::env::var("SC_MODEL") {
            if !v.is_empty() {
                model = v;
            }
        }
        if let Ok(v) = std::env::var("SC_SMALL_MODEL") {
            if !v.is_empty() {
                small_model = v;
            }
        }
        if let Ok(v) = std::env::var("SC_TIMEOUT_MS") {
            if let Ok(t) = v.parse() {
                timeout_ms = t;
            }
        }
        if let Ok(v) = std::env::var("SC_IDLE_TIMEOUT_MS") {
            if let Ok(t) = v.parse::<u64>() {
                idle_timeout_ms = t;
            }
        }
        if let Ok(v) = std::env::var("SC_MAX_RETRIES") {
            if let Ok(t) = v.parse::<u32>() {
                max_retries = t;
            }
        }
        if let Ok(v) = std::env::var("SC_RETRY_BASE_MS") {
            if let Ok(t) = v.parse::<u64>() {
                retry_base_ms = t;
            }
        }
        if let Ok(v) = std::env::var("SC_RETRY_MAX_MS") {
            if let Ok(t) = v.parse::<u64>() {
                retry_max_ms = t;
            }
        }
        if let Ok(v) = std::env::var("SC_TURN_TIMEOUT_MS") {
            if let Ok(t) = v.parse::<u64>() {
                turn_timeout_ms = t;
            }
        }
        if let Ok(v) = std::env::var("SC_MAX_TOKENS") {
            if let Ok(t) = v.parse::<u32>() {
                max_tokens = t;
            }
        }
        if let Ok(v) = std::env::var("SC_TEMPERATURE") {
            if let Ok(t) = v.parse::<f32>() {
                temperature = Some(t);
            }
        }
        if let Ok(v) = std::env::var("SC_DEFAULT_MODE") {
            if !v.is_empty() {
                permissions.default_mode = v;
            }
        }
        if let Some(b) = env_bool("SC_SANDBOX") {
            sandbox.enabled = b;
        }
        if let Some(b) = env_bool("SC_SANDBOX_NET") {
            sandbox.allow_net = b;
        }
        if let Some(b) = env_bool("SC_REQUEST_GZIP") {
            request_gzip = b;
        }
        // Context caps: `0` = unlimited, so an explicit `SC_TOOL_RESULT_CAP=0`
        // is a meaningful value and parses like any other.
        if let Ok(v) = std::env::var("SC_INLINE_FILE_CAP") {
            if let Ok(t) = v.parse::<usize>() {
                context.inline_file_cap = t;
            }
        }
        if let Ok(v) = std::env::var("SC_TOOL_RESULT_CAP") {
            if let Ok(t) = v.parse::<usize>() {
                context.tool_result_cap = t;
            }
        }
        if let Ok(v) = std::env::var("SC_BASH_OUTPUT_CAP") {
            if let Ok(t) = v.parse::<usize>() {
                context.bash_output_cap = t;
            }
        }
        if let Ok(v) = std::env::var("SC_GREP_OUTPUT_CAP") {
            if let Ok(t) = v.parse::<usize>() {
                context.grep_output_cap = t;
            }
        }
        if let Ok(v) = std::env::var("SC_READ_DEFAULT_LIMIT") {
            if let Ok(t) = v.parse::<u32>() {
                context.read_default_limit = t;
            }
        }
        if let Ok(v) = std::env::var("SC_READ_MAX_LINE_CHARS") {
            if let Ok(t) = v.parse::<usize>() {
                context.read_max_line_chars = t;
            }
        }
        if let Ok(v) = std::env::var("SC_GLOB_CAP") {
            if let Ok(t) = v.parse::<usize>() {
                context.glob_cap = t;
            }
        }
        if let Ok(v) = std::env::var("SC_MAX_ITERS") {
            if let Ok(t) = v.parse::<u32>() {
                context.max_iters = t;
            }
        }

        let api_key = std::env::var(&api_key_env).ok().filter(|s| !s.is_empty());

        Settings {
            base_url,
            // The roster always contains the model in use, whatever set it
            // (file, `SC_MODEL`, or `--model`). Otherwise the settings page
            // would show a current model that isn't in the list it cycles
            // through, and one press of ←/→ would jump somewhere unrelated.
            models: {
                models.retain(|m| !m.is_empty());
                models.dedup();
                if !models.contains(&model) {
                    models.insert(0, model.clone());
                }
                models
            },
            api_key,
            model,
            small_model,
            timeout_ms,
            idle_timeout_ms,
            max_retries,
            retry_base_ms,
            retry_max_ms,
            turn_timeout_ms,
            max_tokens,
            temperature,
            permissions,
            sandbox,
            context,
            request_gzip,
        }
    }
}

/// Parse a bool env var: `1`/`true`/`yes`/on (case-insensitive) → true, `0`/`false`/`no`/off → false.
fn env_bool(name: &str) -> Option<bool> {
    let v = std::env::var(name).ok()?;
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `~/.sc/` — the user-global config directory.
pub fn user_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".sc"))
}

fn user_settings_path() -> Option<PathBuf> {
    user_dir().map(|d| d.join("settings.json"))
}

fn project_settings_path(project: &Path) -> Option<PathBuf> {
    Some(project.join(".sc").join("settings.json"))
}

/// Read + parse a settings file. M0 failed soft (a malformed or absent file
/// was silently ignored); G4 now reports the parse error so a typo in
/// `settings.json` isn't a silent no-op. Returns the parsed file on success,
/// or `Err(message)` on a read/parse failure so the caller can surface it.
pub(crate) fn read_settings(path: &Path) -> Result<SettingsFile, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice::<SettingsFile>(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Scan a settings file's raw text for a key-shaped string. The API key must
/// come from the env var named by `provider.api_key_env` (default
/// `SC_API_KEY`); a literal key in the file is a secret leak. Returns the first
/// suspicious line, if any.
///
/// This is the G7 promise from the module doc: `sc doctor` complains loudly if
/// a key-shaped string appears in any settings file. "Key-shaped" is heuristic
/// — a long base64-ish token in a `provider.api_key` / `api_key` / `key` field,
/// or a `Bearer`-prefixed string.
pub fn scan_for_secret(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for (i, line) in text.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        // A field literally named like a key, holding something long.
        let looks_like_key_field = lower.contains("\"api_key\"")
            || lower.contains("\"apikey\"")
            || lower.contains("\"key\"")
            || lower.contains("\"token\"")
            || lower.contains("\"secret\"")
            || lower.contains("\"bearer\"");
        if !looks_like_key_field {
            continue;
        }
        // Extract the value side of the `"field": "value"` pair.
        if let Some(colon) = line.find(':') {
            let after = &line[colon + 1..];
            let trimmed = after.trim_start();
            if let Some(rest) = trimmed.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    let val = &rest[..end];
                    // Heuristic: a real secret is long and high-entropy. A short
                    // or empty value (e.g. `"api_key_env": "SC_API_KEY"`) is fine.
                    if val.len() >= 20 && val.chars().any(|c| c.is_ascii_alphanumeric()) {
                        let preview: String = val.chars().take(8).collect();
                        return Some(format!(
                            "{}:{}: possible secret in settings ({}…) — use the env var instead",
                            path.display(),
                            i + 1,
                            preview
                        ));
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped defaults are the product: every context cap unlimited.
    #[test]
    fn context_defaults_are_unlimited() {
        let c = ContextConfig::default();
        assert_eq!(c.inline_file_cap, 0);
        assert_eq!(c.tool_result_cap, 0);
        assert_eq!(c.bash_output_cap, 0);
        assert_eq!(c.grep_output_cap, 0);
        assert_eq!(c.read_default_limit, 0);
        assert_eq!(c.read_max_line_chars, 0);
        assert_eq!(c.glob_cap, 0);
        // The runaway backstop is the one non-zero default.
        assert_eq!(c.max_iters, 1000);
    }

    /// A settings file may dial the caps back in for a small-context model, and
    /// partial blocks keep the defaults for unmentioned fields.
    #[test]
    fn context_block_round_trips_from_json() {
        let json = r#"{"tool_result_cap": 16384, "read_default_limit": 2000}"#;
        let c: ContextConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.tool_result_cap, 16384);
        assert_eq!(c.read_default_limit, 2000);
        // Unmentioned fields keep the unlimited default.
        assert_eq!(c.bash_output_cap, 0);
        assert_eq!(c.max_iters, 1000);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let json = r#"{"context": {"tool_result_cap": 42, "future_key": true}}"#;
        let f: SettingsFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.context.unwrap().tool_result_cap, 42);
    }

    /// The `models` roster parses as a plain array of names.
    #[test]
    fn models_roster_parses_from_json() {
        let json = r#"{"model": "a/one", "models": ["a/one", "b/two"]}"#;
        let f: SettingsFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.model.as_deref(), Some("a/one"));
        assert_eq!(
            f.models.unwrap(),
            vec!["a/one".to_string(), "b/two".to_string()]
        );
    }

    /// A settings file with no `models` key still yields a usable roster: the
    /// active model. The settings page's ←/→ would have nothing to stand on
    /// otherwise, which is the state every existing install starts in.
    #[test]
    fn roster_always_contains_the_active_model() {
        let s = Settings::load(Path::new("/nonexistent-project-dir"));
        assert!(
            s.models.contains(&s.model),
            "roster {:?} must contain the active model {}",
            s.models,
            s.model
        );
    }
}
