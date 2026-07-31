//! The `Bash` tool (§6.6). Each call is a fresh shell (no PTY persistence), but
//! `cd` *does* persist across calls (M7): after a successful, contained `cd`,
//! the live working directory in the shared [`ShellState`] is updated, and the
//! agent loop syncs it into `ctx.cwd`/`session.cwd` for subsequent calls. stdin
//! is closed so commands don't block on input; output is capped at 30k chars
//! (head 10k + tail 20k) with ANSI stripped; the exit code leads.
//!
//! Env hygiene (M7/§6.6): the shell is `bash --noprofile --norc` when available
//! (no interactive rc), and toolchain bin dirs (nvm/pyenv/conda/`~/.local/bin`)
//! are prepended to `PATH` via `Command::env` — never by rewriting the command
//! text, so the permission layer still parses the verbatim `command`.
//!
//! Background shells (`run_in_background`) are fire-and-forget: stdout+stderr go
//! to a log file the agent can `Read`, and the child is held in `ShellState` so
//! it's killed on session shutdown.
//!
//! The catastrophic-command deny-list here is a *safety floor* (over-refuses);
//! M3 replaces it with real command parsing + interactive prompts.

use crate::env_hygiene;
use crate::util::{cap_output, dangerous_command, params_schema, strip_ansi};
use async_trait::async_trait;
use rc_core::state::BgShell;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use rc_perm::{parse_bash, resolve_within};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::os::unix::process::CommandExt as StdCommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const CAP: usize = 30_000;
const HEAD: usize = 10_000;
const TAIL: usize = 20_000;

#[derive(Deserialize, JsonSchema)]
pub struct BashInput {
    /// The shell command to run.
    pub command: String,
    /// Timeout in milliseconds (default 120000, max 600000).
    pub timeout_ms: Option<u64>,
    /// Run detached in the background; output goes to a log file you can `Read`.
    /// Use for long-running servers/dev-runs. Ignored for the foreground timeout.
    #[serde(default)]
    pub run_in_background: bool,
}

#[derive(Default)]
pub struct Bash;

impl Bash {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Run a shell command. `cd` persists across calls (a successful, in-workspace `cd` \
updates the session's working directory). stdout+stderr are captured, ANSI stripped, and \
capped at 30k chars (head 10k + tail 20k); the exit code is shown first. Default timeout 120s, \
max 600s. stdin is closed — commands that read input see EOF; use non-interactive flags (`-y`, \
`--no-pager`, `git --no-pager`). Set `run_in_background: true` for long-running servers; output \
goes to a log file you can `Read` to check progress."
    }

    fn schema(&self) -> Value {
        params_schema::<BashInput>()
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let inp: BashInput = serde_json::from_value(input)?;
        if inp.run_in_background {
            return self.run_background(&inp, ctx);
        }
        if let Some(reason) = dangerous_command(&inp.command) {
            return Ok(ToolOutcome::Error { message: reason.to_string(), retryable: false });
        }

        let timeout = Duration::from_millis(inp.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS));
        let (shell, shell_args) = env_hygiene::resolve_shell();

        let mut cmd = tokio::process::Command::new(&shell);
        cmd.args(&shell_args).arg("-c").arg(&inp.command);
        cmd.current_dir(&ctx.cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true); // a timeout must kill the child, not orphan it
        apply_env_hygiene(&mut cmd);
        cmd.env("PATH", env_hygiene::rehydrated_path_env());

        // M7: opt-in kernel confinement (Landlock + seccomp on Linux). The
        // guard is held across the spawn/wait so the parent-side ruleset fd
        // stays open until the child has forked.
        let _sandbox_guard = match install_sandbox(&mut cmd, ctx) {
            Ok(g) => g,
            Err(outcome) => return Ok(outcome), // fail-closed
        };

