//! End-to-end: rc-mcp against the stdio fixture server in `tests/fixture`.

use rc_core::state::ReadRegistry;
use rc_core::tool::{Tool, ToolCtx, ToolOutcome};
use rc_core::{ChangeJournal, ShellState};
use rc_mcp::{McpHub, ServerConfig, ServerState, Transport};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str, env: &[(&str, &str)]) -> ServerConfig {
    ServerConfig {
        name: name.into(),
        transport: Transport::Stdio {
            command: env!("CARGO_BIN_EXE_rc-mcp-fixture").into(),
            args: Vec::new(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            cwd: None,
        },
        display: format!("stdio: fixture {name}"),
        secrets: Vec::new(),
        startup_timeout: Duration::from_secs(20),
        tool_timeout: Duration::from_secs(5),
    }
}

fn ctx() -> ToolCtx {
    let dir = std::env::temp_dir();
    ToolCtx {
        cwd: dir.clone(),
        allowed_roots: vec![dir.clone()],
        cancel: CancellationToken::new(),
        read_registry: Arc::new(Mutex::new(ReadRegistry::new())),
        shell_state: Arc::new(Mutex::new(ShellState::new(dir))),
        change_journal: Arc::new(Mutex::new(ChangeJournal::new())),
        sandbox: None,
    }
}

fn tool(hub: &McpHub, name: &str) -> Arc<dyn Tool> {
    hub.tools()
        .into_iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

fn text(outcome: ToolOutcome) -> (bool, String) {
    match outcome {
        ToolOutcome::Ok { content, .. } => (true, content),
        ToolOutcome::Error { message, .. } => (false, message),
        other => panic!("unexpected outcome {other:?}"),
    }
}

#[tokio::test]
async fn tools_are_listed_namespaced_and_callable() {
    let hub = McpHub::connect(vec![fixture("fx", &[])], Vec::new(), Vec::new()).await;
    let mut names: Vec<String> = hub.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "mcp__fx__crash",
            "mcp__fx__echo",
            "mcp__fx__env",
            "mcp__fx__fail",
            "mcp__fx__family",
            "mcp__fx__sleep"
        ]
    );

    // Schemas are made provider-safe even when the server sends `{}`.
    assert_eq!(tool(&hub, "mcp__fx__crash").schema()["type"], "object");

    let echo = tool(&hub, "mcp__fx__echo");
    let (ok, body) = text(echo.call(json!({"text": "hi"}), &ctx()).await.unwrap());
    assert!(ok);
    assert_eq!(body, "echo: hi");

    let (ok, body) = text(
        tool(&hub, "mcp__fx__fail")
            .call(json!({}), &ctx())
            .await
            .unwrap(),
    );
    assert!(!ok);
    assert_eq!(body, "it went wrong");
}

#[tokio::test]
async fn a_crashed_server_reports_errors_instead_of_failing_the_session() {
    let hub = McpHub::connect(vec![fixture("fx", &[])], Vec::new(), Vec::new()).await;
    let (ok, _) = text(
        tool(&hub, "mcp__fx__crash")
            .call(json!({}), &ctx())
            .await
            .unwrap(),
    );
    assert!(!ok);

    // Later calls answer at once with the reason, including the stderr tail.
    let (ok, body) = text(
        tool(&hub, "mcp__fx__echo")
            .call(json!({"text": "again"}), &ctx())
            .await
            .unwrap(),
    );
    assert!(!ok);
    assert!(body.contains("not running"), "{body}");
    let state = &hub.status()[0].state;
    assert!(matches!(state, ServerState::Closed { .. }), "{state:?}");
}

