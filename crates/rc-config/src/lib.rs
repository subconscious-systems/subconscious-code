//! Layered settings (§10).
//!
//! M0 implemented a useful subset of the §10.1 precedence stack:
//!   compiled defaults → user (`~/.rc/settings.json`) → project
//!   (`./.rc/settings.json`) → env vars. CLI flags (applied in rc-cli) override
//!   on top. Enterprise/locked settings, JSON Schema validation, and hot-reload
//!   land in later milestones (G1/G4/G5).
//!
//! The API key is never stored in a settings file — it is resolved from the
//! env var named by `provider.api_key_env` (default `RC_API_KEY`). `rc doctor`
//! (G7) will complain loudly if a key-shaped string appears in any file.
//!
//! M3 adds the `permissions` block (§7.1/§10.2): allow/ask/deny rule lists,
//! `defaultMode`, and `additionalDirectories`.

use std::path::{Path, PathBuf};

/// Resolved settings ready to drive a chat client + the permission engine.
#[derive(Debug, Clone)]
pub struct Settings {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub small_model: String,
    pub timeout_ms: u64,
    /// T2: max gap between stream chunks before the turn aborts (0 = off). Env
    /// `RC_IDLE_TIMEOUT_MS`.
    pub idle_timeout_ms: u64,
    /// Retry: max retries on transient HTTP errors 429/5xx (0 = off). Env
    /// `RC_MAX_RETRIES`. Recommended production value: 2–3.
    pub max_retries: u32,
    /// Retry: base backoff (ms) for the first retry; doubles each attempt. Env
    /// `RC_RETRY_BASE_MS`.
    pub retry_base_ms: u64,
    /// Retry: cap (ms) on the backoff between retries. Env `RC_RETRY_MAX_MS`.
    pub retry_max_ms: u64,
    /// T3: wall-clock budget for a turn in ms (0 = off). Env `RC_TURN_TIMEOUT_MS`.
    pub turn_timeout_ms: u64,
    /// M4: per-response completion-token cap (0 = provider default). Env
    /// `RC_MAX_TOKENS`. Bounds each reply's length.
    pub max_tokens: u32,
    /// Sampling temperature (None = provider default). Env `RC_TEMPERATURE`.
    pub temperature: Option<f32>,
    pub permissions: PermissionsConfig,
    /// M7: opt-in kernel sandbox for `Bash` (§7.6). Off by default.
    pub sandbox: SandboxConfig,
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
    /// Enable confinement for every approved `Bash` command. Env `RC_SANDBOX`.
    pub enabled: bool,
    /// Allow network syscalls under confinement. Env `RC_SANDBOX_NET`.
    pub allow_net: bool,
}

/// On-disk shape of a settings file. Unknown keys are ignored (forward-compat).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct SettingsFile {
    provider: Option<ProviderFile>,
    model: Option<String>,
    small_model: Option<String>,
    permissions: Option<PermissionsConfig>,
    sandbox: Option<SandboxConfig>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ProviderFile {
    base_url: Option<String>,
    api_key_env: Option<String>,
    timeout_ms: Option<u64>,
}

// Defaults from §10.2. All overridable via env (§5.6 G3) or settings files.
const DEFAULT_BASE_URL: &str = "https://zig.subconscious.dev/v1";
const DEFAULT_MODEL: &str = "tim-qwen3.6-27b";
const DEFAULT_SMALL_MODEL: &str = "qwen3.5-9b";
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

