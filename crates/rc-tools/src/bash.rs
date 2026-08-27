//! The `Bash` tool (§6.6). Each call is a fresh shell (no PTY persistence), but
//! `cd` *does* persist across calls (M7): after a successful, contained `cd`,
//! the live working directory in the shared [`ShellState`] is updated, and the
//! agent loop syncs it into `ctx.cwd`/`session.cwd` for subsequent calls. stdin
//! is closed so commands don't block on input. The model-facing result is
//! unlimited by default, but transport capture always retains a bounded
//! head+tail window so a noisy child cannot exhaust the editor's memory; see
//! [`Bash::with_cap`] for an additional small-context result cap.
//!
//! Env hygiene (M7/§6.6): the shell is
//! `bash --noprofile --norc -o pipefail` when available (no interactive rc and
//! no false-success pipelines), and toolchain bin dirs
//! (nvm/pyenv/conda/`~/.local/bin`) are prepended to `PATH` via `Command::env`
//! — never by rewriting the command text, so the permission layer still parses
//! the verbatim `command`.
//!
//! Background shells (`run_in_background`) are fire-and-forget: merged output
//! is published to a bounded rotating log the agent can `Read`, and the child
//! is held in `ShellState` so it's killed on session shutdown.
//!
//! The catastrophic-command deny-list here is a *safety floor* (over-refuses);
//! M3 replaces it with real command parsing + interactive prompts.

use crate::env_hygiene;
use crate::util::{cap_output, dangerous_command, params_schema, strip_ansi};
use async_trait::async_trait;
use rc_core::state::{BgShell, BgShellStatus};
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use rc_perm::{parse_bash, resolve_within};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::os::unix::process::{CommandExt as StdCommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Independent of the model-facing result cap: never let a noisy child grow
/// an in-memory `Vec` until the cgroup kills the whole editor.
const FOREGROUND_CAPTURE_BYTES_PER_STREAM: usize = 2 * 1024 * 1024;

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

pub struct Bash {
    /// Max chars of combined stdout+stderr returned to the model. `0` =
    /// unlimited (the default). When set, the output is head+tail elided.
    cap: usize,
    /// Built at construction so the model is told the truth about `cap` —
    /// advertising a limit that isn't enforced (or vice versa) changes how the
    /// model uses the tool.
    description: String,
}

impl Default for Bash {
    fn default() -> Self {
        Self::new()
    }
}

impl Bash {
    /// A `Bash` with no additional model-facing cap. The independent transport
    /// capture ceiling still applies.
    pub fn new() -> Self {
        Self::with_cap(0)
    }

    /// A `Bash` that elides output beyond `cap` chars (`0` = unlimited).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            description: bash_description(cap),
        }
    }

    /// Head/tail split for the elision, preserving the historical 1:2 ratio.
    fn head_tail(&self) -> (usize, usize) {
        let head = self.cap / 3;
        (head, self.cap - head)
    }
}

