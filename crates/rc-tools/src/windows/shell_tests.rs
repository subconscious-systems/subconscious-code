use super::*;
use crate::util::test_ctx;
use serde_json::json;

fn powershell() -> Bash {
    Bash::with_kind(Ok(ShellKind::PowerShell), 0)
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}
fn content(outcome: ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Ok { content, .. } => content,
        other => panic!("expected success: {other:?}"),
    }
}
async fn call(script: &str, ctx: &ToolCtx) -> ToolOutcome {
    powershell()
        .call(json!({"command": script}), ctx)
        .await
        .unwrap()
}

#[test]
fn shell_names_and_conservative_description() {
    assert_eq!(powershell().name(), "PowerShell");
    assert_eq!(Bash::with_kind(Ok(ShellKind::Bash), 0).name(), "Bash");
    assert!(powershell().description().contains("NOT Bash"));
    assert!(powershell().description().contains("5.1"));
}

#[tokio::test]
async fn powershell_unicode_quotes_metacharacters_and_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = test_ctx(dir.path());
    let text = content(call("Write-Output 'café 日本語 & %PATH% \"quoted\"'", &ctx).await);
    assert!(text.contains("café 日本語 & %PATH%"), "{text}");
    let text = content(
        call(
            "[Console]::Out.WriteLine('OUT'); [Console]::Error.WriteLine('ERR')",
            &ctx,
        )
        .await,
    );
    assert!(
        text.contains("OUT") && text.contains("--- stderr ---") && text.contains("ERR"),
        "{text}"
    );
}

#[tokio::test]
async fn powershell_exit_codes_native_errors_and_closed_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = test_ctx(dir.path());
    assert!(content(call("exit 7", &ctx).await).starts_with("exit: 7"));
    assert!(content(call("& $env:ComSpec /d /c 'exit 9'", &ctx).await).starts_with("exit: 9"));
    assert!(content(call("throw 'failure marker'", &ctx).await).starts_with("exit: 1"));
    let text = content(
        call(
            "[Console]::WriteLine([Console]::In.ReadToEnd().Length)",
            &ctx,
        )
        .await,
    );
    assert!(text.starts_with("exit: 0\n0"), "{text}");
}

#[tokio::test]
async fn successful_cwd_persists_but_failed_and_outside_changes_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = test_ctx(dir.path());
    let sub = dir.path().join("space & quote's 日本語");
    std::fs::create_dir(&sub).unwrap();
    let _ = content(call(&format!("Set-Location -LiteralPath {}", quote(&sub)), &ctx).await);
    assert_eq!(
        ctx.shell_state.lock().unwrap().cwd,
        std::fs::canonicalize(&sub).unwrap()
    );
    let original = ctx.shell_state.lock().unwrap().cwd.clone();
    let _ = content(call("Set-Location ..; exit 3", &ctx).await);
    assert_eq!(ctx.shell_state.lock().unwrap().cwd, original);
    let outside = tempfile::tempdir().unwrap();
    let text = content(
        call(
            &format!("Set-Location -LiteralPath {}", quote(outside.path())),
            &ctx,
        )
        .await,
    );
    assert!(text.contains("not persisted"), "{text}");
    assert_eq!(ctx.shell_state.lock().unwrap().cwd, original);
}

#[tokio::test]
async fn sandbox_requests_fail_closed_and_destructive_commands_are_denied() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = test_ctx(dir.path());
    ctx.sandbox = Some(rc_core::tool::SandboxPolicy { allow_net: true });
    assert!(matches!(
        call("Write-Output unsafe", &ctx).await,
        ToolOutcome::Error { .. }
    ));
    ctx.sandbox = None;
    assert!(matches!(
        call("Remove-Item -Recurse C:\\", &ctx).await,
        ToolOutcome::Denied { .. }
    ));
}

#[tokio::test]
async fn capture_is_bounded_and_includes_head_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let out = call(
        "[Console]::Write('HEAD' + ('x' * 3000000) + 'TAIL')",
        &test_ctx(dir.path()),
    )
    .await;
    match out {
        ToolOutcome::Ok {
            content, truncated, ..
        } => {
            assert!(truncated);
            assert!(content.contains("HEAD") && content.ends_with("TAIL"));
            assert!(content.len() < CAPTURE_BYTES + 200);
        }
        other => panic!("{other:?}"),
    }
}

