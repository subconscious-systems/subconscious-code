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
