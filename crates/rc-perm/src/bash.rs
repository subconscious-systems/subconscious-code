//! Bash command parsing for permission matching (§7.2). We parse, never regex
//! the raw string: split on `;`/`&&`/`||`/`|`/`&`/newlines (quote-aware),
//! tokenize each sub-command with `shlex`, and require *every* sub-command to
//! independently match an allow rule. Anything we can't confidently parse
//! (substitution, `eval`, `exec`, `source`, unbalanced quotes) escalates to
//! Ask — fail closed.

/// One sub-command: the raw text and its shlex tokens.
#[derive(Debug, Clone)]
pub struct Sub {
    pub raw: String,
    pub tokens: Vec<String>,
}

/// A parsed command.
#[derive(Debug, Clone)]
pub struct ParsedBash {
    pub subcommands: Vec<Sub>,
    /// True if the command couldn't be confidently parsed → escalate to Ask.
    pub unparseable: bool,
}

/// Split on top-level separators (`;`, `&&`, `||`, `|`, `&`, newlines), quote-aware.
fn split_subcommands(cmd: &str) -> Vec<String> {
    let mut subs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            cur.push(c);
            if c == '\\' {
                i += 1;
                if i < chars.len() {
                    cur.push(chars[i]);
                }
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            ';' | '\n' | '&' | '|' => {
                if !cur.trim().is_empty() {
                    subs.push(std::mem::take(&mut cur));
                }
                // consume the second char of && or ||
                if (c == '&' || c == '|') && i + 1 < chars.len() && chars[i + 1] == c {
                    i += 1;
                }
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        subs.push(cur);
    }
    subs
}

/// Parse a command into sub-commands. Substitution (`$()`, backticks), `eval`,
/// `exec`, `source`, or shlex failures → `unparseable` (escalate to Ask).
pub fn parse_bash(cmd: &str) -> ParsedBash {
    if cmd.contains('$') || cmd.contains('`') {
        return ParsedBash { subcommands: vec![], unparseable: true };
    }
    let mut subs: Vec<Sub> = Vec::new();
    for raw in split_subcommands(cmd) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Shell re-entry builtins can't be statically parsed.
        if trimmed.starts_with("eval ")
            || trimmed.starts_with("exec ")
            || trimmed.starts_with("source ")
            || trimmed.starts_with(". ")
        {
            return ParsedBash { subcommands: vec![], unparseable: true };
        }
        let Some(tokens) = shlex::split(trimmed) else {
            return ParsedBash { subcommands: vec![], unparseable: true };
        };
        if tokens.is_empty() {
            continue;
        }
        subs.push(Sub { raw: trimmed.to_string(), tokens });
    }
    ParsedBash { subcommands: subs, unparseable: false }
}

