//! Native Windows shell backend. PowerShell by default; Git Bash is opt-in.
//! Kept separate from bash.rs so the Unix implementation does not change.
use crate::env_hygiene::{self, ShellKind};
use crate::util::{cap_output, dangerous_command, params_schema, strip_ansi};
use async_trait::async_trait;
use rc_core::state::{BgShell, BgShellStatus};
use rc_core::windows_process::{Job, CREATE_SUSPENDED};
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt};

const CAPTURE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
pub struct BashInput {
    /// Complete script in the selected shell's syntax (PowerShell by default).
    pub command: String,
    /// Timeout in milliseconds (default 120000, maximum 600000).
    pub timeout_ms: Option<u64>,
    /// Run in the background; read the returned log path to monitor output.
    #[serde(default)]
    pub run_in_background: bool,
}

/// The existing composition-root type now selects the Windows shell tool.
/// Its advertised name is PowerShell unless Git Bash was explicitly selected.
pub struct Bash {
    kind: Result<ShellKind, String>,
    cap: usize,
    description: String,
}

impl Default for Bash {
    fn default() -> Self {
        Self::new()
    }
}
impl Bash {
    pub fn new() -> Self {
        Self::with_cap(0)
    }
    pub fn with_cap(cap: usize) -> Self {
        Self::with_kind(env_hygiene::shell_kind(), cap)
    }

    fn with_kind(kind: Result<ShellKind, String>, cap: usize) -> Self {
        let shell = if kind == Ok(ShellKind::Bash) {
            "Run a Git Bash script on Windows with --noprofile --norc and pipefail."
        } else {
            "Run a PowerShell script natively on Windows, NOT Bash. Use Windows PowerShell 5.1-compatible syntax (or PowerShell 7 features only after checking $PSVersionTable). Use PowerShell cmdlets and Windows paths; do not assume Unix utilities exist. Check $LASTEXITCODE after native programs in multi-command scripts; PowerShell 5.1 does not provide Bash pipefail."
        };
        Self { kind, cap, description: format!("{shell} stdin is closed. Default timeout 120s, maximum 600s. Commands and descendants are terminated on timeout/cancellation. stdout/stderr use bounded 2 MiB head+tail capture per stream; additional result cap: {cap} characters (0 means no extra cap). A successful in-workspace directory change persists across foreground calls. run_in_background writes merged output to a bounded rotating log and the process tree is stopped when the session ends. PowerShell approvals are for the complete script, not Bash prefixes. Kernel filesystem/network sandboxing is unavailable on Windows; requests for it fail closed.") }
    }
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        if self.kind == Ok(ShellKind::Bash) {
            "Bash"
        } else {
            "PowerShell"
        }
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
        let input: BashInput = serde_json::from_value(input)?;
        let kind = match &self.kind {
            Ok(kind) => *kind,
            Err(error) => return Ok(ToolOutcome::error(error.clone())),
        };
        if ctx.sandbox.is_some() {
            return Ok(ToolOutcome::error(
                "kernel sandbox unavailable on Windows; refusing to run unsandboxed".into(),
            ));
        }
        if input.command.trim().is_empty() {
            return Ok(ToolOutcome::error("command must not be empty".into()));
        }
        let dangerous = if kind == ShellKind::PowerShell {
            rc_perm::rules::powershell_is_catastrophic(&input.command)
        } else {
            dangerous_command(&input.command).is_some()
        };
        if dangerous {
            return Ok(ToolOutcome::Denied {
                reason: "destructive command refused".into(),
            });
        }
        let prepared = match Prepared::new(kind, &input.command, input.run_in_background) {
            Ok(prepared) => prepared,
            Err(error) => return Ok(ToolOutcome::error(error)),
        };
        if input.run_in_background {
            return self.background(prepared, ctx);
        }
        self.foreground(prepared, &input, ctx).await
    }
}

