//! Layered settings (§10).
//!
//! M0 implements a useful subset of the §10.1 precedence stack:
//!   compiled defaults → user (`~/.rc/settings.json`) → project
//!   (`./.rc/settings.json`) → env vars. CLI flags (applied in rc-cli) override
//!   on top. Enterprise/locked settings, deep-merge of arbitrary keys, JSON
//!   Schema validation, and hot-reload land in later milestones (G1/G4/G5).
//!
//! The API key is never stored in a settings file — it is resolved from the
//! env var named by `provider.api_key_env` (default `RC_API_KEY`). `rc doctor`
//! (G7) will complain loudly if a key-shaped string appears in any file.

use std::path::{Path, PathBuf};

/// Resolved settings ready to drive a chat client.
#[derive(Debug, Clone)]
pub struct Settings {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub small_model: String,
    pub timeout_ms: u64,
}

/// On-disk shape of a settings file. Unknown keys are ignored (forward-compat).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct SettingsFile {
    provider: Option<ProviderFile>,
    model: Option<String>,
    small_model: Option<String>,
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
        let mut model = DEFAULT_MODEL.to_string();
        let mut small_model = DEFAULT_SMALL_MODEL.to_string();

        // Later layers override earlier ones. User before project so a
        // committed project file beats a user global — matches §10.1 (project
        // is higher precedence than user).
        let mut layers: Vec<Option<PathBuf>> =
            vec![user_settings_path(), project_settings_path(project_dir)];
        for path in layers.drain(..).flatten() {
            if let Some(file) = read_settings(&path) {
                if let Some(p) = file.provider {
                    if let Some(u) = p.base_url { base_url = u; }
                    if let Some(e) = p.api_key_env { api_key_env = e; }
                    if let Some(t) = p.timeout_ms { timeout_ms = t; }
                }
                if let Some(m) = file.model { model = m; }
                if let Some(s) = file.small_model { small_model = s; }
            }
        }

        // Env wins over files (§10.1).
        if let Ok(v) = std::env::var("RC_BASE_URL") { if !v.is_empty() { base_url = v; } }
        if let Ok(v) = std::env::var("RC_MODEL") { if !v.is_empty() { model = v; } }
        if let Ok(v) = std::env::var("RC_SMALL_MODEL") { if !v.is_empty() { small_model = v; } }
        if let Ok(v) = std::env::var("RC_TIMEOUT_MS") { if let Ok(t) = v.parse() { timeout_ms = t; } }

        let api_key = std::env::var(&api_key_env)
            .ok()
            .filter(|s| !s.is_empty());

        Settings { base_url, api_key, model, small_model, timeout_ms }
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