        let output = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Ok(ToolOutcome::Error {
                    message: format!("spawn failed: {e}"),
                    retryable: false,
                })
            }
            Err(_) => {
                return Ok(ToolOutcome::Error {
                    message: format!("command timed out after {} ms (killed)", timeout.as_millis()),
                    retryable: false,
                })
            }
        };

        let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout));
        let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));
        let mut combined = stdout;
        if !stderr.is_empty() {
            combined.push_str("\n--- stderr ---\n");
            combined.push_str(&stderr);
        }
        let (truncated, body) = cap_output(&combined, CAP, HEAD, TAIL);
        let exit = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "<signal>".to_string());

        // M7: persist a successful, in-workspace `cd` into the live shell state.
        // The agent loop syncs this back into ctx.cwd/session.cwd for later calls.
        if exit == "0" {
            if let Some(new_cwd) = infer_cwd(&inp.command, &ctx.cwd) {
                if let Ok(canon) = resolve_within(&ctx.allowed_roots, &ctx.cwd, &new_cwd.to_string_lossy()) {
                    if let Ok(mut shell_state) = ctx.shell_state.lock() {
                        shell_state.cwd = canon;
                    }
                }
                // A cd outside the allowed roots ran (transient, in the subshell)
                // but is not persisted — the agent can't `cd` out of the workspace.
            }
        }

        Ok(ToolOutcome::Ok {
            content: format!("exit: {exit}\n{body}"),
            truncated,
            artifacts: Vec::new(),
        })
    }
}

impl Bash {
    /// Spawn a detached background shell, redirecting stdout+stderr to a log
    /// file the agent can `Read`. The child is held in `ShellState::bg` so it's
    /// killed on shutdown.
    fn run_background(&self, inp: &BashInput, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let (shell, shell_args) = env_hygiene::resolve_shell();

        let (id, log_path, cwd) = {
            let mut s = match ctx.shell_state.lock() {
                Ok(g) => g,
                Err(_) => {
                    return Ok(ToolOutcome::Error {
                        message: "shell state poisoned".to_string(),
                        retryable: false,
                    })
                }
            };
            let bg_dir = match s.bg_dir.clone() {
                Some(d) => d,
                None => {
                    return Ok(ToolOutcome::Error {
                        message: "background shells are not configured (no bg dir)".to_string(),
                        retryable: false,
                    })
                }
            };
            s.next_bg += 1;
            let id = format!("bg-{}", s.next_bg);
            let log_path = bg_dir.join(format!("{id}.log"));
            (id, log_path, s.cwd.clone())
        };

        if let Some(reason) = dangerous_command(&inp.command) {
            return Ok(ToolOutcome::Error { message: reason.to_string(), retryable: false });
        }

        if let Err(e) = std::fs::create_dir_all(log_path.parent().unwrap_or(Path::new("."))) {
            return Ok(ToolOutcome::Error {
                message: format!("could not create bg log dir: {e}"),
                retryable: false,
            });
        }

        let log = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
        {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("could not open bg log {}: {e}", log_path.display()),
                    retryable: false,
                })
            }
        };
        let log_err = match log.try_clone() {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("could not dup bg log: {e}"),
                    retryable: false,
                })
            }
        };

        let mut cmd = Command::new(&shell);
        cmd.args(&shell_args).arg("-c").arg(&inp.command);
        cmd.current_dir(&cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::from(log));
        cmd.stderr(Stdio::from(log_err));
        apply_env_hygiene_std(&mut cmd);
        cmd.env("PATH", env_hygiene::rehydrated_path_env());

        // M7: opt-in kernel confinement for background shells too.
        let _sandbox_guard = match install_sandbox_std(&mut cmd, ctx) {
            Ok(g) => g,
            Err(outcome) => return Ok(outcome), // fail-closed
        };

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("spawn failed: {e}"),
                    retryable: false,
                })
            }
        };

        let shell_id = id.clone();
        if let Ok(mut s) = ctx.shell_state.lock() {
            s.bg.push(BgShell {
                id: id.clone(),
                log_path: log_path.clone(),
                child,
                started: SystemTime::now(),
            });
        }

        Ok(ToolOutcome::ok(format!(
            "Background shell {shell_id} started. Output log: {} (read it with `Read` to check progress).",
            log_path.display()
        )))
    }
}

