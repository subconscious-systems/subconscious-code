//! Layered settings (§10).
//!
//! M0 implemented a useful subset of the §10.1 precedence stack:
//!   compiled defaults → user (`~/.sc/settings.json`) → project
//!   (`./.sc/settings.json`) → env vars. CLI flags (applied in rc-cli) override
//!   on top. Enterprise/locked settings, JSON Schema validation, and hot-reload
//!   land in later milestones (G1/G4/G5).
//!
//! The API key is resolved from the env var named by `provider.api_key_env`
//! (default `SC_API_KEY`), falling back to `~/.sc/key` — a dedicated file the
//! TUI `/menu` "Change API key" option writes at mode 0600 — when the env var
//! is unset. It is never stored in a *settings* file: `sc doctor` (G7)
//! complains loudly if a key-shaped string appears in a settings file. The
//! `~/.sc/key` file is the sanctioned exception.
//!
//! M3 adds the `permissions` block (§7.1/§10.2): allow/ask/deny rule lists,
//! `defaultMode`, and `additionalDirectories`.
//!
//! M8 adds the `context` block: the per-item truncation caps. Most remain
//! unlimited by default; tool results use a provider-safe projection cap so a
//! runaway result cannot invalidate the next request.

use std::path::{Path, PathBuf};

pub mod edit;

/// Request-body transport between Subconscious Code and its HTTP endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestTransport {
    /// Ordinary OpenAI-compatible JSON.
    Json,
    /// Require the DLR sidecar; fail closed if it is unavailable.
    Dlr,
    /// Prefer DLR, falling back to JSON only before DLR becomes active.
    #[default]
    Auto,
}

impl std::fmt::Display for RequestTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Json => "json",
            Self::Dlr => "dlr",
            Self::Auto => "auto",
        })
    }
}

/// Resolved settings ready to drive a chat client + the permission engine.
#[derive(Debug, Clone)]
pub struct Settings {
    pub base_url: String,
    pub api_key: Option<String>,
    /// The env var the key was resolved from (default `SC_API_KEY`, overridable
    /// via `provider.api_key_env`). Exposed so the TUI can warn when a saved
    /// `~/.sc/key` is shadowed by the env var.
    pub api_key_env: String,
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
    /// Retry: max retries on transient HTTP errors 429/5xx (0 = off, the
    /// default). The Subconscious gateway owns upstream retries, so enabling
    /// another client retry layer is opt-in. Env `SC_MAX_RETRIES`.
    pub max_retries: u32,
    /// Retry: base backoff (ms) for the first retry; doubles each attempt. Env
    /// `SC_RETRY_BASE_MS`.
    pub retry_base_ms: u64,
    /// Retry: cap (ms) on the backoff between retries. Env `SC_RETRY_MAX_MS`.
    pub retry_max_ms: u64,
    /// T3: wall-clock budget for a turn in ms (0 = off). Env `SC_TURN_TIMEOUT_MS`.
    pub turn_timeout_ms: u64,
    /// M4: per-response completion-token cap (8192 by default; 0 = provider
    /// default). Env `SC_MAX_TOKENS`. The explicit default avoids the GLM
    /// route's observed 4096-token implicit ceiling cutting tool calls.
    pub max_tokens: u32,
    /// Sampling temperature (None = provider default). Env `SC_TEMPERATURE`.
    pub temperature: Option<f32>,
    /// Provider-native reasoning effort. The Subconscious GLM route maps
    /// `high` to its lower-latency coding posture; omitting the field selects
    /// `max`, which the Spark traces showed spending most turn time in hidden
    /// reasoning. Set `off`/`none`/an empty value to omit the wire field. Env
    /// `SC_REASONING_EFFORT`.
    pub reasoning_effort: Option<String>,
    pub permissions: PermissionsConfig,
    /// M7: opt-in kernel sandbox for `Bash` (§7.6). Off by default.
    pub sandbox: SandboxConfig,
    /// M8: per-item context caps. Tool results have a provider-safe default;
    /// the remaining caps are unlimited unless configured.
    pub context: ContextConfig,
    /// M8: gzip the request body (`Content-Encoding: gzip`). Enabled by default
    /// for `api.subconscious.dev`; an incompatible gateway is detected once and
    /// the request is safely retried uncompressed. Env `SC_REQUEST_GZIP`.
    pub request_gzip: bool,
    /// Request transport. DLR affects only the hop to the sidecar; the
    /// sidecar forwards ordinary JSON to the configured provider.
    pub request_transport: RequestTransport,
    /// Base URL of the independently deployed DLR sidecar.
    pub dlr_url: String,
    /// Optional sidecar ingress secret, resolved from an env var and never
    /// persisted as a literal in settings.
    pub dlr_ingress_token: Option<String>,
    pub dlr_ingress_token_env: String,
    /// Extra repair symbols requested during a RESYNC/MISSING exchange.
    pub dlr_repair_margin_pct: u32,
    /// Whether the TUI grabs the mouse. On by default so the wheel and trackpad
    /// scroll conversation history immediately. `sc` performs selection and
    /// copies on release while captured; Ctrl+O releases the mouse when native
    /// terminal selection is preferred.
    pub mouse: bool,
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
/// Most shipped defaults remain unlimited. Tool results default to a bounded
/// projection because providers enforce a real token window: one accidental
/// recursive inventory must not make the next request invalid. The full
/// session remains available on disk and the cap can still be configured.
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

/// Model-facing bytes retained from each tool result by default. Results remain
/// complete in the persisted session (up to the separate 1 MiB emergency
/// backstop); this smaller stable projection prevents one read from dominating
/// request latency without retroactively rewriting cached history.
pub const DEFAULT_TOOL_RESULT_CAP: usize = 64 * 1024;

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            inline_file_cap: 0,
            tool_result_cap: DEFAULT_TOOL_RESULT_CAP,
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
    ui: Option<UiFile>,
}