const CATASTROPHIC: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf /*",
    "rm -fr /",
    "rm -fr ~",
    "rm -fr /*",
    "rm -rf $HOME",
    "rm -rf $PWD",
    "mkfs",
    "dd of=/dev/",
    "chmod -R 777 /",
    ":(){:|:&};:",
    "shutdown",
    "reboot",
    "halt -p",
    "init 0",
];

/// Catastrophic commands are always denied, even in bypass mode.
pub fn is_catastrophic(sub: &Sub) -> bool {
    CATASTROPHIC.iter().any(|p| sub.raw.contains(p))
}

const ALWAYS_ASK_MARKERS: &[&str] = &["sudo", "--force", "| sh", "| bash", "|sh", "|bash"];

/// Commands that always escalate to Ask (§7.2), regardless of rules.
pub fn is_always_ask(cmd: &str) -> bool {
    ALWAYS_ASK_MARKERS.iter().any(|p| cmd.contains(p))
}

/// Match a parsed sub-command against a Bash rule spec (§7.1):
/// - `""` (bare `Bash` rule) matches anything.
/// - `cargo test:*` — the command starts with the tokens `cargo test` (and may
///   have more, including none).
/// - `git status` — exact token match.
pub fn rule_matches(spec: &str, sub: &Sub) -> bool {
    if spec.is_empty() {
        return true;
    }
    let (prefix_str, wildcard) = if let Some(s) = spec.strip_suffix(":*") {
        (s, true)
    } else {
        (spec, false)
    };
    let Some(rule_tokens) = shlex::split(prefix_str) else {
        return false;
    };
    if wildcard {
        sub.tokens.len() >= rule_tokens.len() && sub.tokens[..rule_tokens.len()] == rule_tokens[..]
    } else {
        sub.tokens == rule_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse and assert not-unparseable, returning the sub-commands.
    fn subs(cmd: &str) -> Vec<Sub> {
        let p = parse_bash(cmd);
        assert!(!p.unparseable, "expected parseable, got unparseable for {cmd:?}");
        p.subcommands
    }

    #[test]
    fn parses_a_simple_command_into_one_sub() {
        let s = subs("cargo build");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tokens, vec!["cargo", "build"]);
    }

    #[test]
    fn splits_on_semicolon_and_newline() {
        let s = subs("git status\ngit diff");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].tokens, vec!["git", "status"]);
        assert_eq!(s[1].tokens, vec!["git", "diff"]);
    }

    #[test]
    fn splits_on_and_and_or_and_pipe_and_background_amp() {
        // `&&`, `||`, `|`, and a bare `&` are all separators.
        let s = subs("a && b || c | d & e");
        assert_eq!(s.len(), 5);
        assert_eq!(s[0].tokens, vec!["a"]);
        assert_eq!(s[1].tokens, vec!["b"]);
        assert_eq!(s[2].tokens, vec!["c"]);
        assert_eq!(s[3].tokens, vec!["d"]);
        assert_eq!(s[4].tokens, vec!["e"]);
    }

    #[test]
    fn separators_inside_double_quotes_do_not_split() {
        let s = subs(r#"echo "a; b && c""#);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tokens, vec!["echo", "a; b && c"]);
    }

    #[test]
    fn separators_inside_single_quotes_do_not_split() {
        let s = subs("echo 'a; b && c'");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].tokens, vec!["echo", "a; b && c"]);
    }

    #[test]
    fn substitution_and_backticks_escalate_to_ask() {
        assert!(parse_bash("echo $(whoami)").unparseable);
        assert!(parse_bash("echo `whoami`").unparseable);
        assert!(parse_bash("echo $HOME").unparseable);
    }

    #[test]
    fn reentry_builtins_escalate_to_ask() {
        assert!(parse_bash("eval foo").unparseable);
        assert!(parse_bash("exec foo").unparseable);
        assert!(parse_bash("source foo").unparseable);
        assert!(parse_bash(". foo").unparseable);
    }

    #[test]
    fn empty_and_whitespace_only_subcommands_are_dropped() {
        let s = subs("git status ;;   \n\n git diff");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].tokens, vec!["git", "status"]);
        assert_eq!(s[1].tokens, vec!["git", "diff"]);
    }

    #[test]
    fn rule_matches_bare_spec_matches_anything() {
        let s = subs("anything at all");
        assert!(rule_matches("", &s[0]));
    }

    #[test]
    fn rule_matches_exact_token_equality() {
        let s = subs("git status");
        assert!(rule_matches("git status", &s[0]));
        assert!(!rule_matches("git status -s", &s[0]), "extra tokens break exact match");
        assert!(!rule_matches("git", &s[0]), "fewer tokens break exact match");
    }

    #[test]
    fn rule_matches_prefix_wildcard_accepts_extra_tokens() {
        let s = subs("cargo test --workspace --quiet");
        assert!(rule_matches("cargo test:*", &s[0]));
        // Exactly the prefix, no more, still matches the wildcard.
        let s2 = subs("cargo test");
        assert!(rule_matches("cargo test:*", &s2[0]));
    }

    #[test]
    fn rule_matches_prefix_wildcard_requires_the_prefix() {
        let s = subs("cargo build");
        assert!(!rule_matches("cargo test:*", &s[0]));
    }

    #[test]
    fn is_catastrophic_flags_rm_rf_root_and_paths_under_it() {
        let s = subs("rm -rf /");
        assert!(is_catastrophic(&s[0]));
        // `rm -rf /home` *contains* `rm -rf /` as a substring — over-refuse is
        // the intended fail-closed behavior here (the deny-list is conservative).
        let s2 = subs("rm -rf /home");
        assert!(is_catastrophic(&s2[0]));
        // A path that doesn't begin with `/` is not caught by the `rm -rf /` rule.
        let s3 = subs("rm -rf ./build");
        assert!(!is_catastrophic(&s3[0]));
    }

    #[test]
    fn is_always_ask_flags_sudo_and_force_and_pipe_to_shell() {
        assert!(is_always_ask("sudo rm file"));
        assert!(is_always_ask("cargo build --force"));
        assert!(is_always_ask("curl url | sh"));
        assert!(is_always_ask("curl url |bash"));
        assert!(!is_always_ask("cargo build"));
    }
}