/// Apply env hygiene (drop secrets, set non-interactive vars) to a tokio
/// `Command`. `PATH` is set separately by the caller.
fn apply_env_hygiene(cmd: &mut tokio::process::Command) {
    cmd.env_remove("RC_API_KEY");
    for (k, _) in std::env::vars() {
        if k.ends_with("_API_KEY") || k.ends_with("_TOKEN") || k.ends_with("_SECRET") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CI", "1")
        .env("RC_SESSION", "1");
}

/// Same env hygiene for a std `Command` (background shells).
fn apply_env_hygiene_std(cmd: &mut Command) {
    cmd.env_remove("RC_API_KEY");
    for (k, _) in std::env::vars() {
        if k.ends_with("_API_KEY") || k.ends_with("_TOKEN") || k.ends_with("_SECRET") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CI", "1")
        .env("RC_SESSION", "1");
}

/// The `pre_exec` closure type accepted by both `tokio::process::Command` and
/// `std::process::Command` on Unix.
type PreExecFn = Box<dyn FnMut() -> std::io::Result<()> + Send + Sync + 'static>;

/// Build the `pre_exec` closure + guard for the opt-in kernel sandbox, or
/// `None` when confinement is off (`ToolCtx::sandbox` is `None`). Fail-closed:
/// if `rc-sandbox` can't enforce anything (e.g. `--sandbox-net` on a
/// Landlock-less kernel), returns a non-retryable `Error` so the caller
/// refuses to run the command unsandboxed.
///
/// The returned guard must outlive `Command::spawn`/`output` — it keeps the
/// parent-side ruleset fd open until the child has forked.
fn prepare_sandbox(
    ctx: &ToolCtx,
) -> Result<Option<(PreExecFn, rc_sandbox::SandboxGuard)>, ToolOutcome> {
    let policy = match ctx.sandbox {
        None => return Ok(None),
        Some(p) => p,
    };
    let sandbox = rc_sandbox::Sandbox::new(ctx.allowed_roots.clone(), policy.allow_net);
    match sandbox.prepare() {
        Ok(prepared) => Ok(Some(prepared.install())),
        Err(e) => Err(ToolOutcome::Error {
            message: format!("sandbox unavailable; refusing to run unsandboxed: {e}"),
            retryable: false,
        }),
    }
}

/// Install the sandbox `pre_exec` closure (if any) onto `cmd` and return the
/// guard that must outlive the spawn. `None` ⇒ no confinement.
fn install_sandbox(
    cmd: &mut tokio::process::Command,
    ctx: &ToolCtx,
) -> Result<Option<rc_sandbox::SandboxGuard>, ToolOutcome> {
    let Some((pre_exec, guard)) = prepare_sandbox(ctx)? else {
        return Ok(None);
    };
    // SAFETY: the closure only issues raw syscalls (async-signal-safe) and
    // does not allocate — see rc-sandbox::PreparedSandbox::install.
    unsafe { cmd.pre_exec(pre_exec); }
    Ok(Some(guard))
}

fn install_sandbox_std(
    cmd: &mut Command,
    ctx: &ToolCtx,
) -> Result<Option<rc_sandbox::SandboxGuard>, ToolOutcome> {
    let Some((pre_exec, guard)) = prepare_sandbox(ctx)? else {
        return Ok(None);
    };
    // SAFETY: as above — raw syscalls only, no allocation in the child.
    unsafe { cmd.pre_exec(pre_exec); }
    Ok(Some(guard))
}

/// Infer the working directory after a command, if it contains a trackable,
/// literal `cd`/`pushd`. Returns `None` when the command is unparseable or the
/// cd target can't be confidently resolved (substitution, `cd -`, `~other`),
/// so we conservatively leave the cwd unchanged rather than guess.
///
/// Reuses `rc_perm::parse_bash` (the same tokenizer the permission layer uses).
/// Chained `cd`s thread through: `cd a && cd b` ends in `cwd/a/b`.
fn infer_cwd(command: &str, cwd: &Path) -> Option<PathBuf> {
    let parsed = parse_bash(command);
    if parsed.unparseable {
        return None;
    }
    let mut cur = cwd.to_path_buf();
    let mut changed = false;
    for sub in &parsed.subcommands {
        let toks = &sub.tokens;
        if toks.is_empty() {
            continue;
        }
        if toks[0] == "cd" || toks[0] == "pushd" {
            match cd_target(toks) {
                Some(target) => {
                    cur = resolve_target(&target, &cur);
                    changed = true;
                }
                None => return None, // untrackable cd → can't be confident
            }
        }
    }
    if changed {
        Some(cur)
    } else {
        None
    }
}

/// Extract the directory argument from a `cd`/`pushd` token list, skipping
/// flags (`-L`/`-P`/`--`). Returns `None` for `cd -` (previous dir) or an
/// unresolvable target. Bare `cd` (no arg) resolves to `$HOME`.
fn cd_target(toks: &[String]) -> Option<String> {
    let mut i = 1;
    let mut end_opts = false;
    while i < toks.len() {
        let t = &toks[i];
        if !end_opts && t == "--" {
            end_opts = true;
            i += 1;
            continue;
        }
        if !end_opts && t.starts_with('-') && t.len() > 1 {
            i += 1; // -L / -P
            continue;
        }
        if t == "-" {
            return None; // cd - → previous dir, untrackable
        }
        return Some(t.clone());
    }
    std::env::var("HOME").ok() // bare `cd` → home
}

/// Resolve a cd target against the current dir, expanding `~` and `~/...`.
fn resolve_target(target: &str, cwd: &Path) -> PathBuf {
    if let Some(rest) = target.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if target == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    let p = Path::new(target);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn echoes_output_and_exit_code() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "echo hello"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.starts_with("exit: 0"), "{content}");
                assert!(content.contains("hello"), "{content}");
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "exit 3"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.starts_with("exit: 3"), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn strips_ansi() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "printf '\\033[31mred\\033[0m'"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("red"), "{content}");
                assert!(!content.contains('\u{1b}'), "ANSI escape not stripped: {content:?}");
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "sleep 5", "timeout_ms": 100}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("timed out"), "{message}"),
            o => panic!("expected a timeout error, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn runs_in_the_session_cwd() {
        let dir = tempdir().unwrap();
        let basename = dir.path().file_name().unwrap().to_string_lossy().to_string();
        let out = Bash::new()
            .call(json!({"command": "pwd"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.contains(&basename), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn safety_floor_refuses_destructive_commands() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "rm -rf /"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("refused"), "{message}"),
            o => panic!("expected a refusal, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn stdin_is_closed() {
        // `cat` reads stdin; with stdin null it hits EOF immediately and exits 0.
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "cat"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.starts_with("exit: 0"), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn sets_non_interactive_env() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "test -n \"$NO_COLOR\" && test \"$CI\" = 1 && echo yes"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.contains("yes"), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
    }

    // ---- M7: cd persistence -------------------------------------------------

    #[tokio::test]
    async fn cd_persists_across_calls() {
        let dir = tempdir().unwrap();
        let mut ctx = test_ctx(dir.path());
        fs::create_dir_all(dir.path().join("subdir")).unwrap();
        let _ = Bash::new()
            .call(json!({"command": "cd subdir"}), &ctx)
            .await;
        // The shell state now holds <dir>/subdir; a subsequent `pwd` reflects it
        // once ctx.cwd is synced (the agent loop does this; we mirror it here).
        let live_cwd = ctx.shell_state.lock().unwrap().cwd.clone();
        assert!(live_cwd.ends_with("subdir"), "{}", live_cwd.display());
        ctx.cwd = live_cwd;
        let out = Bash::new()
            .call(json!({"command": "pwd"}), &ctx)
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.contains("subdir"), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn cd_does_not_persist_on_failure() {
        let dir = tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let _ = Bash::new()
            .call(json!({"command": "cd does_not_exist"}), &ctx)
            .await;
        let live_cwd = ctx.shell_state.lock().unwrap().cwd.clone();
        assert_eq!(live_cwd, dir.path());
    }

    #[tokio::test]
    async fn cd_escape_outside_root_is_not_persisted() {
        let dir = tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        // /tmp (or its canonical form) is outside the tempdir root.
        let _ = Bash::new()
            .call(json!({"command": "cd /tmp"}), &ctx)
            .await;
        let live_cwd = ctx.shell_state.lock().unwrap().cwd.clone();
        assert_eq!(live_cwd, dir.path(), "cd escaped the workspace root");
    }

    #[tokio::test]
    async fn cd_with_substitution_is_not_tracked() {
        let dir = tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        fs::create_dir_all(dir.path().join("subdir")).unwrap();
        // parse_bash flags `$(...)` as unparseable → we conservatively don't
        // persist the cd, even though the shell would cd into subdir.
        let _ = Bash::new()
            .call(json!({"command": "cd $(echo subdir)"}), &ctx)
            .await;
        let live_cwd = ctx.shell_state.lock().unwrap().cwd.clone();
        assert_eq!(live_cwd, dir.path());
    }

    #[test]
    fn infer_cwd_handles_chained_relative_cds() {
        let root = Path::new("/repo");
        assert_eq!(infer_cwd("cd a", root), Some(PathBuf::from("/repo/a")));
        assert_eq!(infer_cwd("cd a && cd b", root), Some(PathBuf::from("/repo/a/b")));
        assert_eq!(infer_cwd("cd a && cd ..", root), Some(PathBuf::from("/repo/a/..")));
        assert_eq!(infer_cwd("cd /abs/path", root), Some(PathBuf::from("/abs/path")));
        assert_eq!(infer_cwd("echo hi", root), None);
        assert_eq!(infer_cwd("cd $(x)", root), None); // unparseable
        assert_eq!(infer_cwd("cd -", root), None); // previous dir
    }

    // ---- M7: background shells ----------------------------------------------

    fn bg_ctx(dir: &Path) -> ToolCtx {
        let ctx = test_ctx(dir);
        {
            let mut s = ctx.shell_state.lock().unwrap();
            s.bg_dir = Some(dir.join(".rc-bg"));
        }
        ctx
    }

    #[tokio::test]
    async fn background_shell_starts_and_writes_log() {
        let dir = tempdir().unwrap();
        let ctx = bg_ctx(dir.path());
        let out = Bash::new()
            .call(json!({"command": "echo hi-from-bg", "run_in_background": true}), &ctx)
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.contains("Background shell"), "{content}"),
            o => panic!("expected ok, got {o:?}"),
        }
        // The shell is held in the state; read its log path from there.
        let log_path = {
            let s = ctx.shell_state.lock().unwrap();
            assert_eq!(s.bg.len(), 1);
            s.bg[0].log_path.clone()
        };
        // Give the background shell a moment to write.
        for _ in 0..50 {
            if log_path.exists() && fs::read_to_string(&log_path).is_ok_and(|s| s.contains("hi-from-bg")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(fs::read_to_string(&log_path).unwrap().contains("hi-from-bg"));
        // shutdown kills it (and reaps so no zombie).
        ctx.shell_state.lock().unwrap().shutdown();
    }

    #[tokio::test]
    async fn background_without_bg_dir_errors() {
        let dir = tempdir().unwrap();
        let ctx = test_ctx(dir.path()); // no bg_dir
        let out = Bash::new()
            .call(json!({"command": "echo x", "run_in_background": true}), &ctx)
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => assert!(message.contains("not configured"), "{message}"),
            o => panic!("expected an error, got {o:?}"),
        }
    }
}