/// The `ui` block: terminal-interaction preferences.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct UiFile {
    mouse: Option<bool>,
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
    reasoning_effort: Option<String>,
    /// User-facing DLR switch. `true` selects safe DLR-first (`auto`) mode;
    /// `false` selects ordinary JSON. The older `request_transport` setting is
    /// retained below for compatibility and for its expert fail-closed mode.
    dlr_enabled: Option<bool>,
    request_transport: Option<RequestTransport>,
    dlr_url: Option<String>,
    dlr_ingress_token_env: Option<String>,
    dlr_repair_margin_pct: Option<u32>,
}

// Defaults from §10.2. All overridable via env (§5.6 G3) or settings files.
const DEFAULT_BASE_URL: &str = "https://api.subconscious.dev/v1";
// DLR appends `/v1/dlr/*` itself, so this is the origin rather than the
// OpenAI-compatible `/v1` base used by ordinary JSON requests.
const DEFAULT_DLR_URL: &str = "https://api.subconscious.dev";
const DEFAULT_MODEL: &str = "subconscious/glm-5.2";
const DEFAULT_SMALL_MODEL: &str = "subconscious/glm-5.2";
/// Off: see [`Settings::timeout_ms`].
const DEFAULT_TIMEOUT_MS: u64 = 0;
/// The liveness backstop that replaces the total timeout. Two minutes with no
/// stream chunk means something is wrong.
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 120_000;
/// The gateway/router owns retries and worker failover. Retrying again in SC
/// multiplies one logical request across every upstream retry layer and can
/// hold a router circuit open. Custom direct endpoints may opt back in.
const DEFAULT_MAX_RETRIES: u32 = 0;
/// The observed GLM route otherwise defaults to 4096 completion tokens, which
/// is frequently consumed entirely by reasoning before a tool call is closed.
const DEFAULT_MAX_TOKENS: u32 = 8_192;
const DEFAULT_REASONING_EFFORT: &str = "high";

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
        let mut max_tokens: u32 = DEFAULT_MAX_TOKENS;
        let mut temperature: Option<f32> = None;
        let mut reasoning_effort = Some(DEFAULT_REASONING_EFFORT.to_string());
        let mut model = DEFAULT_MODEL.to_string();
        let mut models: Vec<String> = Vec::new();
        let mut small_model = DEFAULT_SMALL_MODEL.to_string();
        let mut permissions = PermissionsConfig::default();
        let mut sandbox = SandboxConfig::default();
        let mut context = ContextConfig::default();
        let mut request_gzip = false;
        let mut request_gzip_configured = false;
        // DLR is attempted first. Until `/v1/dlr/*` is deployed, the bounded
        // capability probe fails and `auto` safely uses normal JSON.
        let mut request_transport = RequestTransport::default();
        let mut dlr_url = DEFAULT_DLR_URL.to_string();
        let mut dlr_ingress_token_env = "SC_DLR_INGRESS_TOKEN".to_string();
        let mut dlr_repair_margin_pct = 5u32;
        // Scrollback is a primary conversation action, so wheel/trackpad events
        // work on first launch. Ctrl+O hands the mouse back to the terminal for
        // native selection whenever that is preferable.
        let mut mouse = true;

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
                            request_gzip_configured = true;
                        }
                        if let Some(effort) = p.reasoning_effort {
                            reasoning_effort = normalize_reasoning_effort(&effort);
                        }
                        if let Some(transport) = p.request_transport {
                            request_transport = transport;
                        }
                        // The simple switch is the current public setting and
                        // wins over a legacy transport value in the same layer.
                        if let Some(enabled) = p.dlr_enabled {
                            request_transport = transport_for_dlr_enabled(enabled);
                        }
                        if let Some(url) = p.dlr_url {
                            dlr_url = url;
                        }
                        if let Some(env) = p.dlr_ingress_token_env {
                            dlr_ingress_token_env = env;
                        }
                        if let Some(margin) = p.dlr_repair_margin_pct {
                            dlr_repair_margin_pct = margin;
                        }
                    }
                    if let Some(ui) = file.ui {
                        if let Some(m) = ui.mouse {
                            mouse = m;
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
        if let Ok(v) = std::env::var("SC_REASONING_EFFORT") {
            reasoning_effort = normalize_reasoning_effort(&v);
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
            request_gzip_configured = true;
        }
        if let Ok(v) = std::env::var("SC_REQUEST_TRANSPORT") {
            request_transport = match v.trim().to_ascii_lowercase().as_str() {
                "dlr" => RequestTransport::Dlr,
                "auto" => RequestTransport::Auto,
                "json" => RequestTransport::Json,
                _ => request_transport,
            };
        }
        // The boolean switch is the public override and therefore wins over
        // the legacy/expert SC_REQUEST_TRANSPORT value when both are present.
        if let Some(enabled) = env_bool("SC_DLR_ENABLED") {
            request_transport = transport_for_dlr_enabled(enabled);
        }
        if let Ok(v) = std::env::var("SC_DLR_URL") {
            if !v.trim().is_empty() {
                dlr_url = v;
            }
        }
        if let Ok(v) = std::env::var("SC_DLR_REPAIR_MARGIN_PCT") {
            if let Ok(value) = v.parse::<u32>() {
                dlr_repair_margin_pct = value;
            }
        }
        if let Some(b) = env_bool("SC_MOUSE") {
            mouse = b;
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

        let api_key = resolve_api_key(&api_key_env);
        let dlr_ingress_token = std::env::var(&dlr_ingress_token_env)
            .ok()
            .filter(|value| !value.is_empty());

        if !request_gzip_configured && base_url.contains("api.subconscious.dev") {
            request_gzip = true;
        }

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
            api_key_env,
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
            reasoning_effort,
            permissions,
            sandbox,
            context,
            request_gzip,
            request_transport,
            dlr_url,
            dlr_ingress_token,
            dlr_ingress_token_env,
            dlr_repair_margin_pct,
            mouse,
        }
    }
}