#[tokio::test]
async fn a_server_that_cannot_start_is_reported_and_others_still_work() {
    let hub = McpHub::connect(
        vec![
            fixture(
                "broken",
                &[
                    ("FIXTURE_EXIT_EARLY", "1"),
                    ("FIXTURE_STDERR", "boot failed: no token"),
                ],
            ),
            fixture("good", &[]),
            ServerConfig {
                name: "missing".into(),
                transport: Transport::Stdio {
                    command: "rc-mcp-no-such-binary".into(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    cwd: None,
                },
                display: "stdio: rc-mcp-no-such-binary".into(),
                secrets: Vec::new(),
                startup_timeout: Duration::from_secs(5),
                tool_timeout: Duration::from_secs(5),
            },
        ],
        vec![("typo".into(), "unknown field `comand`".into())],
        Vec::new(),
    )
    .await;

    let names: Vec<String> = hub.tools().iter().map(|t| t.name().to_string()).collect();
    assert!(
        names.iter().all(|n| n.starts_with("mcp__good__")),
        "{names:?}"
    );
    assert_eq!(names.len(), 6);

    let status = hub.status();
    let by_name = |n: &str| status.iter().find(|s| s.name == n).unwrap().state.clone();
    match by_name("broken") {
        ServerState::Failed { error } => assert!(error.contains("boot failed"), "{error}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(by_name("missing"), ServerState::Failed { .. }));
    assert!(matches!(by_name("typo"), ServerState::Failed { .. }));
    assert_eq!(hub.problems().len(), 3);
    let report = hub.report_lines().join("\n");
    assert!(report.contains("good  connected"), "{report}");
    assert!(report.contains("mcp__good__echo"), "{report}");
}

#[tokio::test]
async fn slow_calls_time_out_and_cancellation_interrupts() {
    let mut config = fixture("fx", &[]);
    config.tool_timeout = Duration::from_millis(300);
    let hub = McpHub::connect(vec![config], Vec::new(), Vec::new()).await;
    let sleep = tool(&hub, "mcp__fx__sleep");

    let (ok, body) = text(sleep.call(json!({"ms": 3000}), &ctx()).await.unwrap());
    assert!(!ok);
    assert!(body.contains("did not answer"), "{body}");

    let cancelled = ctx();
    cancelled.cancel.cancel();
    let outcome = sleep.call(json!({"ms": 3000}), &cancelled).await.unwrap();
    assert!(matches!(outcome, ToolOutcome::Interrupted), "{outcome:?}");
}

#[tokio::test]
async fn servers_get_only_the_allowlisted_environment() {
    std::env::set_var("RC_MCP_TEST_PARENT_SECRET", "must-not-leak");
    let hub = McpHub::connect(
        vec![fixture("fx", &[("FIXTURE_CONFIGURED", "yes")])],
        Vec::new(),
        Vec::new(),
    )
    .await;
    let (ok, names) = text(
        tool(&hub, "mcp__fx__env")
            .call(json!({}), &ctx())
            .await
            .unwrap(),
    );
    assert!(ok, "{names}");
    let names: Vec<&str> = names.lines().collect();
    assert!(!names.contains(&"RC_MCP_TEST_PARENT_SECRET"), "{names:?}");
    assert!(names.contains(&"FIXTURE_CONFIGURED"), "{names:?}");
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("PATH")),
        "{names:?}"
    );
    #[cfg(windows)]
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("SystemRoot")),
        "{names:?}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn substituted_secrets_never_appear_in_status_or_problems() {
    let mut config = fixture(
        "leaky",
        &[
            ("FIXTURE_EXIT_EARLY", "1"),
            ("FIXTURE_STDERR", "auth failed for tok-9f8e7d6c"),
        ],
    );
    config.display = "stdio: fixture --token ${API_TOKEN}".into();
    config.secrets = vec!["tok-9f8e7d6c".into()];
    let hub = McpHub::connect(vec![config], Vec::new(), Vec::new()).await;
    let shown = format!(
        "{}\n{}",
        hub.report_lines().join("\n"),
        hub.problems().join("\n")
    );
    assert!(!shown.contains("tok-9f8e7d6c"), "{shown}");
    assert!(shown.contains("${API_TOKEN}"), "{shown}");
    assert!(shown.contains("auth failed for ***"), "{shown}");
}

