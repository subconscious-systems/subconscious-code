//! PowerShell approvals are deliberately separate from the POSIX tokenizer.
//! Only whole-tool or byte-exact command grants are supported. Prefix grants
//! cannot safely describe PowerShell expressions, pipelines, or script blocks.

use super::{Decision, Mode, PermissionEngine, Rule};
use serde_json::Value;

pub fn exact_grant(command: &str) -> String {
    let hex: String = command
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("PowerShell(exact:{hex})")
}

fn valid_spec(spec: &str) -> bool {
    spec.strip_prefix("exact:").is_some_and(|hex| {
        !hex.is_empty() && hex.len() % 2 == 0 && hex.bytes().all(|c| c.is_ascii_hexdigit())
    })
}

fn matches(rule: &Rule, command: &str) -> bool {
    rule.tool == "PowerShell"
        && rule
            .spec
            .as_ref()
            .is_none_or(|spec| format!("PowerShell({spec})") == exact_grant(command))
}

/// A conservative safety floor, not a parser or security sandbox. Approval is
/// still required for every ungranted script in the normal permission modes.
pub fn is_catastrophic(command: &str) -> bool {
    let text = command.to_lowercase().replace(['\'', '"', '`'], "");
    if [
        "format-volume",
        "clear-disk",
        "initialize-disk",
        "format.com",
    ]
    .iter()
    .any(|word| text.contains(word))
    {
        return true;
    }
    let words: Vec<_> = text
        .split(|c: char| c.is_whitespace() || ";|&()".contains(c))
        .filter(|s| !s.is_empty())
        .collect();
    let deleting = words.iter().any(|word| {
        matches!(
            *word,
            "remove-item" | "ri" | "rm" | "rmdir" | "rd" | "del" | "erase"
        )
    });
    deleting
        && words.iter().any(|word| {
            let path = word.replace('/', "\\");
            matches!(
                path.as_str(),
                "\\" | "\\*"
                    | "*"
                    | "$home"
                    | "$home\\*"
                    | "$env:userprofile"
                    | "$env:userprofile\\*"
                    | "$env:systemroot"
                    | "$env:windir"
            ) || (path.as_bytes().get(1) == Some(&b':')
                && matches!(path.get(2..), Some("\\" | "\\*" | "")))
        })
}

pub(super) fn bypass(input: &Value) -> Decision {
    match input.get("command").and_then(Value::as_str) {
        Some(command) if !command.trim().is_empty() && !is_catastrophic(command) => Decision::Allow,
        _ => Decision::Deny("missing or destructive PowerShell command refused".into()),
    }
}

impl PermissionEngine {
    pub(super) fn powershell_check(&self, input: &Value, grants: &[Rule], mode: Mode) -> Decision {
        let Some(command) = input
            .get("command")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        else {
            return Decision::Deny("PowerShell call without a command".into());
        };
        if is_catastrophic(command) {
            return Decision::Deny("destructive PowerShell command refused".into());
        }
        if mode == Mode::Plan {
            return Decision::Deny("PowerShell is disabled in plan mode".into());
        }
        for rule in self
            .deny
            .iter()
            .chain(&self.allow)
            .chain(&self.ask)
            .chain(grants)
        {
            if rule.tool == "PowerShell" && rule.spec.as_ref().is_some_and(|s| !valid_spec(s)) {
                return Decision::Deny("unsupported PowerShell rule: use exact-command approvals, not Bash-style prefixes".into());
            }
        }
        if self.deny.iter().any(|rule| matches(rule, command)) {
            return Decision::Deny("PowerShell denied by a rule".into());
        }
        if mode == Mode::Auto {
            return Decision::Allow;
        }
        if self.ask.iter().any(|rule| matches(rule, command)) {
            return Decision::Ask("PowerShell confirmation required by a rule".into());
        }
        if grants
            .iter()
            .chain(&self.allow)
            .any(|rule| matches(rule, command))
        {
            return Decision::Allow;
        }
        Decision::Ask("PowerShell requires approval of the complete script".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PermissionChecker;
    use serde_json::json;
    use std::path::Path;

    fn check(mode: Mode, command: &str, grants: &[String]) -> Decision {
        PermissionEngine::new(mode, vec![], vec![], vec![]).check(
            "PowerShell",
            &json!({"command": command}),
            Path::new("C:\\work"),
            &[],
            grants,
        )
    }

    #[test]
    fn powershell_is_mutating_and_bash_grants_do_not_apply() {
        for mode in [Mode::Default, Mode::AcceptEdits, Mode::Ask] {
            assert!(matches!(
                check(mode, "Get-Location", &["Bash".into()]),
                Decision::Ask(_)
            ));
        }
        assert!(matches!(
            check(Mode::Plan, "Get-Location", &["PowerShell".into()]),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check(Mode::Auto, "Get-Location", &[]),
            Decision::Allow
        ));
    }

    #[test]
    fn exact_grants_do_not_allow_command_extension_or_substitution() {
        let script = "Write-Output 'hello (world)'\nGet-Location";
        let grants = [exact_grant(script)];
        assert!(matches!(
            check(Mode::Default, script, &grants),
            Decision::Allow
        ));
        for suffix in ["; Remove-Item x", "\n& other.exe", " | Invoke-Expression"] {
            assert!(matches!(
                check(Mode::Default, &format!("{script}{suffix}"), &grants),
                Decision::Ask(_)
            ));
        }
        assert!(matches!(
            check(
                Mode::Default,
                script,
                &["PowerShell(Write-Output:*)".into()]
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn destructive_floor_and_deny_rules_override_grants() {
        for script in [
            "Remove-Item -Recurse C:\\",
            "rm -r $HOME",
            "Clear-Disk 0",
            "cmd /c rd /s /q C:\\",
        ] {
            assert!(matches!(
                check(Mode::Auto, script, &["PowerShell".into()]),
                Decision::Deny(_)
            ));
            assert!(matches!(
                bypass(&json!({"command": script})),
                Decision::Deny(_)
            ));
        }
        let engine =
            PermissionEngine::new(Mode::Default, vec!["PowerShell".into()], vec![], vec![]);
        assert!(matches!(
            engine.check(
                "PowerShell",
                &json!({"command": "Get-Date"}),
                Path::new("."),
                &[],
                &[exact_grant("Get-Date")]
            ),
            Decision::Deny(_)
        ));
        assert!(!is_catastrophic(
            "Remove-Item -LiteralPath 'C:\\work\\scratch.txt'"
        ));
    }
}
