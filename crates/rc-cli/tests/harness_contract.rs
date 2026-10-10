//! Offline harness contracts through the real CLI, SSE client, context
//! assembler, permission engine, and tools. Scripted responses make runtime
//! failures reproducible; they do not measure a live model's coding ability.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::{tempdir, TempDir};
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Clone)]
struct Script(Arc<Mutex<VecDeque<ResponseTemplate>>>);

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ResponseTemplate::new(503).set_body_string("script exhausted"))
    }
}

fn response(delta: Value, finish: &str) -> ResponseTemplate {
    let chunk = json!({
        "id": "harness-fixture", "object": "chat.completion.chunk",
        "created": 1, "model": "harness-fixture",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
}

fn answer(text: &str, finish: &str) -> ResponseTemplate {
    response(json!({"content": text}), finish)
}

fn call(id: &str, name: &str, arguments: Value) -> ResponseTemplate {
    response(
        json!({"tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()},
        }]}),
        "tool_calls",
    )
}

struct Run {
    _directory: TempDir,
    workspace: PathBuf,
    output: Output,
    report: Value,
    trajectory: Value,
    requests: Vec<Value>,
}

async fn run(script: Vec<ResponseTemplate>, max_iters: u32, timeout_ms: u64, mode: &str) -> Run {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Script(Arc::new(Mutex::new(script.into()))))
        .mount(&server)
        .await;

    let directory = tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let profile = directory.path().join("profile");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(workspace.join("answer.txt"), "answer: 41\n").unwrap();
    let report_path = directory.path().join("report.json");
    let trajectory_path = directory.path().join("trajectory.json");

    let mut command = Command::new(env!("CARGO_BIN_EXE_marathon"));
    command
        .env_clear()
        .env("HOME", &profile)
        .env("USERPROFILE", &profile)
        .env("SC_API_KEY", "dummy-harness-contract-key")
        .env("SC_DLR_ENABLED", "false")
        .env("SC_REQUEST_GZIP", "false")
        .env("SC_MAX_RETRIES", "0")
        .env("SC_MAX_ITERS", max_iters.to_string())
        .env("SC_TURN_TIMEOUT_MS", timeout_ms.to_string())
        .env("SC_DEFAULT_MODE", mode)
        .env("SC_RESOURCE_LIMITS", "0")
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .arg("--base-url")
        .arg(format!("{}/v1", server.uri()))
        .args(["--model", "harness-fixture", "--benchmark-report"])
        .arg(&report_path)
        .arg("--benchmark-trajectory")
        .arg(&trajectory_path)
        .args([
            "-p",
            "Change the fixture answer to 42 and verify the result.",
        ]);
    // Retain only OS launch essentials. Provider settings, credentials, proxies,
    // and real user configuration must not affect these offline evaluations.
    for variable in ["PATH", "SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(variable) {
            command.env(variable, value);
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("CLI did not finish within the fixture budget")
        .expect("could not launch CLI");
    let report = serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    let trajectory = serde_json::from_slice(&std::fs::read(&trajectory_path).unwrap()).unwrap();
    let requests = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    Run {
        _directory: directory,
        workspace,
        output,
        report,
        trajectory,
        requests,
    }
}

fn assert_terminal(run: &Run, outcome: &str, success: bool) {
    assert_eq!(
        run.report["outcome"],
        outcome,
        "stderr: {}",
        String::from_utf8_lossy(&run.output.stderr)
    );
    assert_eq!(
        run.output.status.code(),
        Some(if success { 0 } else { 1 }),
        "{outcome}: stdout={} stderr={}",
        String::from_utf8_lossy(&run.output.stdout),
        String::from_utf8_lossy(&run.output.stderr)
    );
    assert!(!run.trajectory["steps"].as_array().unwrap().is_empty());
    assert!(!run
        .report
        .to_string()
        .contains("dummy-harness-contract-key"));
    assert!(!run
        .trajectory
        .to_string()
        .contains("dummy-harness-contract-key"));
}

#[tokio::test]
async fn clean_stop_returns_success_with_final_artifacts() {
    let run = run(vec![answer("Completed", "stop")], 8, 0, "default").await;
    assert_terminal(&run, "stop", true);
    assert_eq!(run.requests.len(), 1);
    assert_eq!(run.report["request_count"], 1);
}