/// The tool description, with the output-limit sentence matched to `cap`.
fn bash_description(cap: usize) -> String {
    let limit = if cap == 0 {
        "stdout+stderr are ANSI stripped and returned from a bounded head+tail capture window"
            .to_string()
    } else {
        format!(
            "stdout+stderr are captured, ANSI stripped, and capped at {} chars (head {} + tail {})",
            cap,
            cap / 3,
            cap - cap / 3,
        )
    };
    format!(
        "Run a shell command. `cd` persists across calls (a successful, in-workspace `cd` \
updates the session's working directory). {limit}; the exit code is shown first. Default \
timeout 120s, max 600s. stdin is closed — commands that read input see EOF; use \
non-interactive flags (`-y`, `--no-pager`, `git --no-pager`). Pipelines use `pipefail`, so \
any failed stage makes the reported exit non-zero; avoid piping builds or tests to `head`/`tail`. \
Set `run_in_background: true` \
for long-running servers; output goes to a bounded rotating log file you can `Read` to check progress."
    )
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        &self.description
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
            return Ok(ToolOutcome::Error {
                message: reason.to_string(),
                retryable: false,
            });
        }

        let timeout = Duration::from_millis(
            inp.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
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
        install_process_group(&mut cmd);

        // M7: opt-in kernel confinement (Landlock + seccomp on Linux). The
        // guard is held across the spawn/wait so the parent-side ruleset fd
        // stays open until the child has forked.
        let _sandbox_guard = match install_sandbox(&mut cmd, ctx) {
            Ok(g) => g,
            Err(outcome) => return Ok(outcome), // fail-closed
        };

        // Spawn the child and drain stdout+stderr concurrently, racing the drain
        // against the timeout. On a timeout we kill the child and surface the
        // partial output captured so far (bounded drains update incrementally,
        // so the captures survive the dropped drain future) — otherwise a hung
        // command's output is discarded and the model must re-run to find where
        // it stopped.
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("spawn failed: {e}"),
                    retryable: false,
                })
            }
        };
        let mut process_group = ProcessGroupGuard::new(child.id());
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut out_buf = BoundedCapture::new(FOREGROUND_CAPTURE_BYTES_PER_STREAM);
        let mut err_buf = BoundedCapture::new(FOREGROUND_CAPTURE_BYTES_PER_STREAM);

        let drain = async {
            let (o, e) = tokio::join!(
                drain_bounded(&mut stdout, &mut out_buf),
                drain_bounded(&mut stderr, &mut err_buf),
            );
            let _ = (o, e);
            child.wait().await
        };

        match tokio::time::timeout(timeout, drain).await {
            Ok(Ok(status)) => {
                // The shell has exited, but a command may have daemonized a
                // grandchild after redirecting its pipes. Foreground calls do
                // not own detached work, so clean any survivors in the group.
                process_group.kill();
                let (stdout_capture_truncated, stdout) = out_buf.render();
                let (stderr_capture_truncated, stderr) = err_buf.render();
                let stdout = strip_ansi(&stdout);
                let stderr = strip_ansi(&stderr);
                let mut combined = stdout;
                if !stderr.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                    combined.push_str(&stderr);
                }
                let (head, tail) = self.head_tail();
                let (result_truncated, body) = cap_output(&combined, self.cap, head, tail);
                let truncated =
                    stdout_capture_truncated || stderr_capture_truncated || result_truncated;
                let exit = status.code().map(|c| c.to_string()).unwrap_or_else(|| {
                    status
                        .signal()
                        .map(|signal| format!("signal {signal}"))
                        .unwrap_or_else(|| "<signal>".to_string())
                });

                // M7: persist a successful, in-workspace `cd` into the live shell state.
                // The agent loop syncs this back into ctx.cwd/session.cwd for later calls.
                if exit == "0" {
                    if let Some(new_cwd) = infer_cwd(&inp.command, &ctx.cwd) {
                        if let Ok(canon) =
                            resolve_within(&ctx.allowed_roots, &ctx.cwd, &new_cwd.to_string_lossy())
                        {
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
            Ok(Err(e)) => Ok(ToolOutcome::Error {
                message: format!("spawn failed: {e}"),
                retryable: false,
            }),
            Err(_) => {
                // Timed out: kill the child (closing its pipes) and reap it, then
                // surface the partial output captured before the timeout so the
                // model can see where the command hung without re-running it.
                process_group.kill();
                let _ = child.kill().await;
                let _ = child.wait().await;
                let (_, stdout) = out_buf.render();
                let (_, stderr) = err_buf.render();
                let stdout = strip_ansi(&stdout);
                let stderr = strip_ansi(&stderr);
                let mut combined = stdout;
                if !stderr.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                    combined.push_str(&stderr);
                }
                let (head, tail) = self.head_tail();
                let (_, body) = cap_output(&combined, self.cap, head, tail);
                Ok(ToolOutcome::Error {
                    message: format!(
                        "command timed out after {} ms (killed); partial output below:\n{}",
                        timeout.as_millis(),
                        body
                    ),
                    retryable: false,
                })
            }
        }
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
            return Ok(ToolOutcome::Error {
                message: reason.to_string(),
                retryable: false,
            });
        }

        if let Err(e) = std::fs::create_dir_all(log_path.parent().unwrap_or(Path::new("."))) {
            return Ok(ToolOutcome::Error {
                message: format!("could not create bg log dir: {e}"),
                retryable: false,
            });
        }

        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
        {
            return Ok(ToolOutcome::Error {
                message: format!("could not open bg log {}: {e}", log_path.display()),
                retryable: false,
            });
        }

        let mut cmd = Command::new(&shell);
        // Merge stderr inside the child shell so one bounded drain owns log
        // ordering and no pair of writer threads can race the same file.
        let merged_command = format!("exec 2>&1\n{}", inp.command);
        cmd.args(&shell_args).arg("-c").arg(merged_command);
        cmd.current_dir(&cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        apply_env_hygiene_std(&mut cmd);
        cmd.env("PATH", env_hygiene::rehydrated_path_env());
        install_process_group_std(&mut cmd);

        // M7: opt-in kernel confinement for background shells too.
        let _sandbox_guard = match install_sandbox_std(&mut cmd, ctx) {
            Ok(g) => g,
            Err(outcome) => return Ok(outcome), // fail-closed
        };

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutcome::Error {
                    message: format!("spawn failed: {e}"),
                    retryable: false,
                })
            }
        };
        let pgid = i32::try_from(child.id()).ok();
        let stdout = child.stdout.take().expect("background stdout was piped");

        let shell_id = id.clone();
        // A poisoned mutex still contains the only owner capable of cleaning
        // up this child. Recover the inner state instead of leaking an
        // untracked server process.
        let status = std::sync::Arc::new(std::sync::Mutex::new(BgShellStatus::Running));
        let supervised = ctx
            .shell_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .supervise_background(
                BgShell {
                    id: id.clone(),
                    log_path: log_path.clone(),
                    started: SystemTime::now(),
                    status,
                },
                child,
                stdout,
                pgid,
            );
        if let Err(error) = supervised {
            return Ok(ToolOutcome::Error {
                message: format!("could not supervise background shell: {error}"),
                retryable: false,
            });
        }

        Ok(ToolOutcome::ok(format!(
            "Background shell {shell_id} started. Output log: {} (read it with `Read` to check progress).",
            log_path.display()
        )))
    }
}

