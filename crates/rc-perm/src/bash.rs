//! Bash command parsing for permission matching (§7.2). We parse, never regex
//! the raw string: split on `;`/`&&`/`||`/`|`/`&`/newlines (quote-aware),
//! tokenize each sub-command with `shlex`, and require *every* sub-command to
//! independently match an allow rule. Anything we can't confidently parse
//! (substitution, `eval`, `exec`, `source`, unbalanced quotes) escalates to
//! Ask — fail closed.

/// One parsed write-redirection target (e.g. `log.txt` from `> log.txt`
/// or `err.log` from `2>>err.log`). Pure fd duplication (`2>&1`) is not a
/// file write and is deliberately *not* collected — it needs no policing.
#[derive(Debug, Clone)]
pub struct Sub {
    pub raw: String,
    pub tokens: Vec<String>,
    /// Write-redirection targets (`>`, `>>`, `N>`, `N>>`, `&>`) in this
    /// sub-command. The permission engine polices these separately from the
    /// command itself, because a rule granted on the command (`Bash(echo:*)`)
    /// must not silently cover `echo x > ~/.ssh/authorized_keys`.
    pub redirect_targets: Vec<String>,
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
                // `>&` / `2>&` is an fd-duplication operator, not a
                // backgrounding `&` — `cargo build 2>&1` must stay one
                // sub-command or the redirect scanner sees a broken tail.
                if c == '&' && cur.ends_with('>') {
                    cur.push(c);
                    i += 1;
                    continue;
                }
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
        return ParsedBash {
            subcommands: vec![],
            unparseable: true,
        };
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
            return ParsedBash {
                subcommands: vec![],
                unparseable: true,
            };
        }
        let Some(tokens) = shlex::split(trimmed) else {
            return ParsedBash {
                subcommands: vec![],
                unparseable: true,
            };
        };
        if tokens.is_empty() {
            continue;
        }
        let redirect_targets = match collect_redirect_targets(&tokens) {
            Ok(t) => t,
            // `echo x >` — a redirection with no target is a shell syntax error.
            // We can't say what it writes to, so fail closed.
            Err(()) => {
                return ParsedBash {
                    subcommands: vec![],
                    unparseable: true,
                }
            }
        };
        subs.push(Sub {
            raw: trimmed.to_string(),
            tokens,
            redirect_targets,
        });
    }
    ParsedBash {
        subcommands: subs,
        unparseable: false,
    }
}

/// What a token means as a write redirection, if it is one.
enum Redirect<'a> {
    /// `>`, `>>`, `2>`, `&>` — the target is the *next* token.
    Next,
    /// `>file`, `2>>log`, `&>out` — the target is attached to the operator.
    Attached(&'a str),
    /// `2>&1`, `>&2` — fd duplication, not a file write. Not policed.
    Dup,
}

/// Classify one shlex token as a write redirection. shlex has already removed
/// quotes, so a token that *starts* with the operator here was unquoted in the
/// original command — this cannot mistake a quoted argument like `">"` for a
/// live redirection (and when in doubt we treat it as one, fail closed).
fn write_redirect(tok: &str) -> Option<Redirect<'_>> {
    // `N>` / `N>>` style: skip leading fd digits (`2>`, `12>>`).
    let mut b = tok;
    while b.starts_with(|c: char| c.is_ascii_digit()) {
        b = &b[1..];
    }
    if let Some(rest) = b.strip_prefix("&>") {
        // `>&1` / `2>&2` is fd duplication when the word is all digits;
        // `&>file` / `>&file` redirects stdout+stderr to a file.
        return Some(if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            Redirect::Dup
        } else if rest.is_empty() {
            Redirect::Next
        } else {
            Redirect::Attached(rest)
        });
    }
    let rest = b.strip_prefix('>')?;
    if let Some(dst) = rest.strip_prefix('&') {
        // `>&1` / `2>&2` is a duplication when the word is all digits.
        return if !dst.is_empty() && dst.chars().all(|c| c.is_ascii_digit()) {
            Some(Redirect::Dup)
        } else {
            // `>&word` redirects to a file named `word` — a write.
            Some(Redirect::Attached(dst))
        };
    }
    let rest = rest.strip_prefix('>').unwrap_or(rest); // `>>` and `>>file`
    Some(if rest.is_empty() {
        Redirect::Next
    } else {
        Redirect::Attached(rest)
    })
}