impl Bash {
    async fn foreground(
        &self,
        prepared: Prepared,
        input: &BashInput,
        ctx: &ToolCtx,
    ) -> Result<ToolOutcome, ToolError> {
        let mut command = tokio::process::Command::from(prepared.command(&ctx.cwd));
        command.kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Ok(ToolOutcome::error(format!("shell spawn failed: {error}"))),
        };
        // SAFETY: our command uses CREATE_SUSPENDED; the process is still owned.
        let job = match unsafe {
            Job::attach_and_resume(
                child.raw_handle().expect("live child"),
                child.id().expect("live child"),
            )
        } {
            Ok(job) => job.with_scripts(prepared.dir),
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(ToolOutcome::error(format!(
                    "cannot contain Windows shell process tree: {error}"
                )));
            }
        };
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut out = Capture::new(CAPTURE_BYTES);
        let mut err = Capture::new(CAPTURE_BYTES);
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(120_000).min(600_000));
        let result = {
            let wait_and_drain = async {
                let wait = async {
                    let status = child.wait().await;
                    // Close pipes retained by detached descendants when the leader exits.
                    job.kill();
                    status
                };
                let drain = async {
                    let _ =
                        tokio::join!(drain(&mut stdout, &mut out), drain(&mut stderr, &mut err));
                };
                let (status, ()) = tokio::join!(wait, drain);
                status
            };
            tokio::select! {
                _ = ctx.cancel.cancelled() => None,
                result = tokio::time::timeout(timeout, wait_and_drain) => Some(result),
            }
        };
        job.kill();
        let (truncated, body) = self.output(&out, &err);
        match result {
            None => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Ok(ToolOutcome::Interrupted)
            }
            Some(Err(_)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Ok(ToolOutcome::error(format!("command timed out after {} ms (process tree killed); partial output below:\n{body}", timeout.as_millis())))
            }
            Some(Ok(Err(error))) => Ok(ToolOutcome::error(format!("shell wait failed: {error}"))),
            Some(Ok(Ok(status))) => {
                let code = status.code().unwrap_or(-1);
                let mut content = format!("exit: {code}\n{body}");
                if code == 0 {
                    if let Ok(cwd) = std::fs::read_to_string(&prepared.cwd_file) {
                        let cwd = cwd.trim();
                        if !cwd.is_empty() {
                            match rc_perm::resolve_within(&ctx.allowed_roots, &ctx.cwd, cwd) {
                                Ok(cwd) => ctx.shell_state.lock().unwrap_or_else(|e| e.into_inner()).cwd = cwd,
                                Err(_) => content.push_str("\nnote: cwd was not persisted (outside workspace roots or not a filesystem directory)"),
                            }
                        }
                    }
                }
                Ok(ToolOutcome::Ok {
                    content,
                    truncated,
                    artifacts: Vec::new(),
                })
            }
        }
    }

    fn output(&self, stdout: &Capture, stderr: &Capture) -> (bool, String) {
        let (out_truncated, out) = stdout.render();
        let (err_truncated, err) = stderr.render();
        let mut text = strip_ansi(&out);
        if !err.is_empty() {
            text.push_str(&format!("\n--- stderr ---\n{}", strip_ansi(&err)));
        }
        let (capped, body) = cap_output(&text, self.cap, self.cap / 3, self.cap - self.cap / 3);
        (out_truncated || err_truncated || capped, body)
    }

    fn background(&self, prepared: Prepared, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let (id, log_path, cwd) = {
            let mut state = ctx.shell_state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(dir) = state.bg_dir.clone() else {
                return Ok(ToolOutcome::error(
                    "background shells are not configured (no bg dir)".into(),
                ));
            };
            state.next_bg += 1;
            let id = format!("bg-{}", state.next_bg);
            (id.clone(), dir.join(format!("{id}.log")), state.cwd.clone())
        };
        std::fs::create_dir_all(log_path.parent().expect("bg log parent"))?;
        let mut child = match prepared.command(&cwd).spawn() {
            Ok(child) => child,
            Err(error) => return Ok(ToolOutcome::error(format!("shell spawn failed: {error}"))),
        };
        // SAFETY: same suspended-process contract as foreground execution.
        let job = match unsafe { Job::attach_and_resume(child.as_raw_handle(), child.id()) } {
            Ok(job) => job.with_scripts(prepared.dir),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ToolOutcome::error(format!(
                    "cannot contain background shell: {error}"
                )));
            }
        };
        let stdout = rc_core::windows_process::Output {
            stdout: child.stdout.take().expect("piped stdout"),
            stderr: child.stderr.take().expect("piped stderr"),
        };
        let shell = BgShell {
            id: id.clone(),
            log_path: log_path.clone(),
            started: SystemTime::now(),
            status: Arc::new(Mutex::new(BgShellStatus::Running)),
        };
        ctx.shell_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .supervise_background(shell, child, stdout, job)?;
        Ok(ToolOutcome::ok(format!(
            "Background shell {id} started. Output log: {} (read it with Read to check progress).",
            log_path.display()
        )))
    }
}

struct Prepared {
    dir: tempfile::TempDir,
    cwd_file: PathBuf,
    source_file: PathBuf,
    shell: PathBuf,
    args: Vec<OsString>,
    background: bool,
    kind: ShellKind,
}

