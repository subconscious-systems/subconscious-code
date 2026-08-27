//! The rule engine (§7.1): deny → allow → ask, first match wins; tool-specific
//! matchers; the five modes (§7.3); and the [`PermissionChecker`] trait with
//! [`AllowAllChecker`] (tests), [`BypassChecker`]
//! (`--dangerously-skip-permissions`), and [`PermissionEngine`] (the real one).

use crate::bash::{is_always_ask, is_catastrophic_cmd, parse_bash, rule_matches};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone)]
pub enum Decision {
    Allow,
    Deny(String),
    Ask(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    AcceptEdits,
    Plan,
    /// Confirm every tool call, including reads.
    Ask,
    /// Run without prompting; catastrophic commands stay denied.
    Auto,
}

impl Mode {
    /// Parse a `permissions.default_mode` string. `bypassPermissions` is the
    /// pre-rename spelling of `auto`, still accepted so an existing
    /// `settings.json` keeps working instead of silently falling back to
    /// `default`.
    pub fn parse(s: &str) -> Self {
        match s {
            "acceptEdits" => Mode::AcceptEdits,
            "plan" => Mode::Plan,
            "ask" => Mode::Ask,
            "auto" | "bypassPermissions" => Mode::Auto,
            _ => Mode::Default,
        }
    }

    /// The canonical `settings.json` spelling, and what the UI shows.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::AcceptEdits => "acceptEdits",
            Mode::Plan => "plan",
            Mode::Ask => "ask",
            Mode::Auto => "auto",
        }
    }
    /// Stable codec for the `AtomicU8` the engine stores its mode in (so the
    /// TUI's Shift+Tab can swap it live without a `Mutex`).
    pub fn to_u8(self) -> u8 {
        match self {
            Mode::Default => 0,
            Mode::AcceptEdits => 1,
            Mode::Plan => 2,
            // 3 stays `Auto` (formerly `BypassPermissions`) so the codec keeps
            // its meaning across the rename; `Ask` takes the next free value.
            Mode::Auto => 3,
            Mode::Ask => 4,
        }
    }
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Mode::AcceptEdits,
            2 => Mode::Plan,
            3 => Mode::Auto,
            4 => Mode::Ask,
            _ => Mode::Default,
        }
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    /// `auto` is the new spelling; `bypassPermissions` is the old one. Both
    /// must resolve to the same mode or an existing `settings.json` would
    /// silently fall back to `default` — the failure this rename could easily
    /// have introduced.
    #[test]
    fn auto_parses_from_both_spellings() {
        assert_eq!(Mode::parse("auto"), Mode::Auto);
        assert_eq!(Mode::parse("bypassPermissions"), Mode::Auto);
        assert_eq!(Mode::parse("ask"), Mode::Ask);
        assert_eq!(Mode::parse("nonsense"), Mode::Default);
    }

    /// The `AtomicU8` codec round-trips every mode. `Auto` must stay 3 so a
    /// mode set before the rename still means the same thing.
    #[test]
    fn u8_codec_round_trips_and_keeps_auto_at_three() {
        for m in [
            Mode::Default,
            Mode::AcceptEdits,
            Mode::Plan,
            Mode::Ask,
            Mode::Auto,
        ] {
            assert_eq!(Mode::from_u8(m.to_u8()), m, "{m:?} must survive the codec");
        }
        assert_eq!(
            Mode::Auto.to_u8(),
            3,
            "Auto keeps BypassPermissions' old value"
        );
    }

    /// `ask` confirms *everything*, including the read-only tools `default`
    /// lets through — that distinction is the mode's entire purpose.
    #[test]
    fn ask_mode_confirms_reads_too() {
        for tool in ["Read", "Glob", "Grep", "Write", "Append", "Bash"] {
            assert!(
                matches!(mode_default(tool, Mode::Ask), Decision::Ask(_)),
                "{tool} should require confirmation in ask mode"
            );
        }
        // Contrast: default lets reads through untouched.
        assert!(matches!(
            mode_default("Read", Mode::Default),
            Decision::Allow
        ));
    }

    /// `auto` allows everything the mode layer sees; the catastrophic-command
    /// guard lives in `bash_check` and is tested separately.
    #[test]
    fn auto_mode_allows_every_tool() {
        for tool in ["Read", "Write", "Append", "Edit", "Bash"] {
            assert!(
                matches!(mode_default(tool, Mode::Auto), Decision::Allow),
                "{tool} in auto"
            );
        }
    }

    /// `as_str` must produce exactly what `parse` accepts, or the settings page
    /// would write a value the loader then ignores.
    #[test]
    fn as_str_round_trips_through_parse() {
        for m in [
            Mode::Default,
            Mode::AcceptEdits,
            Mode::Plan,
            Mode::Ask,
            Mode::Auto,
        ] {
            assert_eq!(
                Mode::parse(m.as_str()),
                m,
                "{m:?} must survive as_str -> parse"
            );
        }
    }
}