/// Collect every write-redirection target in a token list. `Err(())` when a
/// bare redirect operator is the last token (no target — shell syntax error;
/// refuse to guess).
fn collect_redirect_targets(tokens: &[String]) -> Result<Vec<String>, ()> {
    let mut targets = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        match write_redirect(tok) {
            Some(Redirect::Next) => {
                let target = tokens.get(i + 1).ok_or(())?;
                targets.push(target.clone());
            }
            Some(Redirect::Attached(t)) => targets.push(t.to_string()),
            Some(Redirect::Dup) | None => {}
        }
    }
    Ok(targets)
}

/// A redirection target that is never a dangerous write. `2>/dev/null` is the
/// canonical suppression idiom; whitelisting it in the permission engine
/// keeps routine commands from asking.
pub(crate) const BENIGN_REDIRECT_TARGETS: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/stdout",
    "/dev/stderr",
];

/// Is `t` a target whose deletion (`rm -rf`) would be catastrophic: the
/// filesystem root, anything absolute (over-refusal is intended — the raw
/// fallback has always refused `rm -rf /home`), the home dir or its shell
/// variables, or a parent directory?
fn is_rootish_target(t: &str) -> bool {
    t == "/"
        || t.starts_with('/')
        || t == "~"
        || t.starts_with("~/")
        || t == ".."
        || t.starts_with("../")
        || matches!(t, "$HOME" | "${HOME}" | "$PWD" | "${PWD}")
        || t.starts_with("$HOME/")
        || t.starts_with("${HOME}/")
}

/// Token-based catastrophic detection: flag-insensitive `rm` with recursive
/// *and* force targeting a root-ish path, `mkfs*`, `dd of=/dev/…`, recursive
/// world-writable `chmod` on a root-ish path, and the system-halt family.
/// Token matching (rather than substring) is the whole point: `rm -rf "/"`,
/// `rm  -rf  /`, and `rm --recursive --force /` all land here where the old
/// substring list let each through.
fn is_catastrophic_tokens(tokens: &[String]) -> bool {
    let Some(first) = tokens.first() else {
        return false;
    };
    let name = first.rsplit('/').next().unwrap_or(first);
    match name {
        "rm" => {
            let mut recursive = false;
            let mut force = false;
            for t in &tokens[1..] {
                if let Some(f) = t.strip_prefix("--") {
                    match f {
                        "recursive" => recursive = true,
                        "force" => force = true,
                        _ => {}
                    }
                } else if let Some(f) = t.strip_prefix('-') {
                    // Combined short flags: -r -f -rf -fr -Rf -fR …
                    if !f.is_empty() && f.chars().all(|c| "rRfF".contains(c)) {
                        if f.contains('r') || f.contains('R') {
                            recursive = true;
                        }
                        if f.contains('f') || f.contains('F') {
                            force = true;
                        }
                    }
                }
            }
            recursive
                && force
                && tokens[1..]
                    .iter()
                    .filter(|t| !t.starts_with('-'))
                    .any(|t| is_rootish_target(t))
        }
        // Any mkfs / mkfs.* filesystem builder.
        n if n == "mkfs" || n.starts_with("mkfs.") => true,
        "dd" => tokens
            .iter()
            .any(|t| t.strip_prefix("of=").is_some_and(|v| v.starts_with("/dev/"))),
        "chmod" => {
            let recursive = tokens.iter().any(|t| t == "-R" || t == "--recursive");
            recursive
                && tokens.iter().any(|t| t == "777")
                && tokens
                    .iter()
                    .filter(|t| !t.starts_with('-'))
                    .any(|t| is_rootish_target(t))
        }
        "shutdown" | "reboot" | "halt" => true,
        "init" => tokens.iter().any(|t| t == "0"),
        _ => false,
    }
}