#[tokio::test]
async fn untrusted_servers_are_listed_but_not_started() {
    let hub = McpHub::connect(
        Vec::new(),
        Vec::new(),
        vec![("project-srv".into(), "project server not trusted".into())],
    )
    .await;
    assert!(hub.tools().is_empty());
    assert!(matches!(hub.status()[0].state, ServerState::Skipped { .. }));
    assert!(hub
        .report_lines()
        .join("\n")
        .contains("project-srv  not started"));
}

#[tokio::test]
async fn shutdown_kills_the_server_and_its_children() {
    let hub = McpHub::connect(vec![fixture("fx", &[])], Vec::new(), Vec::new()).await;
    let (ok, pids) = text(
        tool(&hub, "mcp__fx__family")
            .call(json!({}), &ctx())
            .await
            .unwrap(),
    );
    assert!(ok, "{pids}");
    let pids: Vec<u32> = pids
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(
        pids.iter().all(|&pid| alive(pid)),
        "{pids:?} should be running"
    );

    hub.shutdown().await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pids.iter().any(|&pid| alive(pid)) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        pids.iter().all(|&pid| !alive(pid)),
        "{pids:?} survived shutdown"
    );
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // A killed grandchild stays a zombie until init reaps it; `ps` shows `Z`.
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    out.status.success() && !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

#[tokio::test]
async fn http_connect_errors_do_not_leak_a_url_token() {
    // Bind and drop a listener so the port refuses connections.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_value(json!({
        "web": {"url": format!("http://127.0.0.1:{port}/mcp?key=${{TOK}}"), "startup_timeout_ms": 10000},
    }))
    .unwrap();
    let lookup = |name: &str| (name == "TOK").then(|| "ab/cd+ef=gh@ij".to_string());
    let (servers, errors) = rc_mcp::parse_servers(&raw, &lookup);
    assert!(errors.is_empty(), "{errors:?}");
    let hub = McpHub::connect(servers, Vec::new(), Vec::new()).await;
    let shown = format!(
        "{}\n{}",
        hub.report_lines().join("\n"),
        hub.problems().join("\n")
    );
    assert!(
        matches!(hub.status()[0].state, ServerState::Failed { .. }),
        "{shown}"
    );
    for fragment in ["cd+ef", "gh@ij", "cd%2Bef", "gh%40ij"] {
        assert!(!shown.contains(fragment), "{fragment} leaked: {shown}");
    }
    assert!(shown.contains("${TOK}"), "{shown}");
}

/// Kills the HTTP fixture when the test ends, pass or fail.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn http_servers_list_and_call_tools_with_configured_headers() {
    use std::io::BufRead;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rc-mcp-fixture"))
        .arg("--http")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _server = KillOnDrop(child);
    let url = line.trim().strip_prefix("listening ").unwrap().to_string();

    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_value(json!({
        "web": {"type": "http", "url": url, "headers": {"Authorization": "Bearer ${TOK}"}},
    }))
    .unwrap();
    let lookup = |name: &str| (name == "TOK").then(|| "http-token-123".to_string());
    let (servers, errors) = rc_mcp::parse_servers(&raw, &lookup);
    assert!(errors.is_empty(), "{errors:?}");
    let hub = McpHub::connect(servers, Vec::new(), Vec::new()).await;
    assert!(hub.problems().is_empty(), "{:?}", hub.problems());

    let (ok, body) = text(
        tool(&hub, "mcp__web__echo")
            .call(json!({"text": "over http"}), &ctx())
            .await
            .unwrap(),
    );
    assert!(ok, "{body}");
    assert_eq!(body, "echo: over http");
    let (ok, body) = text(
        tool(&hub, "mcp__web__whoami")
            .call(json!({}), &ctx())
            .await
            .unwrap(),
    );
    assert!(ok, "{body}");
    assert_eq!(body, "Bearer http-token-123");
    hub.shutdown().await;
}