fn is_mutating(tool: &str) -> bool {
    matches!(
        tool,
        "Edit" | "Write" | "Append" | "Bash" | "NotebookEdit" | "Task"
    )
}

/// The mode's default decision when no rule matches (§7.3).
fn mode_default(tool: &str, mode: Mode) -> Decision {
    match mode {
        Mode::Default => {
            if is_mutating(tool) {
                Decision::Ask(format!("{tool} requires confirmation"))
            } else {
                Decision::Allow
            }
        }
        Mode::AcceptEdits => {
            if tool == "Bash" {
                Decision::Ask("Bash requires confirmation".into())
            } else {
                Decision::Allow
            }
        }
        Mode::Plan => {
            if is_mutating(tool) {
                Decision::Deny("mutating tools are disabled in plan mode".into())
            } else {
                Decision::Allow
            }
        }
        // Every call is confirmed, reads included — the point of `ask` is that
        // nothing runs unseen, so it deliberately does not exempt Read/Glob/Grep
        // the way `Default` does.
        Mode::Ask => Decision::Ask(format!("{tool} requires confirmation (ask mode)")),
        Mode::Auto => Decision::Allow,
    }
}

/// A parsed rule: `Tool` (bare — whole tool) or `Tool(specifier)`.
#[derive(Debug, Clone)]
struct Rule {
    tool: String,
    spec: Option<String>,
}

impl Rule {
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if let (Some(open), Some(close)) = (s.find('('), s.rfind(')')) {
            if open < close {
                let tool = s[..open].trim().to_string();
                let spec = s[open + 1..close].to_string();
                return Some(Rule {
                    tool,
                    spec: Some(spec),
                });
            }
        }
        Some(Rule {
            tool: s.to_string(),
            spec: None,
        })
    }
}

fn parse_rules(list: &[String]) -> Vec<Rule> {
    list.iter().filter_map(|s| Rule::parse(s)).collect()
}

/// The seam the agent loop calls before each tool (so the loop is testable
/// without the concrete engine).
pub trait PermissionChecker: Send + Sync {
    fn check(
        &self,
        tool: &str,
        input: &Value,
        cwd: &Path,
        roots: &[PathBuf],
        grants: &[String],
    ) -> Decision;
    /// Live mode cycling (Shift+Tab in the TUI). Default no-op; `PermissionEngine`
    /// overrides it to swap its mode atomically. `AllowAllChecker`/`BypassChecker`
    /// keep the default — their decision is mode-independent.
    fn set_mode(&self, _mode: Mode) {}
}

/// Allows everything — for tests and scripted loops.
pub struct AllowAllChecker;
impl PermissionChecker for AllowAllChecker {
    fn check(&self, _: &str, _: &Value, _: &Path, _: &[PathBuf], _: &[String]) -> Decision {
        Decision::Allow
    }
}

