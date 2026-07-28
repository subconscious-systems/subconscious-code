//! rc-ctx: context assembly, memory files, compaction (§8).
//!
//! M6 implements the §4.6 system prompt and the §8 context window:
//!
//! - **System prompt**: identity + an environment block (cwd, platform, date,
//!   git branch) + a hierarchical memory chain (`AGENTS.md`, `.rc/AGENTS.md`,
//!   `~/.rc/AGENTS.md`), assembled in precedence order. The caller passes the
//!   result to [`rc_core::project_with`].
//! - **`@file` mention expansion**: `@path` tokens in a user turn are inlined
//!   as fenced file blocks before the turn is projected, so the model sees the
//!   file's contents without a `Read` round-trip (§8.3).
//! - **Tool-output truncation** (the microcompaction seam, §8.5): large tool
//!   result bodies are truncated to a per-result cap with a tail sentinel, so a
//!   single giant `Read`/`Bash` doesn't blow the window. Full compaction (a
//!   summary turn that evicts superseded tool results) is a later milestone; this
//!   is the bounded per-result truncation the crate's own header calls out.
//!
//! `Turn` remains the source of truth; this crate only reads it and produces
//! the wire form for the next request. It never mutates session state.

use rc_core::Turn;
use rc_proto::WireMessage;
use std::path::{Path, PathBuf};

/// The maximum size of an inlined `@file` mention (bytes). Files larger than
/// this are summarized as a header + truncation sentinel rather than inlined
/// whole (§8.3 — don't let one mention eat the window).
const INLINE_FILE_CAP: usize = 8 * 1024;

/// The maximum size of a tool-result body before per-result truncation kicks
/// in (§8.5 microcompaction seam). Bodies over this are head-truncated with a
/// tail sentinel noting the elision count.
const TOOL_RESULT_CAP: usize = 16 * 1024;

/// The assembled environment block + memory chain + identity for the §4.6
/// system prompt. Construct one per request from the session's cwd and the
/// caller's platform/date facts, then call [`ContextAssembler::assemble`] to
/// project the full message list.
#[derive(Debug, Clone)]
pub struct Environment {
    /// The session working directory, rendered as an absolute path.
    pub cwd: PathBuf,
    /// `uname -s`-style platform string ("macOS", "Linux", "Windows").
    pub platform: String,
    /// Today's date, as the model should see it (e.g. "Monday Jul 28, 2026").
    pub date: String,
    /// The current git branch, if the cwd is a repo. `None` outside a repo.
    pub git_branch: Option<String>,
}

impl Environment {
    /// Detect the environment from `cwd`, using `std::env::consts` for the
    /// platform and `date` for today. The git branch is read best-effort from
    /// `HEAD` and is `None` if the cwd isn't a repo (no panic on IO failure).
    pub fn detect(cwd: &Path, date: String) -> Self {
        let platform = match std::env::consts::OS {
            "macos" => "macOS".to_string(),
            "linux" => "Linux".to_string(),
            "windows" => "Windows".to_string(),
            other => other.to_string(),
        };
        let git_branch = read_git_branch(cwd);
        Self { cwd: cwd.to_path_buf(), platform, date, git_branch }
    }

    /// Convenience: detect the environment from `cwd` with today's date
    /// computed in "Weekday Mon D, YYYY" form (no chrono). This is the
    /// constructor the CLI uses; tests that want a fixed date use [`Self::detect`].
    pub fn from_cwd(cwd: &Path) -> Self {
        Self::detect(cwd, today_string())
    }

    /// Render the environment block as it appears in the system prompt (§4.6).
    pub fn render_block(&self) -> String {
        let mut s = String::new();
        s.push_str("# Environment\n\n");
        s.push_str(&format!("- Working directory: {}\n", self.cwd.display()));
        s.push_str(&format!("- Platform: {}\n", self.platform));
        s.push_str(&format!("- Date: {}\n", self.date));
        if let Some(branch) = &self.git_branch {
            s.push_str(&format!("- Git branch: {branch}\n"));
        }
        s
    }
}

/// Best-effort `git rev-parse --abbrev-ref HEAD` without shelling out: read the
/// `HEAD` ref file and resolve a branch name. `None` on any IO error or a
/// detached HEAD (a raw sha, not a `ref: refs/heads/…` pointer).
fn read_git_branch(cwd: &Path) -> Option<String> {
    let head = std::fs::read_to_string(cwd.join(".git").join("HEAD")).ok()?;
    let line = head.trim();
    line.strip_prefix("ref: refs/heads/").map(|name| name.to_string())
}