impl Prepared {
    fn new(kind: ShellKind, source: &str, background: bool) -> Result<Self, String> {
        let shell = env_hygiene::resolve_shell(kind)?;
        let dir = tempfile::Builder::new()
            .prefix("sc-windows-shell-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let cwd_file = dir.path().join("cwd.txt");
        let source_file = dir.path().join(if kind == ShellKind::PowerShell {
            "command.ps1"
        } else {
            "command.sh"
        });
        let args = if kind == ShellKind::PowerShell {
            // The BOM is required for non-ASCII source under Windows PowerShell 5.1.
            std::fs::write(&source_file, format!("\u{feff}{source}")).map_err(|e| e.to_string())?;
            let wrapper = dir.path().join("run.ps1");
            std::fs::write(&wrapper, POWERSHELL_WRAPPER).map_err(|e| e.to_string())?;
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]
            .into_iter()
            .map(OsString::from)
            .chain([wrapper.into_os_string()])
            .collect()
        } else {
            std::fs::write(&source_file, source).map_err(|e| e.to_string())?;
            let wrapper = if background {
                "exec 2>&1\nsource \"$SC_WINDOWS_COMMAND_FILE\"\nsc_code=$?\npwd -W > \"$SC_WINDOWS_CWD_FILE\"\nexit \"$sc_code\""
            } else {
                "source \"$SC_WINDOWS_COMMAND_FILE\"\nsc_code=$?\npwd -W > \"$SC_WINDOWS_CWD_FILE\"\nexit \"$sc_code\""
            };
            ["--noprofile", "--norc", "-o", "pipefail", "-c", wrapper]
                .into_iter()
                .map(OsString::from)
                .collect()
        };
        Ok(Self {
            dir,
            cwd_file,
            source_file,
            shell,
            args,
            background,
            kind,
        })
    }

    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(&self.shell);
        command
            .args(&self.args)
            .current_dir(cwd)
            .creation_flags(CREATE_SUSPENDED)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_ascii_uppercase();
            if upper.ends_with("_API_KEY")
                || upper.ends_with("_TOKEN")
                || upper.ends_with("_SECRET")
            {
                command.env_remove(key);
            }
        }
        command
            .env("PATH", env_hygiene::rehydrated_path_env())
            .env(
                "SC_WINDOWS_COMMAND_FILE",
                shell_path(&self.source_file, self.kind),
            )
            .env("SC_WINDOWS_CWD_FILE", shell_path(&self.cwd_file, self.kind))
            .env(
                "SC_WINDOWS_BACKGROUND",
                if self.background { "1" } else { "0" },
            )
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("CI", "1")
            .env("SC_SESSION", "1");
        command
    }
}

fn shell_path(path: &Path, kind: ShellKind) -> OsString {
    if kind == ShellKind::Bash {
        path.to_string_lossy().replace('\\', "/").into()
    } else {
        path.as_os_str().to_owned()
    }
}

const POWERSHELL_WRAPPER: &str = r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$OutputEncoding = [Console]::OutputEncoding
$global:LASTEXITCODE = 0
try {
    & $env:SC_WINDOWS_COMMAND_FILE
    $scSuccess = $?
    $scCode = $global:LASTEXITCODE
    if (-not $scSuccess -and $scCode -eq 0) { $scCode = 1 }
} catch {
    [Console]::Error.WriteLine($_.ToString())
    $scCode = 1
}
try {
    [IO.File]::WriteAllText($env:SC_WINDOWS_CWD_FILE, (Get-Location).ProviderPath, (New-Object System.Text.UTF8Encoding $false))
} catch {}
exit $scCode
"#;

struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    total: u64,
}
impl Capture {
    fn new(cap: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_cap: cap / 3,
            tail_cap: cap - cap / 3,
            total: 0,
        }
    }
    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let take = (self.head_cap - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        self.tail.extend(&bytes[take..]);
        if self.tail.len() > self.tail_cap {
            self.tail.drain(..self.tail.len() - self.tail_cap);
        }
    }
    fn render(&self) -> (bool, String) {
        let count = self.head.len() + self.tail.len();
        let truncated = self.total > count as u64;
        let mut bytes = self.head.clone();
        if truncated {
            bytes.extend_from_slice(
                format!(
                    "\n[… {} output bytes omitted …]\n",
                    self.total - count as u64
                )
                .as_bytes(),
            );
        }
        bytes.extend(self.tail.iter().copied());
        (truncated, String::from_utf8_lossy(&bytes).into_owned())
    }
}
async fn drain<R: AsyncRead + Unpin>(reader: &mut R, capture: &mut Capture) -> std::io::Result<()> {
    let mut bytes = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        capture.push(&bytes[..count]);
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