/// `--dangerously-skip-permissions`: allows everything except the catastrophic
/// set (still hard-denied) — bypass must not run `rm -rf /`. The catastrophic
/// check is against the raw command string so an unparseable command (e.g.
/// `rm -rf $HOME`, which parse_bash yields no subcommands for) is still caught.
pub struct BypassChecker;
impl PermissionChecker for BypassChecker {
    fn check(&self, tool: &str, input: &Value, _: &Path, _: &[PathBuf], _: &[String]) -> Decision {
        if tool == "Bash" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                if is_catastrophic_cmd(cmd) {
                    return Decision::Deny("destructive command refused (even in bypass)".into());
                }
            }
        }
        Decision::Allow
    }
}

pub struct PermissionEngine {
    mode: AtomicU8,
    deny: Vec<Rule>,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
}

impl PermissionEngine {
    pub fn new(mode: Mode, deny: Vec<String>, allow: Vec<String>, ask: Vec<String>) -> Self {
        Self {
            mode: AtomicU8::new(mode.to_u8()),
            deny: parse_rules(&deny),
            allow: parse_rules(&allow),
            ask: parse_rules(&ask),
        }
    }

    /// Snapshot the current mode (Relaxed: a mid-turn swap is best-effort; the
    /// next `check` sees it). The four-variant codec keeps this lock-free.
    fn mode(&self) -> Mode {
        Mode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    fn path_matches(rule: &Rule, input: &Value, cwd: &Path) -> bool {
        let Some(spec) = &rule.spec else {
            return true;
        }; // bare tool matches any path
        let Some(p) = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(|v| v.as_str())
        else {
            return false;
        };
        let path = Path::new(p);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let rel = abs
            .strip_prefix(cwd)
            .map(|r| r.to_path_buf())
            .unwrap_or(abs);
        let spec = spec.strip_prefix("./").unwrap_or(spec);
        match globset::GlobBuilder::new(spec)
            .literal_separator(false)
            .build()
        {
            Ok(g) => g.compile_matcher().is_match(&rel),
            Err(_) => false,
        }
    }

    fn bash_specs(rules: &[Rule]) -> Vec<&str> {
        rules
            .iter()
            .filter(|r| r.tool == "Bash")
            .filter_map(|r| r.spec.as_deref())
            .collect()
    }

    fn bash_check(&self, cmd: &str, grants: &[Rule], mode: Mode) -> Decision {
        // Catastrophic commands are always denied, even in bypass — and checked
        // against the *raw* string so an unparseable command (e.g. `rm -rf $HOME`,
        // which parse_bash yields no subcommands for) is still caught.
        if is_catastrophic_cmd(cmd) {
            return Decision::Deny("destructive command refused".into());
        }
        // Bypass: allow everything except the catastrophic commands above. The
        // unparseable / always-ask escalations below fail closed for the
        // *asking* modes (Default / AcceptEdits / Plan); in bypass the user
        // opted out of prompts, so honor that for ordinary commands. Without
        // this early return, a `$`/`$(...)`/`| sh`/`--force` command would still
        // escalate to Ask in bypass — which is what made bypass feel broken.
        if mode == Mode::Auto {
            return Decision::Allow;
        }
        let parsed = parse_bash(cmd);
        if parsed.unparseable {
            return Decision::Ask("complex or unparseable command — needs approval".into());
        }
        // Session grants: if a granted Bash rule covers every sub-command → Allow.
        let grant_specs = Self::bash_specs(grants);
        let grant_any = grants.iter().any(|r| r.tool == "Bash" && r.spec.is_none());
        if !parsed.subcommands.is_empty()
            && parsed
                .subcommands
                .iter()
                .all(|s| grant_any || grant_specs.iter().any(|g| rule_matches(g, s)))
        {
            return Decision::Allow;
        }
        if is_always_ask(cmd) {
            return Decision::Ask("always-ask command (e.g. sudo, force push)".into());
        }
        // deny: any sub-command matching a deny rule → Deny.
        let deny_specs = Self::bash_specs(&self.deny);
        if parsed
            .subcommands
            .iter()
            .any(|s| deny_specs.iter().any(|r| rule_matches(r, s)))
        {
            return Decision::Deny("denied by a rule".into());
        }
        // allow: every sub-command must match some allow rule (bare Bash = any).
        let allow_specs = Self::bash_specs(&self.allow);
        let allow_any = self
            .allow
            .iter()
            .any(|r| r.tool == "Bash" && r.spec.is_none());
        if !parsed.subcommands.is_empty()
            && parsed
                .subcommands
                .iter()
                .all(|s| allow_any || allow_specs.iter().any(|r| rule_matches(r, s)))
        {
            return Decision::Allow;
        }
        // ask
        let ask_specs = Self::bash_specs(&self.ask);
        if parsed
            .subcommands
            .iter()
            .any(|s| ask_specs.iter().any(|r| rule_matches(r, s)))
        {
            return Decision::Ask("asked by a rule".into());
        }
        mode_default("Bash", mode)
    }
}

impl PermissionChecker for PermissionEngine {
    fn check(
        &self,
        tool: &str,
        input: &Value,
        cwd: &Path,
        _roots: &[PathBuf],
        grants: &[String],
    ) -> Decision {
        let mode = self.mode();
        let grant_rules = parse_rules(grants);

        // Session grants for path tools: a matching grant → Allow.
        if tool != "Bash" {
            for r in &grant_rules {
                if r.tool == tool && (r.spec.is_none() || Self::path_matches(r, input, cwd)) {
                    return Decision::Allow;
                }
            }
        }

        if tool == "Bash" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                return self.bash_check(cmd, &grant_rules, mode);
            }
            return Decision::Ask("Bash call without a command".into());
        }