#[tokio::test]
async fn iteration_limit_returns_failure_after_preserving_tool_results() {
    let run = run(
        vec![call("read", "Read", json!({"file_path": "answer.txt"}))],
        1,
        0,
        "default",
    )
    .await;
    assert_terminal(&run, "iteration_limit", false);
    assert_eq!(run.report["tool_call_count"], 1);
    assert!(run.trajectory.to_string().contains("answer: 41"));
}

#[tokio::test]
async fn repeated_partial_answers_return_failure_and_keep_the_partial_prefix() {
    let run = run(
        vec![
            answer("Partial first answer", "length"),
            answer("Partial second answer", "length"),
        ],
        8,
        0,
        "default",
    )
    .await;
    assert_terminal(&run, "length", false);
    assert_eq!(run.requests.len(), 2);
    assert!(run.trajectory.to_string().contains("Partial first answer"));
    assert!(run.trajectory.to_string().contains("Partial second answer"));
}

#[tokio::test]
async fn repeated_reasoning_only_limits_return_no_progress_failure() {
    let hidden = json!({"reasoning_content": "reasoning-fixture-marker"});
    let run = run(
        vec![
            response(hidden.clone(), "length"),
            response(hidden, "length"),
        ],
        8,
        0,
        "default",
    )
    .await;
    assert_terminal(&run, "no_progress", false);
    assert_eq!(run.requests.len(), 2);
    assert!(!run
        .trajectory
        .to_string()
        .contains("reasoning-fixture-marker"));
}

#[tokio::test]
async fn content_filter_returns_failure_with_diagnostic_artifacts() {
    let run = run(
        vec![answer("Blocked partial response", "content_filter")],
        8,
        0,
        "default",
    )
    .await;
    assert_terminal(&run, "incomplete", false);
    assert!(run
        .trajectory
        .to_string()
        .contains("Blocked partial response"));
}

#[tokio::test]
async fn turn_timeout_returns_failure_and_finalizes_artifacts() {
    let run = run(
        vec![answer("Too late", "stop").set_delay(Duration::from_secs(5))],
        8,
        500,
        "default",
    )
    .await;
    assert_terminal(&run, "time_limit", false);
    assert!(run.requests.len() <= 1);
}

#[tokio::test]
async fn provider_failure_returns_failure_and_records_the_attempt() {
    let run = run(
        vec![ResponseTemplate::new(503).set_body_string("fixture unavailable")],
        8,
        0,
        "default",
    )
    .await;
    assert_terminal(&run, "incomplete", false);
    assert_eq!(run.report["request_count"], 1);
    assert_eq!(
        run.report["requests"][0]["effective_finish_reason"],
        "error"
    );
}

#[tokio::test]
async fn default_permissions_deny_writes_and_preserve_the_original_file() {
    let run = run(
        vec![
            call(
                "write",
                "Write",
                json!({"file_path": "answer.txt", "content": "answer: 42\n"}),
            ),
            answer("The write was denied", "stop"),
            answer("The file remains unchanged", "stop"),
        ],
        8,
        0,
        "default",
    )
    .await;
    assert_terminal(&run, "stop", true);
    assert_eq!(run.report["tool_denied_count"], 1);
    assert_eq!(
        std::fs::read_to_string(run.workspace.join("answer.txt")).unwrap(),
        "answer: 41\n"
    );
    assert!(run.requests[1]["messages"].to_string().contains("[denied:"));
}

#[tokio::test]
async fn read_edit_verify_workflow_is_graded_by_the_actual_file() {
    let run = run(
        vec![
            call("read-before", "Read", json!({"file_path": "answer.txt"})),
            call(
                "edit",
                "Edit",
                json!({"file_path": "answer.txt", "old_string": "41", "new_string": "42"}),
            ),
            call("read-after", "Read", json!({"file_path": "answer.txt"})),
            answer("Updated and verified the answer", "stop"),
            answer("The final audit confirms the update", "stop"),
        ],
        8,
        0,
        "acceptEdits",
    )
    .await;
    assert_terminal(&run, "stop", true);
    assert_eq!(
        std::fs::read_to_string(run.workspace.join("answer.txt")).unwrap(),
        "answer: 42\n"
    );
    assert_eq!(run.report["tool_call_count"], 3);
    assert_eq!(run.report["tool_error_count"], 0);
    assert_eq!(run.report["request_count"], 5);
    assert!(run.requests[1]["messages"]
        .to_string()
        .contains("answer: 41"));
    assert!(run.requests[3]["messages"]
        .to_string()
        .contains("answer: 42"));
    assert!(run.requests[4]["messages"]
        .to_string()
        .contains("benchmark completion gate"));
}