fn transport_for_dlr_enabled(enabled: bool) -> RequestTransport {
    if enabled {
        RequestTransport::Auto
    } else {
        RequestTransport::Json
    }
}

fn normalize_reasoning_effort(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(value.to_ascii_lowercase())
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

/// `~/.sc/key` — the dedicated API-key file the TUI `/menu` "Change API key"
/// option writes. It is the env var's fallback: [`Settings::load`] uses it
/// only when the env var named by `provider.api_key_env` is unset. Created
/// mode 0600 by [`set_api_key`].
pub fn key_file_path() -> Option<PathBuf> {
    user_dir().map(|d| d.join("key"))
}

/// Read and trim the key file; `None` if it is missing, empty, or blank. The
/// loader calls this only as the env var's fallback, so a present file never
/// overrides an explicit env var.
fn read_key_file(path: &Path) -> Option<String> {
    let v = std::fs::read_to_string(path).ok()?;
    let v = v.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// The active API key: the env var named by `provider.api_key_env` first,
/// then `~/.sc/key`. This is startup precedence — an explicit env var beats a
/// file, so a scripted or CI invocation is never quietly overridden by
/// whatever was last saved on that machine.
pub fn resolve_api_key(env_name: &str) -> Option<String> {
    std::env::var(env_name)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| key_file_path().and_then(|p| read_key_file(&p)))
}

/// Just the saved key from `~/.sc/key`, ignoring the environment.
///
/// The `/menu` reload path uses this rather than [`resolve_api_key`]: the user
/// has *just* typed a key into this process, so it is the one they mean for
/// this run even when the env var would otherwise outrank it. Startup
/// precedence is untouched, so the next launch is back to env-first.
pub fn saved_api_key() -> Option<String> {
    key_file_path().and_then(|p| read_key_file(&p))
}

/// Write `value` to `~/.sc/key` (mode 0600), creating `~/.sc/` if needed. This
/// is the persistence path for the TUI "Change API key" menu option. The env
/// var still wins, so a saved key takes effect on the next `sc` launch unless
/// the env var is set in the shell.
pub fn set_api_key(value: &str) -> Result<PathBuf, String> {
    let path = key_file_path().ok_or("HOME is not set; cannot locate ~/.sc/key")?;
    write_key_file(&path, value)?;
    Ok(path)
}

fn write_key_file(path: &Path, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("api key cannot be empty".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, format!("{value}\n"))
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
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

    #[test]
    fn provider_defaults_leave_retry_ownership_upstream() {
        assert_eq!(DEFAULT_MAX_RETRIES, 0);
        assert_eq!(DEFAULT_MAX_TOKENS, 8_192);
        assert_eq!(DEFAULT_REASONING_EFFORT, "high");
        assert_eq!(DEFAULT_DLR_URL, "https://api.subconscious.dev");
        assert_eq!(RequestTransport::default(), RequestTransport::Auto);
        assert_eq!(normalize_reasoning_effort(" OFF "), None);
        assert_eq!(normalize_reasoning_effort("High").as_deref(), Some("high"));
    }

    #[test]
    fn dlr_provider_settings_parse_without_storing_the_secret() {
        let file: SettingsFile = serde_json::from_str(
            r#"{"provider":{"dlr_enabled":true,"request_transport":"dlr","dlr_url":"https://sidecar.internal","dlr_ingress_token_env":"MY_DLR_TOKEN","dlr_repair_margin_pct":9}}"#,
        )
        .unwrap();
        let provider = file.provider.unwrap();
        assert_eq!(provider.dlr_enabled, Some(true));
        assert_eq!(provider.request_transport, Some(RequestTransport::Dlr));
        assert_eq!(
            provider.dlr_url.as_deref(),
            Some("https://sidecar.internal")
        );
        assert_eq!(
            provider.dlr_ingress_token_env.as_deref(),
            Some("MY_DLR_TOKEN")
        );
        assert_eq!(provider.dlr_repair_margin_pct, Some(9));
        assert_eq!(RequestTransport::default(), RequestTransport::Auto);
        assert_eq!(transport_for_dlr_enabled(true), RequestTransport::Auto);
        assert_eq!(transport_for_dlr_enabled(false), RequestTransport::Json);
    }

    /// Only model-facing tool output is bounded by default; direct file reads,
    /// searches, and mentions retain their previous unlimited behavior.
    #[test]
    fn context_defaults_keep_tool_results_provider_safe() {
        let c = ContextConfig::default();
        assert_eq!(c.inline_file_cap, 0);
        assert_eq!(c.tool_result_cap, DEFAULT_TOOL_RESULT_CAP);
        assert_eq!(c.bash_output_cap, 0);
        assert_eq!(c.grep_output_cap, 0);
        assert_eq!(c.read_default_limit, 0);
        assert_eq!(c.read_max_line_chars, 0);
        assert_eq!(c.glob_cap, 0);
        // Iterations retain their independent runaway backstop.
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
        // Unmentioned fields keep their defaults.
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

    /// Scrollback works with the wheel on first launch. Ctrl+O remains the
    /// explicit escape hatch for native terminal selection.
    #[test]
    fn mouse_capture_is_on_for_scrollback_by_default() {
        let s = Settings::load(Path::new("/nonexistent-project-dir"));
        assert!(s.mouse, "default must enable wheel scrollback");
    }

    /// With the env var unset, the resolver is exactly the saved key — the
    /// property the `/menu` reload leans on. (Asserted without mutating the
    /// environment, which would race the other tests in this binary.)
    #[test]
    fn resolve_falls_back_to_the_saved_key_when_the_env_var_is_unset() {
        assert_eq!(
            resolve_api_key("SC_API_KEY_DEFINITELY_NOT_SET_IN_THIS_PROCESS"),
            saved_api_key(),
        );
    }

    /// The dedicated key file round-trips, trims surrounding whitespace, is
    /// created mode 0600 on unix, and an empty/blank value is rejected (the
    /// menu treats an empty Enter as a cancel, not a clear).
    #[test]
    fn key_file_round_trips_and_is_masked_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        write_key_file(&path, "  sk-test-1234567890  \n").unwrap();
        assert_eq!(read_key_file(&path).as_deref(), Some("sk-test-1234567890"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be 0600, got {:o}", mode);
        }
        // A blank file reads as None — the loader treats it as "no key".
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(read_key_file(&path), None);
        // An empty value is rejected rather than writing a blank key file.
        assert!(write_key_file(&path, "   ").is_err(), "empty key rejected");
        // A missing file reads as None (no panic).
        assert_eq!(read_key_file(&dir.path().join("absent")), None);
    }
}
