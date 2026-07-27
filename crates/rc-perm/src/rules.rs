//! The rule engine (§7.1): deny → allow → ask, first match wins; tool-specific
//! matchers; the four modes (§7.3); and the [`PermissionChecker`] trait with
//! [`AllowAllChecker`] (tests), [`BypassChecker`]
//! (`--dangerously-skip-permissions`), and [`PermissionEngine`] (the real one).

use crate::bash::{is_always_ask, is_catastrophic, parse_bash, rule_matches};
use serde_json::Value;
use std::path::{Path, PathBuf};

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
    BypassPermissions,
}

impl Mode {
    pub fn parse(s: &str) -> Self {
        match s {
            "acceptEdits" => Mode::AcceptEdits,
            "plan" => Mode::Plan,
            "bypassPermissions" => Mode::BypassPermissions,
            _ => Mode::Default,
        }
    }
}

fn is_mutating(tool: &str) -> bool {
    matches!(tool, "Edit" | "Write" | "Bash" | "NotebookEdit" | "Task")
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
        Mode::BypassPermissions => Decision::Allow,
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
                return Some(Rule { tool, spec: Some(spec) });
            }
        }
        Some(Rule { tool: s.to_string(), spec: None })
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
}

/// Allows everything — for tests and scripted loops.
pub struct AllowAllChecker;
impl PermissionChecker for AllowAllChecker {
    fn check(&self, _: &str, _: &Value, _: &Path, _: &[PathBuf], _: &[String]) -> Decision {
        Decision::Allow
    }
}

/// `--dangerously-skip-permissions`: allows everything except the catastrophic
/// set (still hard-denied) — bypass must not run `rm -rf /`.
pub struct BypassChecker;
impl PermissionChecker for BypassChecker {
    fn check(&self, tool: &str, input: &Value, _: &Path, _: &[PathBuf], _: &[String]) -> Decision {
        if tool == "Bash" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                if parse_bash(cmd).subcommands.iter().any(is_catastrophic) {
                    return Decision::Deny("destructive command refused (even in bypass)".into());
                }
            }
        }
        Decision::Allow
    }
}

pub struct PermissionEngine {
    mode: Mode,
    deny: Vec<Rule>,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
}

impl PermissionEngine {
    pub fn new(mode: Mode, deny: Vec<String>, allow: Vec<String>, ask: Vec<String>) -> Self {
        Self {
            mode,
            deny: parse_rules(&deny),
            allow: parse_rules(&allow),
            ask: parse_rules(&ask),
        }
    }