/// Retain an exact prefix and suffix while counting every byte consumed. The
/// middle is discarded as it arrives, so memory is bounded independently of
/// how much output a child produces.
struct BoundedCapture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    total: u64,
}

impl BoundedCapture {
    fn new(cap: usize) -> Self {
        let head_cap = cap / 3;
        Self {
            head: Vec::with_capacity(head_cap),
            tail: VecDeque::with_capacity(cap - head_cap),
            head_cap,
            tail_cap: cap - head_cap,
            total: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let head_needed = self.head_cap.saturating_sub(self.head.len());
        let head_take = head_needed.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_take]);
        self.tail.extend(&bytes[head_take..]);
        if self.tail.len() > self.tail_cap {
            let excess = self.tail.len() - self.tail_cap;
            self.tail.drain(..excess);
        }
    }

    fn render(&self) -> (bool, String) {
        let retained = self.head.len().saturating_add(self.tail.len());
        let truncated = self.total > retained as u64;
        let mut bytes = Vec::with_capacity(retained.saturating_add(96));
        bytes.extend_from_slice(&self.head);
        if truncated {
            let omitted = self.total.saturating_sub(retained as u64);
            bytes.extend_from_slice(format!("\n[… {omitted} output bytes omitted …]\n").as_bytes());
        }
        bytes.extend(self.tail.iter().copied());
        (truncated, String::from_utf8_lossy(&bytes).into_owned())
    }
}