fn descendant_script(pid_file: &Path, wait: bool) -> String {
    format!("$p = Start-Process -FilePath $env:ComSpec -ArgumentList '/d','/c','ping -n 40 127.0.0.1 >nul' -PassThru; [IO.File]::WriteAllText({}, $p.Id.ToString()); Write-Output READY; {}", quote(pid_file), if wait { "Start-Sleep -Seconds 30" } else { "" })
}
async fn pid_from_file(path: &Path) -> u32 {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("child never started: {}", path.display());
}
fn assert_dead(pid: u32) {
    let status = Command::new(env_hygiene::resolve_shell(ShellKind::PowerShell).unwrap())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }}"),
        ])
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "descendant {pid} still alive");
}

#[tokio::test]
async fn timeout_preserves_partial_output_and_kills_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pid");
    let result = powershell()
        .call(
            json!({"command": descendant_script(&file, true), "timeout_ms": 5000}),
            &test_ctx(dir.path()),
        )
        .await
        .unwrap();
    match result {
        ToolOutcome::Error { message, .. } => assert!(
            message.contains("timed out") && message.contains("READY"),
            "{message}"
        ),
        other => panic!("{other:?}"),
    }
    assert_dead(pid_from_file(&file).await);
}

#[tokio::test]
async fn cancellation_and_dropped_future_kill_descendants() {
    for abort in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let ctx = test_ctx(dir.path());
        let cancel = ctx.cancel.clone();
        let script = descendant_script(&file, true);
        let task = tokio::spawn(async move { call(&script, &ctx).await });
        let pid = pid_from_file(&file).await;
        if abort {
            task.abort();
        } else {
            cancel.cancel();
        }
        let _ = task.await;
        assert_dead(pid);
    }
}

#[tokio::test]
async fn normal_foreground_exit_cleans_detached_children() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pid");
    let text = content(call(&descendant_script(&file, false), &test_ctx(dir.path())).await);
    assert!(text.starts_with("exit: 0"), "{text}");
    assert_dead(pid_from_file(&file).await);
}

#[tokio::test]
async fn background_merges_streams_and_shutdown_kills_tree() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pid");
    let ctx = test_ctx(dir.path());
    ctx.shell_state.lock().unwrap().bg_dir = Some(dir.path().join("logs"));
    let script = format!(
        "[Console]::Error.WriteLine('ERR'); {}",
        descendant_script(&file, true)
    );
    let result = powershell()
        .call(json!({"command": script, "run_in_background": true}), &ctx)
        .await
        .unwrap();
    assert!(content(result).contains("Background shell"));
    let pid = pid_from_file(&file).await;
    let shell = ctx.shell_state.lock().unwrap().bg[0].clone();
    for _ in 0..100 {
        let log = std::fs::read_to_string(&shell.log_path).unwrap_or_default();
        if log.contains("ERR") && log.contains("READY") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let log = std::fs::read_to_string(&shell.log_path).unwrap();
    assert!(log.contains("ERR") && log.contains("READY"), "{log}");
    ctx.shell_state.lock().unwrap().shutdown();
    assert_eq!(*shell.status.lock().unwrap(), BgShellStatus::Killed);
    assert_dead(pid);
}

#[tokio::test]
async fn background_logs_rotate_and_reap_after_exit() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = test_ctx(dir.path());
    ctx.shell_state.lock().unwrap().bg_dir = Some(dir.path().join("logs"));
    let result = powershell().call(json!({"command": "[Console]::Write('x' * 6000000); Write-Output TAIL", "run_in_background": true}), &ctx).await.unwrap();
    let _ = content(result);
    let shell = ctx.shell_state.lock().unwrap().bg[0].clone();
    for _ in 0..200 {
        if *shell.status.lock().unwrap() != BgShellStatus::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(*shell.status.lock().unwrap(), BgShellStatus::Exited(0));
    let log = std::fs::read_to_string(&shell.log_path).unwrap();
    assert!(log.contains("TAIL"), "tail missing");
    assert!(std::fs::metadata(&shell.log_path).unwrap().len() < 4 * 1024 * 1024);
    assert!(PathBuf::from(format!("{}.1", shell.log_path.display())).exists());
}

#[tokio::test]
async fn optional_git_bash_or_actionable_missing_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let bash = Bash::with_kind(Ok(ShellKind::Bash), 0);
    let result = bash
        .call(
            json!({"command": "printf BASH_OK; false | true"}),
            &test_ctx(dir.path()),
        )
        .await
        .unwrap();
    match env_hygiene::resolve_shell(ShellKind::Bash) {
        Ok(_) => {
            let text = content(result);
            assert!(
                text.starts_with("exit: 1") && text.contains("BASH_OK"),
                "{text}"
            );
        }
        Err(_) => assert!(
            matches!(result, ToolOutcome::Error { message, .. } if message.contains("Git Bash"))
        ),
    }
}