        if mode == Mode::Auto {
            return Decision::Allow;
        }
        // deny → allow → ask, first match wins.
        for r in &self.deny {
            if r.tool == tool && (r.spec.is_none() || Self::path_matches(r, input, cwd)) {
                return Decision::Deny("denied by a rule".into());
            }
        }
        for r in &self.allow {
            if r.tool == tool && (r.spec.is_none() || Self::path_matches(r, input, cwd)) {
                return Decision::Allow;
            }
        }
        for r in &self.ask {
            if r.tool == tool && (r.spec.is_none() || Self::path_matches(r, input, cwd)) {
                return Decision::Ask("asked by a rule".into());
            }
        }

        // A ReadMany call is one transport/tool round, but permission-wise it
        // is exactly a collection of ordinary Reads. Re-evaluate every path as
        // `Read` so existing Read rules and session grants cannot be bypassed
        // by switching to the batched tool. Any denied/asked member governs the
        // whole batch; only an all-allowed set runs.
        if tool == "ReadMany" {
            if let Some(paths) = input.get("file_paths").and_then(Value::as_array) {
                for path in paths.iter().filter_map(Value::as_str) {
                    let read_input = serde_json::json!({"file_path": path});
                    match self.check("Read", &read_input, cwd, _roots, grants) {
                        Decision::Allow => {}
                        decision => return decision,
                    }
                }
                return Decision::Allow;
            }
        }
        mode_default(tool, mode)
    }

    fn set_mode(&self, mode: Mode) {
        self.mode.store(mode.to_u8(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn eng(mode: Mode, deny: &[&str], allow: &[&str], ask: &[&str]) -> PermissionEngine {
        PermissionEngine::new(
            mode,
            deny.iter().map(|s| s.to_string()).collect(),
            allow.iter().map(|s| s.to_string()).collect(),
            ask.iter().map(|s| s.to_string()).collect(),
        )
    }
    fn cwd() -> PathBuf {
        std::env::temp_dir()
    }
    fn roots() -> Vec<PathBuf> {
        vec![cwd()]
    }

    #[test]
    fn bash_catastrophic_is_denied_even_in_bypass() {
        let e = eng(Mode::Auto, &[], &["Bash(rm -rf:*)"], &[]);
        let d = e.check(
            "Bash",
            &json!({"command": "rm -rf /"}),
            &cwd(),
            &roots(),
            &[],
        );
        assert!(matches!(d, Decision::Deny(_)), "{d:?}");
    }

    #[test]
    fn bash_bypass_allows_unparseable_substitution() {
        // `$` makes parse_bash yield no subcommands → the asking modes escalate
        // to Ask (fail closed). Bypass opted out of prompts, so it allows.
        let e = eng(Mode::Auto, &[], &[], &[]);
        let d = e.check(
            "Bash",
            &json!({"command": "echo $HOME"}),
            &cwd(),
            &roots(),
            &[],
        );
        assert!(
            matches!(d, Decision::Allow),
            "bypass allows unparseable: {d:?}"
        );
    }

    #[test]
    fn bash_bypass_allows_always_ask_commands() {
        // sudo / --force / `| sh` always-ask in the asking modes; bypass allows.
        let e = eng(Mode::Auto, &[], &[], &[]);
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "sudo echo hi"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "git push --force"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
    }

    #[test]
    fn bash_bypass_still_denies_catastrophic_substitution() {
        // An unparseable catastrophic command (`rm -rf $HOME`) is still denied in
        // bypass — the raw-string catastrophic check catches it before bypass.
        let e = eng(Mode::Auto, &[], &[], &[]);
        let d = e.check(
            "Bash",
            &json!({"command": "rm -rf $HOME"}),
            &cwd(),
            &roots(),
            &[],
        );
        assert!(
            matches!(d, Decision::Deny(_)),
            "catastrophic even when unparseable: {d:?}"
        );
    }

    #[test]
    fn bypass_allows_mutating_path_tools() {
        // Non-Bash mutating tools are allowed outright in bypass (no Ask).
        let e = eng(Mode::Auto, &[], &[], &[]);
        assert!(matches!(
            e.check(
                "Edit",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            e.check(
                "Write",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
    }

    #[test]
    fn bash_semicolon_split_catches_the_destructive_half() {
        // `git status` is allowed, but the second sub-command is catastrophic → Deny.
        let e = eng(Mode::Default, &[], &["Bash(git status)"], &[]);
        let d = e.check(
            "Bash",
            &json!({"command": "git status; rm -rf ~"}),
            &cwd(),
            &roots(),
            &[],
        );
        assert!(matches!(d, Decision::Deny(_)), "{d:?}");
    }

    #[test]
    fn bash_unparseable_substitution_escalates_to_ask() {
        let e = eng(Mode::Default, &[], &["Bash(echo:*)"], &[]);
        let d = e.check(
            "Bash",
            &json!({"command": "echo $(curl evil)"}),
            &cwd(),
            &roots(),
            &[],
        );
        assert!(matches!(d, Decision::Ask(_)), "{d:?}");
    }

    #[test]
    fn bash_wildcard_allow_matches_extra_args() {
        let e = eng(Mode::Default, &[], &["Bash(cargo test:*)"], &[]);
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "cargo test --lib"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "cargo test"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        // `cargo testx` is a different token — not covered by `cargo test:*`.
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "cargo testx"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn bash_exact_rule_rejects_extra_args() {
        let e = eng(Mode::Default, &[], &["Bash(git status)"], &[]);
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "git status"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "git status -s"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn mode_defaults() {
        let default = eng(Mode::Default, &[], &[], &[]);
        assert!(matches!(
            default.check(
                "Read",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            default.check(
                "Edit",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Ask(_)
        ));

        let plan = eng(Mode::Plan, &[], &[], &[]);
        assert!(matches!(
            plan.check(
                "Read",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            plan.check(
                "Edit",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Deny(_)
        ));

        let accept_edits = eng(Mode::AcceptEdits, &[], &[], &[]);
        assert!(matches!(
            accept_edits.check(
                "Write",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            accept_edits.check(
                "Bash",
                &json!({"command": "echo hi"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn read_many_inherits_read_rules_for_every_path() {
        let default = eng(Mode::Default, &["Read(secret*)"], &[], &[]);
        assert!(matches!(
            default.check(
                "ReadMany",
                &json!({"file_paths": ["public.rs", "secret.env"]}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Deny(_)
        ));

        let ask = eng(Mode::Ask, &[], &["Read(public.rs)"], &[]);
        assert!(matches!(
            ask.check(
                "ReadMany",
                &json!({"file_paths": ["public.rs", "other.rs"]}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn deny_beats_allow() {
        let e = eng(Mode::Default, &["Read(./.env)"], &["Read"], &[]);
        assert!(matches!(
            e.check(
                "Read",
                &json!({"file_path": "./.env"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Deny(_)
        ));
        assert!(matches!(
            e.check(
                "Read",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
    }

    #[test]
    fn session_grant_allows_without_reasking() {
        let e = eng(Mode::Default, &[], &[], &[]);
        let grant = vec!["Bash(cargo test:*)".to_string()];
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "cargo test --lib"}),
                &cwd(),
                &roots(),
                &grant
            ),
            Decision::Allow
        ));
    }

    #[test]
    fn bypass_checker_allows_but_denies_catastrophic() {
        let b = BypassChecker;
        assert!(matches!(
            b.check(
                "Bash",
                &json!({"command": "echo hi"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        assert!(matches!(
            b.check(
                "Bash",
                &json!({"command": "rm -rf /"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn resolve_within_rejects_paths_outside_roots() {
        let roots = vec![std::env::temp_dir()];
        let cwd = std::env::temp_dir();
        // /etc/passwd exists on macOS/Linux and is not under the temp root.
        let res = crate::path::resolve_within(&roots, &cwd, "/etc/passwd");
        assert!(
            res.is_err(),
            "expected an outside-roots refusal, got {res:?}"
        );
        // The error names the allowed roots so the model can self-correct.
        let err = res.unwrap_err();
        assert!(
            err.contains("allowed:"),
            "missing allowed-roots hint: {err}"
        );
    }

    /// Switching the engine to `auto` must stop it asking — including for the
    /// Bash commands that otherwise escalate (a `$(...)` substitution here).
    /// This is the enforcement half of the "bypass isn't working" report; the
    /// other half was the mode never reaching the engine at startup, fixed in
    /// `rc-cli`'s `reconcile_mode`.
    #[test]
    fn auto_mode_stops_asking_once_set_live() {
        let e = eng(Mode::Default, &[], &[], &[]);
        assert!(
            matches!(
                e.check(
                    "Bash",
                    &json!({"command": "echo $(date)"}),
                    &cwd(),
                    &roots(),
                    &[]
                ),
                Decision::Ask(_)
            ),
            "default escalates a substitution"
        );
        e.set_mode(Mode::Auto);
        assert!(
            matches!(
                e.check(
                    "Bash",
                    &json!({"command": "echo $(date)"}),
                    &cwd(),
                    &roots(),
                    &[]
                ),
                Decision::Allow
            ),
            "auto must not ask"
        );
        assert!(matches!(
            e.check(
                "Write",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
        // Still refuses the catastrophic set — "auto" is not "unguarded".
        assert!(matches!(
            e.check(
                "Bash",
                &json!({"command": "rm -rf /"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn set_mode_changes_the_default_decision_live() {
        // The TUI's Shift+Tab calls set_mode to swap the engine's mode atomically;
        // the next check sees it without rebuilding the engine.
        let e = eng(Mode::Default, &[], &[], &[]);
        assert!(matches!(
            e.check(
                "Edit",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Ask(_)
        ));
        e.set_mode(Mode::Plan);
        assert!(matches!(
            e.check(
                "Edit",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Deny(_)
        ));
        // Non-mutating tools still allow in Plan mode.
        assert!(matches!(
            e.check(
                "Read",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
    }

    #[test]
    fn allow_all_set_mode_is_a_no_op() {
        // AllowAllChecker keeps the trait's default set_mode (mode-independent).
        let a = AllowAllChecker;
        a.set_mode(Mode::Plan);
        assert!(matches!(
            a.check(
                "Edit",
                &json!({"file_path": "/tmp/x"}),
                &cwd(),
                &roots(),
                &[]
            ),
            Decision::Allow
        ));
    }
}