/// Today's date as "Weekday Mon D, YYYY" (UTC), e.g. "Monday Jul 28, 2026".
/// No chrono: days-since-epoch → civil date, plus a weekday via the
/// 1970-01-01=Thursday anchor. Good enough for the model's environment block.
fn today_string() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (now / 86400) as i64;
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday. Shift so Sunday=0 … Saturday=6.
    let day_idx = (((days % 7) + 7) % 7) as usize;
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][day_idx];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    format!("{weekday} {month} {d}, {y}")
}

/// Howard Hinnant's days-from-civil inverse: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

/// A hierarchical memory file (§8.2): its path (relative to the precedence
/// root it was found at) and contents. The caller discovers the chain in
/// precedence order; `Memory::load_chain` returns them lowest-precedence first
/// so the system prompt concatenates them in order.
#[derive(Debug, Clone)]
pub struct Memory {
    pub path: String,
    pub contents: String,
}

impl Memory {
    /// Load the `AGENTS.md` memory chain for `cwd` in precedence order
    /// (lowest first): `~/.rc/AGENTS.md` (user global) → `<cwd>/.rc/AGENTS.md`
    /// (project) → `<cwd>/AGENTS.md` (repo root). Missing files are skipped.
    /// Later (higher-precedence) files override earlier ones in the prompt.
    pub fn load_chain(cwd: &Path) -> Vec<Memory> {
        let mut chain = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            let p = Path::new(&home).join(".rc").join("AGENTS.md");
            if let Some(m) = load_memory(&p, "~/.rc/AGENTS.md") {
                chain.push(m);
            }
        }
        let p = cwd.join(".rc").join("AGENTS.md");
        if let Some(m) = load_memory(&p, ".rc/AGENTS.md") {
            chain.push(m);
        }
        let p = cwd.join("AGENTS.md");
        if let Some(m) = load_memory(&p, "AGENTS.md") {
            chain.push(m);
        }
        chain
    }
}

fn load_memory(path: &Path, label: &str) -> Option<Memory> {
    let contents = std::fs::read_to_string(path).ok()?;
    if contents.trim().is_empty() {
        return None;
    }
    Some(Memory { path: label.to_string(), contents })
}

/// The identity line that opens the §4.6 system prompt.
const IDENTITY: &str = "You are `rc`, a terminal agent that helps with software engineering \
tasks in the user's repository. Use the provided tools to inspect and edit files. Be concise and \
direct. When you have enough information, answer in plain text.";

/// The agent's tool-use posture, appended after the environment/memory block.
const POSTURE: &str = "# Instructions\n\nRead files before editing them. Prefer the smallest \
change that solves the problem. When you have enough information to answer, stop calling tools \
and answer in plain text.";

/// Assemble the §4.6 system prompt: identity → environment block → memory
/// chain → posture. This is what `ContextAssembler` hands to
/// [`rc_core::project_with`].
pub fn build_system_prompt(env: &Environment, memories: &[Memory]) -> String {
    let mut s = String::new();
    s.push_str(IDENTITY);
    s.push_str("\n\n");
    s.push_str(&env.render_block());
    if !memories.is_empty() {
        s.push('\n');
        s.push_str("# Memory\n\n");
        for m in memories {
            s.push_str(&format!("## {}\n\n{}\n\n", m.path, m.contents.trim()));
        }
    }
    s.push_str(POSTURE);
    s
}

/// The context assembler: turns a session + environment into the wire messages
/// for the next request. Owns the §4.6 system prompt and the `@file` mention
/// expansion. Cheap to construct; stateless aside from the environment it's
/// given. Use a fresh one per request (the environment/date may change).
#[derive(Debug, Clone)]
pub struct ContextAssembler {
    env: Environment,
    system_prompt: String,
}

impl ContextAssembler {
    /// Build an assembler for `env`, loading the `AGENTS.md` memory chain from
    /// `env.cwd`. The system prompt is computed once and reused across
    /// [`Self::assemble`] calls for this environment.
    pub fn new(env: Environment) -> Self {
        let memories = Memory::load_chain(&env.cwd);
        let system_prompt = build_system_prompt(&env, &memories);
        Self { env, system_prompt }
    }