impl Settings {
    /// Load settings with M0 precedence: defaults → user → project → env.
    pub fn load(project_dir: &Path) -> Self {
        let mut base_url = DEFAULT_BASE_URL.to_string();
        let mut api_key_env = "RC_API_KEY".to_string();
        let mut timeout_ms = DEFAULT_TIMEOUT_MS;
        let mut idle_timeout_ms: u64 = 0;
        let mut max_retries: u32 = 0;
        let mut retry_base_ms: u64 = 200;
        let mut retry_max_ms: u64 = 10_000;
        let mut turn_timeout_ms: u64 = 0;
        let mut max_tokens: u32 = 0;
        let mut temperature: Option<f32> = None;
        let mut model = DEFAULT_MODEL.to_string();
        let mut small_model = DEFAULT_SMALL_MODEL.to_string();
        let mut permissions = PermissionsConfig::default();
        let mut sandbox = SandboxConfig::default();

        // Later layers override earlier ones. User before project so a
        // committed project file beats a user global — matches §10.1 (project
        // is higher precedence than user).
        let layers: Vec<Option<PathBuf>> =
            vec![user_settings_path(), project_settings_path(project_dir)];
        for path in layers.into_iter().flatten() {
            if let Some(file) = read_settings(&path) {
                if let Some(p) = file.provider {
                    if let Some(u) = p.base_url { base_url = u; }
                    if let Some(e) = p.api_key_env { api_key_env = e; }
                    if let Some(t) = p.timeout_ms { timeout_ms = t; }
                }
                if let Some(m) = file.model { model = m; }
                if let Some(s) = file.small_model { small_model = s; }
                if let Some(p) = file.permissions { permissions = p; }
                if let Some(s) = file.sandbox { sandbox = s; }
            }
        }

        // Env wins over files (§10.1).
        if let Ok(v) = std::env::var("RC_BASE_URL") { if !v.is_empty() { base_url = v; } }
        if let Ok(v) = std::env::var("RC_MODEL") { if !v.is_empty() { model = v; } }
        if let Ok(v) = std::env::var("RC_SMALL_MODEL") { if !v.is_empty() { small_model = v; } }
        if let Ok(v) = std::env::var("RC_TIMEOUT_MS") { if let Ok(t) = v.parse() { timeout_ms = t; } }
        if let Ok(v) = std::env::var("RC_IDLE_TIMEOUT_MS") { if let Ok(t) = v.parse::<u64>() { idle_timeout_ms = t; } }
        if let Ok(v) = std::env::var("RC_MAX_RETRIES") { if let Ok(t) = v.parse::<u32>() { max_retries = t; } }
        if let Ok(v) = std::env::var("RC_RETRY_BASE_MS") { if let Ok(t) = v.parse::<u64>() { retry_base_ms = t; } }
        if let Ok(v) = std::env::var("RC_RETRY_MAX_MS") { if let Ok(t) = v.parse::<u64>() { retry_max_ms = t; } }
        if let Ok(v) = std::env::var("RC_TURN_TIMEOUT_MS") { if let Ok(t) = v.parse::<u64>() { turn_timeout_ms = t; } }
        if let Ok(v) = std::env::var("RC_MAX_TOKENS") { if let Ok(t) = v.parse::<u32>() { max_tokens = t; } }
        if let Ok(v) = std::env::var("RC_TEMPERATURE") { if let Ok(t) = v.parse::<f32>() { temperature = Some(t); } }
        if let Ok(v) = std::env::var("RC_DEFAULT_MODE") { if !v.is_empty() { permissions.default_mode = v; } }
        if let Some(b) = env_bool("RC_SANDBOX") { sandbox.enabled = b; }
        if let Some(b) = env_bool("RC_SANDBOX_NET") { sandbox.allow_net = b; }

        let api_key = std::env::var(&api_key_env)
            .ok()
            .filter(|s| !s.is_empty());

        Settings { base_url, api_key, model, small_model, timeout_ms, idle_timeout_ms, max_retries, retry_base_ms, retry_max_ms, turn_timeout_ms, max_tokens, temperature, permissions, sandbox }
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

fn user_settings_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".rc").join("settings.json"))
}

fn project_settings_path(project: &Path) -> Option<PathBuf> {
    Some(project.join(".rc").join("settings.json"))
}

/// Read + parse a settings file. M0 fails soft (a malformed or absent file is
/// ignored); G4/G7 will validate and report.
fn read_settings(path: &Path) -> Option<SettingsFile> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<SettingsFile>(&bytes).ok()
}