/// The raw-pattern fallback for commands `shlex` can't tokenize (unbalanced
/// quotes) and for the fork bomb, whose `:`/`|`/`&` salad is meaningless as
/// tokens. Quotes, braces, and whitespace runs are normalized away so the
/// quoted and double-spaced variants fold onto the plain spellings.
/// Command-name patterns (`shutdown`, `mkfs`, `dd …`) are deliberately NOT
/// raw substrings — the token pass matches command names exactly, so
/// `cat shutdown-plan.md` is no longer false-denied.
const CATASTROPHIC_RAW: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf /*",
    "rm -fr /",
    "rm -fr ~",
    "rm -fr /*",
    "rm --recursive --force /",
    "rm --force --recursive /",
    "rm -rf $HOME",
    "rm -rf $PWD",
    "chmod -R 777 /",
    ":(){:|:&};:",
];

fn normalize_for_raw_match(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut in_space = false;
    for c in cmd.chars() {
        if c == '"' || c == '\'' || c == '{' || c == '}' {
            continue;
        }
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// Catastrophic commands are always denied, even in bypass mode.
pub fn is_catastrophic(sub: &Sub) -> bool {
    is_catastrophic_tokens(&sub.tokens)
        || CATASTROPHIC_RAW.iter().any(|p| sub.raw.contains(p))
}

/// Catastrophic commands checked against the raw command string — catches
/// destructive commands even when [`parse_bash`] can't tokenize them (e.g.
/// `rm -rf $HOME`, which contains `$` so the engine escalates to Ask). Used by
/// the permission engine so a catastrophic command is denied in every mode,
/// including bypass, before the bypass-allow short-circuit.
///
/// Token-first with a normalized raw fallback: the token pass catches quoted,
/// double-spaced, and long-flag spellings (`rm -rf "/"`, `rm  -rf  /`,
/// `rm --recursive --force /`); the raw pass catches what can't be tokenized
/// at all.
pub fn is_catastrophic_cmd(cmd: &str) -> bool {
    for raw in split_subcommands(cmd) {
        if let Some(tokens) = shlex::split(raw.trim()) {
            if is_catastrophic_tokens(&tokens) {
                return true;
            }
        }
    }
    let norm = normalize_for_raw_match(cmd);
    CATASTROPHIC_RAW.iter().any(|p| norm.contains(p))
}

const ALWAYS_ASK_TOKENS: &[&str] = &["sudo", "--force"];
/// Raw fallback markers for commands shlex can't tokenize; the token pass
/// below uses exact token equality (so `cat notes-on-sudoers.txt` and a path
/// containing `--force` no longer false-trigger).
const ALWAYS_ASK_RAW: &[&str] = &["sudo", "--force", "| sh", "| bash", "|sh", "|bash"];

/// Commands that always escalate to Ask (§7.2), regardless of rules.
pub fn is_always_ask(cmd: &str) -> bool {
    let subs = split_subcommands(cmd);
    let mut any_parsed = false;
    for (i, raw) in subs.iter().enumerate() {
        let Some(tokens) = shlex::split(raw.trim()) else {
            continue;
        };
        any_parsed = true;
        if tokens.iter().any(|t| ALWAYS_ASK_TOKENS.contains(&t.as_str())) {
            return true;
        }
        // A shell script as a pipe destination (`curl … | sh`): any
        // sub-command *after the first* starting with `sh`/`bash` is fed the
        // output of everything before it. A lone `bash script.sh` (single
        // sub-command) is ordinary and stays unflagged.
        if i > 0
            && tokens
                .first()
                .is_some_and(|t| t == "sh" || t == "bash" || t.ends_with("/sh") || t.ends_with("/bash"))
        {
            return true;
        }
    }
    // Unparsable → conservative raw matching on the original markers.
    if !any_parsed {
        return ALWAYS_ASK_RAW.iter().any(|p| cmd.contains(p));
    }
    false
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
        assert!(
            !p.unparseable,
            "expected parseable, got unparseable for {cmd:?}"
        );
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
        assert!(
            !rule_matches("git status -s", &s[0]),
            "extra tokens break exact match"
        );
        assert!(
            !rule_matches("git", &s[0]),
            "fewer tokens break exact match"
        );
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
        // `rm -rf /home` is refused as badly-destructive too — over-refuse is
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

    // ---- Catastrophic floor: the quoting/spacing/long-flag bypasses from the
    // 2026-09 review, each of which slipped past the old substring list. ----

    #[test]
    fn is_catastrophic_cmd_survives_quoting_spacing_and_long_flags() {
        for cmd in [
            r#"rm -rf "/""#,
            "rm -rf '/'",
            "rm  -rf  /",
            "rm --recursive --force /",
            "rm --force --recursive /",
            "rm -rf ${HOME}",
            "rm -F -r /",
            "rm -fR /",
            "mkfs.ext4 /dev/sda0",
            "shutdown",
            "reboot",
            "rm -rf $HOME",
        ] {
            assert!(is_catastrophic_cmd(cmd), "must refuse {cmd:?}");
        }
    }

    #[test]
    fn is_catastrophic_cmd_no_longer_substring_matches_arguments() {
        // Word-shaped haystacks that the old substring list false-denied.
        assert!(!is_catastrophic_cmd("cat shutdown-plan.md"));
        assert!(!is_catastrophic_cmd("echo mkfs-notes"));
        assert!(!is_catastrophic_cmd("grep shutdown build.log"));
    }

    #[test]
    fn catastrophic_rm_needs_both_recursive_and_force() {
        // Only one of the two flags → ordinary command, rules decide.
        assert!(!is_catastrophic_cmd("rm -r /build"));
        assert!(!is_catastrophic_cmd("rm -f /build"));
        // Non-root-ish target.
        assert!(!is_catastrophic_cmd("rm -rf ./build"));
    }

    // ---- Redirection extraction ----

    #[test]
    fn write_redirect_classification() {
        assert!(matches!(write_redirect(">"), Some(Redirect::Next)));
        assert!(matches!(write_redirect(">>"), Some(Redirect::Next)));
        assert!(matches!(write_redirect("2>"), Some(Redirect::Next)));
        assert!(matches!(write_redirect("&>"), Some(Redirect::Next)));
        assert!(matches!(write_redirect(">log"), Some(Redirect::Attached("log"))));
        assert!(matches!(write_redirect(">>log"), Some(Redirect::Attached("log"))));
        assert!(matches!(write_redirect("2>err"), Some(Redirect::Attached("err"))));
        assert!(matches!(write_redirect("12>&3"), Some(Redirect::Dup)));
        assert!(matches!(write_redirect("2>&1"), Some(Redirect::Dup)));
        assert!(matches!(write_redirect(">&1"), Some(Redirect::Dup)));
        assert!(matches!(write_redirect("&>both"), Some(Redirect::Attached("both"))));
        assert!(write_redirect("file.txt").is_none());
        assert!(write_redirect("--force").is_none());
    }

    #[test]
    fn parse_collects_bare_and_attached_redirect_targets() {
        let s = subs("echo x > out.txt");
        assert_eq!(s[0].redirect_targets, vec!["out.txt".to_string()]);
        let s = subs("cargo build 2>>err.log >build.log");
        assert!(s[0].redirect_targets.iter().any(|t| t == "err.log"));
        assert!(s[0].redirect_targets.iter().any(|t| t == "build.log"));
        let s = subs("cp a b 2>/dev/null");
        assert_eq!(s[0].redirect_targets, vec!["/dev/null".to_string()]);
    }

    #[test]
    fn fd_duplication_is_not_a_write_target() {
        let s = subs("cargo build 2>&1");
        assert!(s[0].redirect_targets.is_empty());
    }

    #[test]
    fn trailing_bare_redirect_incomplete_command_is_refused() {
        // `echo x >` is a shell syntax error; the parser must not guess.
        let p = parse_bash("echo x >");
        assert!(p.unparseable, "no target on bare redirect: {p:?}");
    }

    #[test]
    fn quoted_redirect_operator_is_still_extracted_fail_closed() {
        // shlex drops quote information, so a literal `>` argument in an
        // edited string can look like a redirect. False Ask is the safe
        // direction — the user sees the prompt.
        let p = parse_bash(r#"echo ">" file"#);
        assert!(!p.unparseable);
        assert_eq!(
            p.subcommands[0].redirect_targets,
            vec!["file".to_string()],
            "ambiguous quoting must lean toward redirect"
        );
    }
}