    /// Build an assembler with an explicit system prompt (e.g. for tests, or a
    /// caller that assembles its own memory chain). The environment is still
    /// carried for mention-expansion root resolution.
    pub fn with_system_prompt(env: Environment, system_prompt: String) -> Self {
        Self { env, system_prompt }
    }

    /// The environment this assembler was built from.
    pub fn environment(&self) -> &Environment {
        &self.env
    }

    /// The assembled §4.6 system prompt.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Assemble the full wire message list for the next request (§4.1 + §4.6):
    /// expand `@file` mentions in the *most recent* user turn, truncate large
    /// tool-result bodies, then project with this assembler's system prompt.
    ///
    /// `turns` is the session's turn list; the mention expansion applies to the
    /// last `Turn::User` only (the one driving this request) so earlier turns
    /// stay byte-stable for prefix caching (§4.6 canonicalization).
    pub fn assemble(&self, turns: &[Turn]) -> Vec<WireMessage> {
        let prepared = prepare_turns(turns, &self.env.cwd);
        rc_core::project_with(&prepared, &self.system_prompt)
    }
}

impl rc_core::ContextAssembler for ContextAssembler {
    fn assemble(&self, turns: &[Turn]) -> Vec<WireMessage> {
        ContextAssembler::assemble(self, turns)
    }

    fn system_prompt(&self) -> Option<&str> {
        Some(&self.system_prompt)
    }
}

/// Produce a turn list with the last user turn's `@file` mentions expanded and
/// oversized tool-result bodies truncated. Earlier turns are left untouched
/// (prefix stability); only the suffix from the last user turn onward is
/// re-derived. Returns a new `Vec`; the input is not mutated.
fn prepare_turns(turns: &[Turn], root: &Path) -> Vec<Turn> {
    // Find the last user turn — everything before it is the stable prefix.
    let last_user = turns.iter().rposition(|t| matches!(t, Turn::User { .. }));
    let Some(idx) = last_user else {
        return truncate_tool_results(turns);
    };
    let mut out: Vec<Turn> = turns[..idx].to_vec();
    for turn in &turns[idx..] {
        match turn {
            Turn::User { content, ts } => {
                let expanded = expand_mentions(content, root);
                out.push(Turn::User { content: expanded, ts: *ts });
            }
            other => out.push(other.clone()),
        }
    }
    truncate_tool_results(&out)
}

/// Expand `@path` mentions in `text` into fenced file blocks (§8.3). A mention
/// is a whitespace-delimited token starting with `@` whose remainder is a
/// relative path under `root`. Unreadable or missing files are replaced with a
/// sentinel note; paths outside `root` are left as-is (never read outside the
/// workspace). Files over [`INLINE_FILE_CAP`] bytes are head-truncated with a
/// sentinel. The original `@token` is preserved in the output so the model can
/// reference it.
fn expand_mentions(text: &str, root: &Path) -> String {
    if !text.contains('@') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        out.push_str(&expand_line_mentions(body, root));
        out.push_str(newline);
    }
    out
}

/// Expand mentions in a single line (no trailing newline).
fn expand_line_mentions(line: &str, root: &Path) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find('@') {
        // Only treat `@` as a trigger if it begins a token (start of line or
        // preceded by whitespace) — `foo@bar` is an email, not a mention.
        let begins_token = at == 0 || rest.as_bytes().get(at - 1) == Some(&b' ');
        out.push_str(&rest[..at + 1]);
        let after = &rest[at + 1..];
        if !begins_token {
            // Not a mention start; just advance past this `@`.
            rest = after;
            continue;
        }
        // The token runs to the next whitespace.
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let token = &after[..end];
        if token.is_empty() {
            rest = after;
            continue;
        }
        let inlined = inline_file(token, root);
        out.push_str(token);
        out.push_str(&inlined);
        rest = &after[end..];
    }
    // Push whatever remains after the last `@` (or the whole line if none).
    out.push_str(rest);
    out
}

