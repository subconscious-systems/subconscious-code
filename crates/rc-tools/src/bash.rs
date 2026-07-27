//! The `Bash` tool (§6.6) — foreground, stateless. Each call is a fresh
//! `$SHELL -c` (no PTY persistence; `cd` does not carry across calls — that's
//! M7). stdin is closed so commands don't block on input; output is capped at
//! 30k chars (head 10k + tail 20k) with ANSI stripped; the exit code leads.
//!
//! The catastrophic-command deny-list here is a *safety floor* (over-refuses);
//! M3 replaces it with real command parsing + interactive prompts.

use crate::util::{cap_output, dangerous_command, params_schema, strip_ansi};
use async_trait::async_trait;
use rc_core::{Concurrency, Tool, ToolCtx, ToolError, ToolOutcome};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;

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
    /// Background shells land in M7; not supported here.
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
        "Run a shell command. Each call is a fresh shell, so `cd` does not persist across \
calls. stdout+stderr are captured, ANSI stripped, and capped at 30k chars (head 10k + tail \
20k); the exit code is shown first. Default timeout 120s, max 600s. stdin is closed — \
commands that read input will see EOF; use non-interactive flags (`-y`, `--no-pager`, \
`git --no-pager`). Background servers/dev-runs land in M7 (`run_in_background`)."
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
            return Ok(ToolOutcome::Error {
                message: "background shells land in M7; run a quick command instead".to_string(),
                retryable: false,
            });
        }
        if let Some(reason) = dangerous_command(&inp.command) {
            return Ok(ToolOutcome::Error { message: reason.to_string(), retryable: false });
        }

        let timeout = Duration::from_millis(inp.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS));
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

        let mut cmd = tokio::process::Command::new(&shell);
        cmd.arg("-c").arg(&inp.command);
        cmd.current_dir(&ctx.cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true); // a timeout must kill the child, not orphan it
        // Env hygiene (§6.6): inherit, drop secrets, set dumb-pager/non-interactive vars.
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

        Ok(ToolOutcome::Ok {
            content: format!("exit: {exit}\n{body}"),
            truncated,
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_ctx;
    use serde_json::json;
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
}