async fn drain_bounded<R: AsyncRead + Unpin>(
    reader: &mut R,
    capture: &mut BoundedCapture,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        capture.push(&chunk[..read]);
    }
}

/// Apply env hygiene (drop secrets, set non-interactive vars) to a tokio
/// `Command`. `PATH` is set separately by the caller.
fn apply_env_hygiene(cmd: &mut tokio::process::Command) {
    cmd.env_remove("SC_API_KEY");
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
        .env("SC_SESSION", "1");
}

/// Same env hygiene for a std `Command` (background shells).
fn apply_env_hygiene_std(cmd: &mut Command) {
    cmd.env_remove("SC_API_KEY");
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
        .env("SC_SESSION", "1");
}

/// Put every shell in a new session/process group. A `kill_on_drop` only
/// targets the direct child; this group is what lets cancellation stop cargo
/// workers, test runners, and other descendants as one owned process tree.
fn install_process_group(cmd: &mut tokio::process::Command) {
    // SAFETY: `setsid` is async-signal-safe and the closure performs no heap
    // allocation between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

fn install_process_group_std(cmd: &mut Command) {
    // SAFETY: same as `install_process_group` above.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// Kills the whole process group when a tool future is dropped by a turn
/// deadline or cancellation. Explicit completion calls `kill` first, making
/// Drop a no-op.
struct ProcessGroupGuard {
    pgid: Option<i32>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self {
            pgid: pid.and_then(|pid| i32::try_from(pid).ok()),
        }
    }

    fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: the negative id addresses the process group created by
            // `setsid`, not an arbitrary process tree.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
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
    unsafe {
        cmd.pre_exec(pre_exec);
    }
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
    unsafe {
        cmd.pre_exec(pre_exec);
    }
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
            let target = cd_target(toks)?; // untrackable cd → can't be confident
            cur = resolve_target(&target, &cur);
            changed = true;
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
    async fn reports_failure_from_any_pipeline_stage() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(json!({"command": "false | true"}), &test_ctx(dir.path()))
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => assert!(content.starts_with("exit: 1"), "{content}"),
            other => panic!("expected shell result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn strips_ansi() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(
                json!({"command": "printf '\\033[31mred\\033[0m'"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("red"), "{content}");
                assert!(
                    !content.contains('\u{1b}'),
                    "ANSI escape not stripped: {content:?}"
                );
            }
            o => panic!("expected ok, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(
                json!({"command": "sleep 5", "timeout_ms": 100}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => {
                assert!(message.contains("timed out"), "{message}")
            }
            o => panic!("expected a timeout error, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_surfaces_partial_output() {
        // A command that prints before hanging: the partial output must reach
        // the model so it can see where the command hung without re-running it.
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(
                // print "partial" to stderr, then hang past the 150 ms timeout.
                json!({"command": "printf 'partial\\n' >&2; sleep 5", "timeout_ms": 150}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => {
                assert!(message.contains("timed out"), "{message}");
                assert!(
                    message.contains("partial"),
                    "partial output missing: {message}"
                );
            }
            o => panic!("expected a timeout error, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn noisy_output_is_retained_as_a_bounded_head_and_tail() {
        let dir = tempdir().unwrap();
        let out = Bash::new()
            .call(
                json!({"command": "yes 0123456789 | head -c 3145728"}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok {
                content, truncated, ..
            } => {
                assert!(truncated);
                assert!(content.contains("output bytes omitted"), "{content}");
                assert!(
                    content.len() < FOREGROUND_CAPTURE_BYTES_PER_STREAM + 256,
                    "bounded capture grew to {} bytes",
                    content.len()
                );
            }
            other => panic!("expected bounded output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_kills_descendant_processes() {
        let dir = tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let command = format!("sleep 30 & echo $! > {}; wait", pid_file.to_string_lossy());
        let out = Bash::new()
            .call(
                json!({"command": command, "timeout_ms": 200}),
                &test_ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(matches!(out, ToolOutcome::Error { .. }));

        let pid = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        for _ in 0..50 {
            // SAFETY: signal 0 is a read-only existence probe.
            if unsafe { libc::kill(pid, 0) } == -1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("descendant process {pid} survived Bash timeout");
    }

    #[tokio::test]
    async fn dropping_the_tool_future_kills_descendant_processes() {
        let dir = tempdir().unwrap();
        let pid_file = dir.path().join("cancelled-descendant.pid");
        let command = format!("sleep 30 & echo $! > {}; wait", pid_file.to_string_lossy());
        let bash = Bash::new();
        let ctx = test_ctx(dir.path());
        let cancelled = tokio::time::timeout(
            Duration::from_millis(200),
            bash.call(json!({"command": command, "timeout_ms": 30_000}), &ctx),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "outer deadline should drop the tool future"
        );

        let pid = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        for _ in 0..50 {
            // SAFETY: signal 0 is a read-only existence probe.
            if unsafe { libc::kill(pid, 0) } == -1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("descendant process {pid} survived dropped Bash future");
    }

    #[tokio::test]
    async fn runs_in_the_session_cwd() {
        let dir = tempdir().unwrap();
        let basename = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
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
            .call(
                json!({"command": "test -n \"$NO_COLOR\" && test \"$CI\" = 1 && echo yes"}),
                &test_ctx(dir.path()),
            )
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
        let _ = Bash::new().call(json!({"command": "cd /tmp"}), &ctx).await;
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
        assert_eq!(
            infer_cwd("cd a && cd b", root),
            Some(PathBuf::from("/repo/a/b"))
        );
        assert_eq!(
            infer_cwd("cd a && cd ..", root),
            Some(PathBuf::from("/repo/a/.."))
        );
        assert_eq!(
            infer_cwd("cd /abs/path", root),
            Some(PathBuf::from("/abs/path"))
        );
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
            .call(
                json!({"command": "echo hi-from-bg", "run_in_background": true}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Ok { content, .. } => {
                assert!(content.contains("Background shell"), "{content}")
            }
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
            if log_path.exists()
                && fs::read_to_string(&log_path).is_ok_and(|s| s.contains("hi-from-bg"))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(fs::read_to_string(&log_path)
            .unwrap()
            .contains("hi-from-bg"));
        // shutdown kills it (and reaps so no zombie).
        ctx.shell_state.lock().unwrap().shutdown();
    }

    #[tokio::test]
    async fn background_log_is_rotated_to_a_fixed_ceiling() {
        let dir = tempdir().unwrap();
        let ctx = bg_ctx(dir.path());
        Bash::new()
            .call(
                json!({
                    "command": "yes background-noise | head -c 10485760",
                    "run_in_background": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        let (log_path, status) = {
            let state = ctx.shell_state.lock().unwrap();
            (state.bg[0].log_path.clone(), state.bg[0].status.clone())
        };
        let rotated = PathBuf::from(format!("{}.1", log_path.display()));
        for _ in 0..300 {
            if *status.lock().unwrap() != BgShellStatus::Running {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(*status.lock().unwrap(), BgShellStatus::Running);
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("log rotated"));
        let total = std::fs::metadata(&log_path).unwrap().len()
            + std::fs::metadata(&rotated).unwrap().len();
        assert!(total < 8 * 1024 * 1024 + 64 * 1024, "{total}");
        ctx.shell_state.lock().unwrap().shutdown();
    }

    #[tokio::test]
    async fn background_without_bg_dir_errors() {
        let dir = tempdir().unwrap();
        let ctx = test_ctx(dir.path()); // no bg_dir
        let out = Bash::new()
            .call(
                json!({"command": "echo x", "run_in_background": true}),
                &ctx,
            )
            .await
            .unwrap();
        match out {
            ToolOutcome::Error { message, .. } => {
                assert!(message.contains("not configured"), "{message}")
            }
            o => panic!("expected an error, got {o:?}"),
        }
    }
}