/// Read `rel` under `root` and render it as a fenced file block. Returns an
/// empty string when the path can't be resolved within `root` or read; the
/// caller keeps the bare `@token` in that case. A fenced block is appended
/// after the token when the file resolves and reads as UTF-8.
fn inline_file(rel: &str, root: &Path) -> String {
    let cleaned = rel.strip_prefix("./").unwrap_or(rel);
    // Refuse absolute paths and `..` escapes — never read outside the workspace.
    if cleaned.starts_with('/') || cleaned.contains("..") {
        return String::new();
    }
    let path = root.join(cleaned);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    if bytes.is_empty() {
        return format!("\n```\n<file {rel} is empty>\n```\n");
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return format!("\n```\n<file {rel} is not valid UTF-8 — not inlined>\n```\n"),
    };
    let lang = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if text.len() > INLINE_FILE_CAP {
        let head: String = text.chars().take(INLINE_FILE_CAP).collect();
        let elided = text.len() - head.len();
        return format!(
            "\n```{lang}\n{head}\n…[{elided} more bytes truncated — use Read for the full file]\n```\n"
        );
    }
    format!("\n```{lang}\n{}\n```\n", text.trim_end_matches('\n'))
}

/// Truncate oversized tool-result bodies in `turns` to [`TOOL_RESULT_CAP`]
/// (§8.5 microcompaction seam). Only `ToolResult` turns with an `Ok` body over
/// the cap are affected; the body is head-truncated and a tail sentinel records
/// the elision. This is a bounded per-result cap, not the full summary-turn
/// compaction that evicts superseded reads — that lands in a later milestone.
fn truncate_tool_results(turns: &[Turn]) -> Vec<Turn> {
    turns
        .iter()
        .map(|t| match t {
            Turn::ToolResult { call_id, tool, result, duration } => {
                let result = result.truncate_body(TOOL_RESULT_CAP);
                Turn::ToolResult {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    result,
                    duration: *duration,
                }
            }
            other => other.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::ToolResultBody;
    use std::time::SystemTime;
    use tempfile::tempdir;

    fn user(content: &str) -> Turn {
        Turn::User { content: content.into(), ts: SystemTime::now() }
    }

    fn tool_ok(call_id: &str, content: &str) -> Turn {
        Turn::ToolResult {
            call_id: call_id.into(),
            tool: "Read".into(),
            result: ToolResultBody::Ok { content: content.into(), truncated: false },
            duration: Default::default(),
        }
    }

    #[test]
    fn build_system_prompt_includes_environment_and_posture() {
        let env = Environment {
            cwd: PathBuf::from("/repo"),
            platform: "macOS".into(),
            date: "Monday Jul 28, 2026".into(),
            git_branch: Some("main".into()),
        };
        let prompt = build_system_prompt(&env, &[]);
        assert!(prompt.contains("You are `rc`"));
        assert!(prompt.contains("Working directory: /repo"));
        assert!(prompt.contains("Platform: macOS"));
        assert!(prompt.contains("Date: Monday Jul 28, 2026"));
        assert!(prompt.contains("Git branch: main"));
        assert!(prompt.contains("# Instructions"));
    }

    #[test]
    fn build_system_prompt_appends_memory_chain() {
        let env = Environment {
            cwd: PathBuf::from("/repo"),
            platform: "Linux".into(),
            date: "x".into(),
            git_branch: None,
        };
        let mem = vec![
            Memory { path: "~/.rc/AGENTS.md".into(), contents: "global rule".into() },
            Memory { path: "AGENTS.md".into(), contents: "repo rule".into() },
        ];
        let prompt = build_system_prompt(&env, &mem);
        assert!(prompt.contains("# Memory"));
        assert!(prompt.contains("## ~/.rc/AGENTS.md"));
        assert!(prompt.contains("global rule"));
        assert!(prompt.contains("## AGENTS.md"));
        assert!(prompt.contains("repo rule"));
    }

    #[test]
    fn environment_detect_reads_git_branch() {
        let dir = tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();
        let env = Environment::detect(dir.path(), "today".into());
        assert_eq!(env.git_branch.as_deref(), Some("feature-x"));
    }

    #[test]
    fn environment_detect_handles_non_repo() {
        let dir = tempdir().unwrap();
        let env = Environment::detect(dir.path(), "today".into());
        assert!(env.git_branch.is_none());
    }

    #[test]
    fn memory_load_chain_reads_agents_md_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "repo memory\n").unwrap();
        let chain = Memory::load_chain(dir.path());
        assert!(chain.iter().any(|m| m.path == "AGENTS.md" && m.contents.contains("repo memory")));
    }

    #[test]
    fn memory_load_chain_skips_empty_and_missing() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "   \n").unwrap();
        let chain = Memory::load_chain(dir.path());
        assert!(chain.is_empty(), "empty files are skipped: {chain:?}");
    }

    #[test]
    fn expand_mentions_inlines_a_file() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "hello world\n").unwrap();
        let out = expand_mentions("see @note.txt please", dir.path());
        assert!(out.contains("@note.txt"));
        assert!(out.contains("```"));
        assert!(out.contains("hello world"));
        // The bare token is preserved and the request still reads "please".
        assert!(out.contains("please"));
    }

    #[test]
    fn expand_mentions_leaves_emails_alone() {
        let dir = tempdir().unwrap();
        let out = expand_mentions("contact me@host.com", dir.path());
        assert_eq!(out, "contact me@host.com");
    }

    #[test]
    fn expand_mentions_refuses_path_escape() {
        let dir = tempdir().unwrap();
        let out = expand_mentions("read @../secret.txt", dir.path());
        // The `..` is not read; the bare token survives, no fence.
        assert!(out.contains("@../secret.txt"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn expand_mentions_handles_missing_file() {
        let dir = tempdir().unwrap();
        let out = expand_mentions("see @nope.txt end", dir.path());
        // Missing file: the bare token survives, no fence inlined.
        assert!(out.contains("@nope.txt"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn expand_mentions_truncates_large_files() {
        let dir = tempdir().unwrap();
        let big = "x".repeat(INLINE_FILE_CAP + 500);
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let out = expand_mentions("@big.txt", dir.path());
        assert!(out.contains("more bytes truncated"));
    }

    #[test]
    fn expand_mentions_with_no_at_returns_input_unchanged() {
        assert_eq!(expand_mentions("no mentions here", Path::new("/")), "no mentions here");
    }

    #[test]
    fn truncate_tool_results_caps_oversized_ok_body() {
        let big = "y".repeat(TOOL_RESULT_CAP + 100);
        let turns = vec![user("go"), tool_ok("c1", &big)];
        let out = truncate_tool_results(&turns);
        match &out[1] {
            Turn::ToolResult { result, .. } => {
                let ToolResultBody::Ok { content, truncated } = result else { panic!() };
                assert!(*truncated, "must be flagged truncated");
                assert!(content.len() <= TOOL_RESULT_CAP + 64, "cap+sentinel: {}", content.len());
                assert!(content.contains("truncated"), "sentinel present: {content}");
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn truncate_tool_results_leaves_small_bodies_alone() {
        let turns = vec![user("go"), tool_ok("c1", "small")];
        let out = truncate_tool_results(&turns);
        match &out[1] {
            Turn::ToolResult { result, .. } => match result {
                ToolResultBody::Ok { content, truncated } => {
                    assert_eq!(content, "small");
                    assert!(!*truncated);
                }
                other => panic!("unexpected body: {other:?}"),
            },
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn assembler_assemble_uses_custom_system_prompt() {
        let dir = tempdir().unwrap();
        let env = Environment::detect(dir.path(), "today".into());
        let asm = ContextAssembler::with_system_prompt(env, "CUSTOM PROMPT".into());
        let wire = asm.assemble(&[user("hi")]);
        assert!(matches!(
            wire.first(),
            Some(WireMessage::System { content }) if content == "CUSTOM PROMPT"
        ));
    }

    #[test]
    fn assembler_assemble_expands_last_user_mentions_only() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "AAA\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "BBB\n").unwrap();
        let env = Environment::detect(dir.path(), "today".into());
        let asm = ContextAssembler::new(env);
        // Two user turns each mentioning a file; both should be expanded since
        // prepare_turns expands from the last user turn onward and here both are
        // at/after the last user index.
        let turns = vec![user("@a.txt first"), user("@b.txt second")];
        let wire = asm.assemble(&turns);
        // The last user message must contain the inlined b.txt content.
        let last_user_content = wire
            .iter()
            .rev()
            .find_map(|m| match m {
                WireMessage::User { content: rc_proto::wire::UserContent::Text(t) } => Some(t.clone()),
                _ => None,
            })
            .unwrap();
        assert!(last_user_content.contains("BBB"), "last user inlined: {last_user_content}");
        assert!(last_user_content.contains("@b.txt"));
    }
}