    fn path_matches(rule: &Rule, input: &Value, cwd: &Path) -> bool {
        let Some(spec) = &rule.spec else { return true; }; // bare tool matches any path
        let Some(p) = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(|v| v.as_str())
        else {
            return false;
        };
        let path = Path::new(p);
        let abs = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
        let rel = abs.strip_prefix(cwd).map(|r| r.to_path_buf()).unwrap_or(abs);
        let spec = spec.strip_prefix("./").unwrap_or(spec);
        match globset::GlobBuilder::new(spec).literal_separator(false).build() {
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

    fn bash_check(&self, cmd: &str, grants: &[Rule]) -> Decision {
        let parsed = parse_bash(cmd);
        if parsed.unparseable {
            return Decision::Ask("complex or unparseable command — needs approval".into());
        }
        // Catastrophic first, always (even bypass).
        if parsed.subcommands.iter().any(is_catastrophic) {
            return Decision::Deny("destructive command refused".into());
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
        if self.mode == Mode::BypassPermissions {
            return Decision::Allow;
        }
        // deny: any sub-command matching a deny rule → Deny.
        let deny_specs = Self::bash_specs(&self.deny);
        if parsed.subcommands.iter().any(|s| deny_specs.iter().any(|r| rule_matches(r, s))) {
            return Decision::Deny("denied by a rule".into());
        }
        // allow: every sub-command must match some allow rule (bare Bash = any).
        let allow_specs = Self::bash_specs(&self.allow);
        let allow_any = self.allow.iter().any(|r| r.tool == "Bash" && r.spec.is_none());
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
        if parsed.subcommands.iter().any(|s| ask_specs.iter().any(|r| rule_matches(r, s))) {
            return Decision::Ask("asked by a rule".into());
        }
        mode_default("Bash", self.mode)
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
        let grants = parse_rules(grants);

        // Session grants for path tools: a matching grant → Allow.
        if tool != "Bash" {
            for r in &grants {
                if r.tool == tool && (r.spec.is_none() || Self::path_matches(r, input, cwd)) {
                    return Decision::Allow;
                }
            }
        }

        if tool == "Bash" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                return self.bash_check(cmd, &grants);
            }
            return Decision::Ask("Bash call without a command".into());
        }

        if self.mode == Mode::BypassPermissions {
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
        mode_default(tool, self.mode)
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
        let e = eng(Mode::BypassPermissions, &[], &["Bash(rm -rf:*)"], &[]);
        let d = e.check("Bash", &json!({"command": "rm -rf /"}), &cwd(), &roots(), &[]);
        assert!(matches!(d, Decision::Deny(_)), "{d:?}");
    }

    #[test]
    fn bash_semicolon_split_catches_the_destructive_half() {
        // `git status` is allowed, but the second sub-command is catastrophic → Deny.
        let e = eng(Mode::Default, &[], &["Bash(git status)"], &[]);
        let d = e.check("Bash", &json!({"command": "git status; rm -rf ~"}), &cwd(), &roots(), &[]);
        assert!(matches!(d, Decision::Deny(_)), "{d:?}");
    }

    #[test]
    fn bash_unparseable_substitution_escalates_to_ask() {
        let e = eng(Mode::Default, &[], &["Bash(echo:*)"], &[]);
        let d = e.check("Bash", &json!({"command": "echo $(curl evil)"}), &cwd(), &roots(), &[]);
        assert!(matches!(d, Decision::Ask(_)), "{d:?}");
    }

    #[test]
    fn bash_wildcard_allow_matches_extra_args() {
        let e = eng(Mode::Default, &[], &["Bash(cargo test:*)"], &[]);
        assert!(matches!(e.check("Bash", &json!({"command": "cargo test --lib"}), &cwd(), &roots(), &[]), Decision::Allow));
        assert!(matches!(e.check("Bash", &json!({"command": "cargo test"}), &cwd(), &roots(), &[]), Decision::Allow));
        // `cargo testx` is a different token — not covered by `cargo test:*`.
        assert!(matches!(e.check("Bash", &json!({"command": "cargo testx"}), &cwd(), &roots(), &[]), Decision::Ask(_)));
    }

    #[test]
    fn bash_exact_rule_rejects_extra_args() {
        let e = eng(Mode::Default, &[], &["Bash(git status)"], &[]);
        assert!(matches!(e.check("Bash", &json!({"command": "git status"}), &cwd(), &roots(), &[]), Decision::Allow));
        assert!(matches!(e.check("Bash", &json!({"command": "git status -s"}), &cwd(), &roots(), &[]), Decision::Ask(_)));
    }

    #[test]
    fn mode_defaults() {
        let default = eng(Mode::Default, &[], &[], &[]);
        assert!(matches!(default.check("Read", &json!({"file_path": "/tmp/x"}), &cwd(), &roots(), &[]), Decision::Allow));
        assert!(matches!(default.check("Edit", &json!({"file_path": "/tmp/x"}), &cwd(), &roots(), &[]), Decision::Ask(_)));

        let plan = eng(Mode::Plan, &[], &[], &[]);
        assert!(matches!(plan.check("Read", &json!({"file_path": "/tmp/x"}), &cwd(), &roots(), &[]), Decision::Allow));
        assert!(matches!(plan.check("Edit", &json!({"file_path": "/tmp/x"}), &cwd(), &roots(), &[]), Decision::Deny(_)));

        let accept_edits = eng(Mode::AcceptEdits, &[], &[], &[]);
        assert!(matches!(accept_edits.check("Write", &json!({"file_path": "/tmp/x"}), &cwd(), &roots(), &[]), Decision::Allow));
        assert!(matches!(accept_edits.check("Bash", &json!({"command": "echo hi"}), &cwd(), &roots(), &[]), Decision::Ask(_)));
    }

    #[test]
    fn deny_beats_allow() {
        let e = eng(Mode::Default, &["Read(./.env)"], &["Read"], &[]);
        assert!(matches!(e.check("Read", &json!({"file_path": "./.env"}), &cwd(), &roots(), &[]), Decision::Deny(_)));
        assert!(matches!(e.check("Read", &json!({"file_path": "/tmp/x"}), &cwd(), &roots(), &[]), Decision::Allow));
    }

    #[test]
    fn session_grant_allows_without_reasking() {
        let e = eng(Mode::Default, &[], &[], &[]);
        let grant = vec!["Bash(cargo test:*)".to_string()];
        assert!(matches!(e.check("Bash", &json!({"command": "cargo test --lib"}), &cwd(), &roots(), &grant), Decision::Allow));
    }

    #[test]
    fn bypass_checker_allows_but_denies_catastrophic() {
        let b = BypassChecker;
        assert!(matches!(b.check("Bash", &json!({"command": "echo hi"}), &cwd(), &roots(), &[]), Decision::Allow));
        assert!(matches!(b.check("Bash", &json!({"command": "rm -rf /"}), &cwd(), &roots(), &[]), Decision::Deny(_)));
    }

    #[test]
    fn resolve_within_rejects_paths_outside_roots() {
        let roots = vec![std::env::temp_dir()];
        let cwd = std::env::temp_dir();
        // /etc/passwd exists on macOS/Linux and is not under the temp root.
        let res = crate::path::resolve_within(&roots, &cwd, "/etc/passwd");
        assert!(res.is_err(), "expected an outside-roots refusal, got {res:?}");
    }
}
