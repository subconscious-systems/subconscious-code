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
    let hub = McpHub::connect(vec![fixture("fx", &[])], Vec::new()).await;
    let mut names: Vec<String> = hub.tools().iter().map(|t| t.name().to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "mcp__fx__crash",
            "mcp__fx__echo",
            "mcp__fx__fail",
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
    let hub = McpHub::connect(vec![fixture("fx", &[])], Vec::new()).await;
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
                startup_timeout: Duration::from_secs(5),
                tool_timeout: Duration::from_secs(5),
            },
        ],
        vec![("typo".into(), "unknown field `comand`".into())],
    )
    .await;

    let names: Vec<String> = hub.tools().iter().map(|t| t.name().to_string()).collect();
    assert!(
        names.iter().all(|n| n.starts_with("mcp__good__")),
        "{names:?}"
    );
    assert_eq!(names.len(), 4);

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
    let hub = McpHub::connect(vec![config], Vec::new()).await;
    let sleep = tool(&hub, "mcp__fx__sleep");

    let (ok, body) = text(sleep.call(json!({"ms": 3000}), &ctx()).await.unwrap());
    assert!(!ok);
    assert!(body.contains("did not answer"), "{body}");

    let cancelled = ctx();
    cancelled.cancel.cancel();
    let outcome = sleep.call(json!({"ms": 3000}), &cancelled).await.unwrap();
    assert!(matches!(outcome, ToolOutcome::Interrupted), "{outcome:?}");
}
