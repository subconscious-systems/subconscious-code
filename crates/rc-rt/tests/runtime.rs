//! Headless runtime tests (§13): drive `Runtime` with a `MockModel` and assert
//! on the `AgentEvent` stream — no terminal, no network. Mirrors the
//! `MockModel`+`Echo` pattern in `rc-core/tests/agent_loop.rs` and
//! `rc-cli/tests/*.rs`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rc_core::{
    AgentLoop, AgentMode, AllowAllChecker, Artifact, AskResponse, Concurrency, EventSink,
    FinalizedToolCall, FinishReason, LoopOutcome, Mode, Model, ModelError, ModelRequest,
    ModelResponse, PermissionChecker, PermissionEngine, Session, Tool, ToolCall, ToolCtx,
    ToolError, ToolOutcome, ToolRegistry, Turn,
};
use rc_rt::{AgentEvent, EventStream, Runtime, UserAction};
use serde_json::{json, Value};

// ---- scripted model + echo tool -------------------------------------------

struct MockModel {
    responses: Mutex<VecDeque<ModelResponse>>,
}
impl MockModel {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}
#[async_trait]
impl Model for MockModel {
    async fn complete(
        &self,
        _req: ModelRequest,
        sink: &dyn EventSink,
    ) -> Result<ModelResponse, ModelError> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted responses exhausted");
        // Exercise the streaming seam the way ChatModel would: one text delta
        // and a tool_start per finalized call.
        if !resp.text.is_empty() {
            sink.on_text(&resp.text);
        }
        for fc in &resp.tool_calls {
            if let FinalizedToolCall::Call(c) = fc {
                sink.on_tool_start(c);
            }
        }
        Ok(resp)
    }
}

struct Echo;
#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Echo back the `msg` argument."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "msg": { "type": "string" } }, "required": ["msg"] })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }
    async fn call(&self, input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let msg = input
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("(none)");
        Ok(ToolOutcome::ok(format!("echo: {msg}")))
    }
}

struct ChangeFile;
#[async_trait]
impl Tool for ChangeFile {
    fn name(&self) -> &str {
        "ChangeFile"
    }
    fn description(&self) -> &str {
        "Emit a deterministic file-change artifact for runtime tests."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::SerialWrite
    }
    async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        Ok(ToolOutcome::Ok {
            content: "changed".into(),
            truncated: false,
            artifacts: vec![Artifact::FileChange {
                path: "src/main.rs".into(),
                before: Some(b"old\n".to_vec().into()),
                after: Some(b"new\nmore\n".to_vec().into()),
            }],
        })
    }
}

fn resp_with_call(id: &str, name: &str, args: Value) -> ModelResponse {
    ModelResponse {
        retries: 0,
        text: String::new(),
        reasoning: None,
        tool_calls: vec![FinalizedToolCall::Call(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string().into(),
        })],
        finish_reason: FinishReason::ToolCalls,
        usage: None,
    }
}
fn resp_stop(text: &str) -> ModelResponse {
    ModelResponse {
        retries: 0,
        text: text.to_string(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn agent(
    model: Arc<dyn Model>,
    tools: Arc<ToolRegistry>,
    perm: Arc<dyn PermissionChecker>,
) -> Arc<AgentLoop> {
    Arc::new(AgentLoop::new(model, tools, perm))
}

fn session() -> Session {
    Session::new("t".into(), std::env::temp_dir(), "mock".into())
}

// ---- broadcast drain helpers ----------------------------------------------

async fn drain_until(rx: &mut EventStream, stop: impl Fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        match rx.recv().await {
            Some(Ok(ev)) => {
                let done = stop(&ev);
                out.push(ev);
                if done {
                    return out;
                }
            }
            Some(Err(n)) => eprintln!("warn: broadcast lagged by {n}"),
            None => panic!("event stream closed before stop"),
        }
    }
}

async fn wait_for(rx: &mut EventStream, pred: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
    loop {
        match rx.recv().await {
            Some(Ok(ev)) if pred(&ev) => return ev,
            Some(Ok(_)) => continue,
            Some(Err(_)) => continue,
            None => panic!("event stream closed"),
        }
    }
}

// ---- tests ----------------------------------------------------------------

#[tokio::test]
async fn turn_emits_tool_and_outcome_events() {
    let tools = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let model = Arc::new(MockModel::new(vec![
        resp_with_call("c1", "Echo", json!({"msg":"hi"})),
        resp_stop("done"),
    ])) as Arc<dyn Model>;
    let rt = Runtime::new(
        agent(
            model,
            tools,
            Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
        ),
        session(),
        None,
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("hi".into()));

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    assert!(got.iter().any(|e| matches!(e, AgentEvent::Ready)), "Ready");
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t == "done")),
        "final text delta"
    );
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { call } if call.name == "Echo")),
        "ToolStart Echo"
    );
    assert!(
        got.iter().any(|e| matches!(e, AgentEvent::ToolEnd { tool, result, .. } if tool == "Echo" && result.render().contains("echo: hi"))),
        "ToolEnd Echo -> echo: hi"
    );
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Stop))),
        "Outcome Stop"
    );
    rt.shutdown().await;
}

#[tokio::test]
async fn event_queue_coalesces_a_burst_without_losing_text() {
    struct BurstModel;
    #[async_trait]
    impl Model for BurstModel {
        async fn complete(
            &self,
            _req: ModelRequest,
            sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            for index in 0..1_000 {
                sink.on_text(&format!("{index},"));
            }
            Ok(resp_stop("done"))
        }
    }

    let rt = Runtime::new(
        agent(
            Arc::new(BurstModel),
            Arc::new(ToolRegistry::new(vec![])),
            Arc::new(AllowAllChecker),
        ),
        session(),
        None,
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("burst".into()));
    // Deliberately let the producer get ahead of the host. The former
    // 256-entry broadcast ring lost the first events in this exact shape.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let got = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    let streamed = got
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(streamed.matches(',').count(), 1_000);
    assert!(streamed.starts_with("0,1,2,"));
    assert!(streamed.ends_with("999,"));
    assert!(got
        .iter()
        .any(|event| matches!(event, AgentEvent::Outcome(LoopOutcome::Stop))));
    rt.shutdown().await;
}

#[tokio::test]
async fn duplicate_submit_is_rejected_until_the_active_turn_finishes() {
    struct GatedModel {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Model for GatedModel {
        async fn complete(
            &self,
            _req: ModelRequest,
            sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            self.entered.notify_one();
            self.release.notified().await;
            sink.on_text("first turn finished");
            Ok(resp_stop("first turn finished"))
        }
    }

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let rt = Runtime::new(
        agent(
            Arc::new(GatedModel {
                entered: entered.clone(),
                release: release.clone(),
            }),
            Arc::new(ToolRegistry::new(vec![])),
            Arc::new(AllowAllChecker),
        ),
        session(),
        None,
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("first".into()));
    entered.notified().await;
    rt.action(UserAction::Submit("duplicate".into()));

    let notice = wait_for(&mut rx, |event| {
        matches!(event, AgentEvent::Notice(message) if message.contains("duplicate submission"))
    })
    .await;
    assert!(matches!(notice, AgentEvent::Notice(_)));

    release.notify_one();
    let got = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    assert!(got
        .iter()
        .any(|event| matches!(event, AgentEvent::Text(text) if text == "first turn finished")));
    rt.shutdown().await;
}

#[tokio::test]
async fn queued_prompt_starts_after_the_active_turn_finishes() {
    struct QueuedModel {
        calls: AtomicUsize,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Model for QueuedModel {
        async fn complete(
            &self,
            _req: ModelRequest,
            sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let text = if call == 0 {
                "first finished"
            } else {
                "queued finished"
            };
            sink.on_text(text);
            Ok(resp_stop(text))
        }
    }

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let rt = Runtime::new(
        agent(
            Arc::new(QueuedModel {
                calls: AtomicUsize::new(0),
                entered: entered.clone(),
                release: release.clone(),
            }),
            Arc::new(ToolRegistry::new(vec![])),
            Arc::new(AllowAllChecker),
        ),
        session(),
        None,
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("first".into()));
    entered.notified().await;
    rt.action(UserAction::Queue("follow up".into()));
    release.notify_one();

    let got = tokio::time::timeout(Duration::from_secs(2), async {
        let mut events = Vec::new();
        let mut idles = 0;
        while idles < 2 {
            match rx.recv().await {
                Some(Ok(event)) => {
                    if matches!(event, AgentEvent::Idle) {
                        idles += 1;
                    }
                    events.push(event);
                }
                Some(Err(_)) => {}
                None => panic!("event stream closed before queued turn"),
            }
        }
        events
    })
    .await
    .expect("queued turn timed out");

    assert_eq!(
        got.iter()
            .filter(|event| matches!(event, AgentEvent::Ready))
            .count(),
        2
    );
    assert!(got
        .iter()
        .any(|event| matches!(event, AgentEvent::Text(text) if text == "first finished")));
    assert!(got
        .iter()
        .any(|event| matches!(event, AgentEvent::Text(text) if text == "queued finished")));
    rt.shutdown().await;
}

#[tokio::test]
async fn cancelling_an_active_turn_preserves_and_starts_its_queue() {
    struct CancelThenRunModel {
        calls: AtomicUsize,
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Model for CancelThenRunModel {
        async fn complete(
            &self,
            _req: ModelRequest,
            sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                std::future::pending::<()>().await;
                unreachable!();
            }
            sink.on_text("queue survived cancellation");
            Ok(resp_stop("queue survived cancellation"))
        }
    }

    let entered = Arc::new(tokio::sync::Notify::new());
    let rt = Runtime::new(
        agent(
            Arc::new(CancelThenRunModel {
                calls: AtomicUsize::new(0),
                entered: entered.clone(),
            }),
            Arc::new(ToolRegistry::new(vec![])),
            Arc::new(AllowAllChecker),
        ),
        session(),
        None,
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("first".into()));
    entered.notified().await;
    rt.action(UserAction::Queue("follow up".into()));
    rt.action(UserAction::Cancel);

    let got = tokio::time::timeout(Duration::from_secs(2), async {
        drain_until(&mut rx, |event| {
            matches!(event, AgentEvent::Text(text) if text == "queue survived cancellation")
        })
        .await
    })
    .await
    .expect("queued turn did not start after cancellation");
    assert!(got
        .iter()
        .any(|event| matches!(event, AgentEvent::Outcome(LoopOutcome::Cancelled))));
    rt.shutdown().await;
}

#[tokio::test]
async fn file_change_artifact_arrives_before_its_tool_end() {
    let tools = Arc::new(ToolRegistry::new(vec![
        Arc::new(ChangeFile) as Arc<dyn Tool>
    ]));
    let model = Arc::new(MockModel::new(vec![
        resp_with_call("change-1", "ChangeFile", json!({})),
        resp_stop("done"),
    ])) as Arc<dyn Model>;
    let rt = Runtime::new(
        agent(
            model,
            tools,
            Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
        ),
        session(),
        None,
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("change it".into()));

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    let artifact_at = got
        .iter()
        .position(|e| matches!(e, AgentEvent::Artifact { call_id, .. } if call_id == "change-1"))
        .expect("file artifact");
    let end_at = got
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolEnd { call_id, .. } if call_id == "change-1"))
        .expect("tool end");
    assert!(
        artifact_at < end_at,
        "artifact must render before completion"
    );
    match &got[artifact_at] {
        AgentEvent::Artifact {
            artifact:
                Artifact::FileChange {
                    path,
                    before,
                    after,
                },
            ..
        } => {
            assert_eq!(path, std::path::Path::new("src/main.rs"));
            assert_eq!(before.as_deref(), Some(b"old\n".as_slice()));
            assert_eq!(after.as_deref(), Some(b"new\nmore\n".as_slice()));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    rt.shutdown().await;
}

#[tokio::test]
async fn permission_ask_is_resolved_by_user_action() {
    // Default mode, no rules, Edit not registered → Ask (checked before tool lookup).
    let tools = Arc::new(ToolRegistry::new(vec![]));
    let perm = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let model = Arc::new(MockModel::new(vec![
        resp_with_call("c1", "Edit", json!({})),
        resp_stop("ok"),
    ])) as Arc<dyn Model>;
    let rt = Runtime::new(agent(model, tools, perm), session(), None);
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("edit it".into()));

    let ask = wait_for(&mut rx, |e| matches!(e, AgentEvent::PermissionAsk { .. })).await;
    let id = match ask {
        AgentEvent::PermissionAsk { id, .. } => id,
        _ => unreachable!(),
    };
    rt.action(UserAction::PermissionAnswer {
        id,
        response: AskResponse::Once,
    });

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    assert!(
        got.iter().any(|e| matches!(
            e,
            AgentEvent::PermissionDecision {
                response: AskResponse::Once,
                ..
            }
        )),
        "PermissionDecision Once"
    );
    // Edit isn't registered, but the Ask was approved → unknown-tool error result.
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::ToolEnd { tool, .. } if tool == "Edit")),
        "ToolEnd Edit"
    );
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Stop))),
        "Outcome Stop"
    );
    rt.shutdown().await;
}

#[tokio::test]
async fn cancel_during_ask_denies_and_ends_turn() {
    let tools = Arc::new(ToolRegistry::new(vec![]));
    let perm = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let model = Arc::new(MockModel::new(vec![
        resp_with_call("c1", "Edit", json!({})),
        resp_stop("ok"),
    ])) as Arc<dyn Model>;
    let rt = Runtime::new(agent(model, tools, perm), session(), None);
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("edit it".into()));

    let _ask = wait_for(&mut rx, |e| matches!(e, AgentEvent::PermissionAsk { .. })).await;
    rt.action(UserAction::Cancel);

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    // Cancellation now interrupts the active permission/tool phase instead of
    // winding through another model request. The pump may also publish a Deny
    // decision while clearing the pending prompt, but that event is not needed
    // to make the turn terminal and invariant-safe.
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Cancelled))),
        "Outcome Cancelled"
    );
    rt.shutdown().await;
}

#[tokio::test]
async fn set_mode_bypasses_ask_for_a_mutating_call() {
    // An Edit call that would Ask in Default mode runs without an Ask once the
    // mode is cycled to Auto (enforcement changes atomically).
    let tools = Arc::new(ToolRegistry::new(vec![]));
    let perm = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let model = Arc::new(MockModel::new(vec![
        resp_with_call("c1", "Edit", json!({})),
        resp_stop("ok"),
    ])) as Arc<dyn Model>;
    let rt = Runtime::new(agent(model, tools, perm), session(), None);
    let mut rx = rt.subscribe();

    rt.action(UserAction::SetMode(AgentMode::Auto));
    let _ = wait_for(&mut rx, |e| {
        matches!(e, AgentEvent::ModeChanged(AgentMode::Auto))
    })
    .await;
    rt.action(UserAction::Submit("edit it".into()));

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    assert!(
        !got.iter()
            .any(|e| matches!(e, AgentEvent::PermissionAsk { .. })),
        "no Ask in bypass"
    );
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::ToolEnd { tool, .. } if tool == "Edit")),
        "ToolEnd Edit"
    );
    assert!(
        got.iter()
            .any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Stop))),
        "Outcome Stop"
    );
    rt.shutdown().await;
}

#[tokio::test]
async fn session_store_persists_turns_after_each_run() {
    // A turn that calls Echo then stops should append User+Assistant+ToolResult
    // turns to the SessionStore, and re-loading the file must reproduce them.
    use rc_rt::SessionStore;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");

    let tools = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let model = Arc::new(MockModel::new(vec![
        resp_with_call("c1", "Echo", json!({"msg":"hi"})),
        resp_stop("done"),
    ])) as Arc<dyn Model>;
    let perm = Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>;
    let agent = agent(model, tools, perm);
    let session = session();

    let store = SessionStore::create(path.clone(), &session).unwrap();
    let rt = Runtime::new(agent, session, Some(store));
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("hello".into()));

    // Drain to idle so the driver has flushed the turn to the store.
    let _ = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    rt.shutdown().await;

    // The file exists and re-loads the conversation we just ran.
    assert!(path.exists(), "session file was created");
    let loaded = rc_session::load(&path).unwrap();
    // User("hello") + Assistant(tool-call) + ToolResult + Assistant("done") = 4.
    assert_eq!(
        loaded.messages.len(),
        4,
        "expected 4 persisted turns: {:?}",
        loaded.messages
    );
    assert!(
        matches!(&loaded.messages[0], Turn::User { content, .. } if content.as_ref() == "hello")
    );
    // The final assistant turn carries the "done" text.
    assert!(matches!(&loaded.messages[3], Turn::Assistant { text, .. } if text.as_ref() == "done"));
}

#[tokio::test]
async fn session_store_is_replayable_while_the_next_request_is_still_running() {
    use rc_rt::SessionStore;

    struct PausingModel {
        calls: Mutex<u32>,
        second_started: Arc<tokio::sync::Notify>,
        release_second: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl Model for PausingModel {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            let call = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            if call == 1 {
                return Ok(resp_with_call("checkpoint", "Echo", json!({"msg":"saved"})));
            }
            self.second_started.notify_one();
            self.release_second.notified().await;
            Ok(resp_stop("done"))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incremental.jsonl");
    let second_started = Arc::new(tokio::sync::Notify::new());
    let release_second = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(PausingModel {
        calls: Mutex::new(0),
        second_started: second_started.clone(),
        release_second: release_second.clone(),
    });
    let initial = session();
    let store = SessionStore::create(path.clone(), &initial).unwrap();
    let rt = Runtime::new(
        agent(
            model,
            Arc::new(ToolRegistry::new(vec![Arc::new(Echo)])),
            Arc::new(AllowAllChecker),
        ),
        initial,
        Some(store),
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("checkpoint this".into()));
    second_started.notified().await;

    // Persistence runs on its own blocking writer thread. The second model
    // request proves all three records have been enqueued, but it must not be
    // used as a scheduling signal that the writer thread has already flushed
    // them. Wait briefly for the documented durable state while the request is
    // still paused, instead of racing the filesystem immediately.
    let loaded = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let loaded = rc_session::load(&path).unwrap();
            if loaded.messages.len() == 3 {
                break loaded;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| rc_session::load(&path).unwrap());
    assert_eq!(loaded.messages.len(), 3, "user/call/result must be durable");
    assert!(matches!(loaded.messages[2], Turn::ToolResult { .. }));

    release_second.notify_one();
    let _ = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    rt.shutdown().await;
}

#[tokio::test]
async fn session_store_persists_the_latest_mode_for_resume() {
    use rc_rt::SessionStore;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mode.jsonl");
    let model = Arc::new(MockModel::new(vec![resp_stop("done")])) as Arc<dyn Model>;
    let tools = Arc::new(ToolRegistry::new(vec![]));
    let perm = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let session = session();
    let store = SessionStore::create(path.clone(), &session).unwrap();
    let rt = Runtime::new(agent(model, tools, perm), session, Some(store));
    let mut rx = rt.subscribe();

    rt.action(UserAction::SetMode(AgentMode::Auto));
    let _ = wait_for(&mut rx, |event| {
        matches!(event, AgentEvent::ModeChanged(AgentMode::Auto))
    })
    .await;
    // A completed run is a driver-queue barrier: the prior SetMode command and
    // its append-only metadata note have been handled before Idle arrives.
    rt.action(UserAction::Submit("continue here".into()));
    let _ = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    rt.shutdown().await;

    let loaded = rc_session::load(&path).unwrap();
    assert_eq!(loaded.mode, AgentMode::Auto);
    assert!(loaded.messages.iter().any(|turn| matches!(
        turn,
        Turn::SystemNote {
            kind: rc_core::NoteKind::ModeChange,
            text,
        } if text == "auto"
    )));
}

#[tokio::test]
async fn compact_persists_a_summary_and_moves_the_projection_boundary() {
    use rc_rt::SessionStore;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compact.jsonl");
    let model = Arc::new(MockModel::new(vec![])) as Arc<dyn Model>;
    let tools = Arc::new(ToolRegistry::new(vec![]));
    let perm = Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>;
    let mut session = session();
    session.messages.push(Turn::User {
        content: "old request with bulky context".into(),
        ts: std::time::SystemTime::now(),
    });
    session.messages.push(Turn::Assistant {
        text: "important result to retain".into(),
        reasoning: Some("private reasoning is omitted".into()),
        calls: vec![],
        usage: None,
        cost: None,
        trace: None,
    });
    session.messages.push(Turn::ToolResult {
        call_id: "old-tool".into(),
        tool: "Read".into(),
        result: rc_core::ToolResultBody::Ok {
            content: "bulky raw tool output must leave context".into(),
            truncated: false,
        },
        duration: Default::default(),
    });

    let mut store = SessionStore::create(path.clone(), &session).unwrap();
    for turn in &session.messages {
        store.append_turn(turn).unwrap();
    }
    let rt = Runtime::new(agent(model, tools, perm), session, Some(store));
    let mut rx = rt.subscribe();
    rt.action(UserAction::Compact);
    let events = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    rt.shutdown().await;

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Notice(text) if text.contains("Context compacted")
    )));
    let loaded = rc_session::load(&path).unwrap();
    let summary = loaded
        .messages
        .iter()
        .find_map(|turn| match turn {
            Turn::SystemNote {
                kind: rc_core::NoteKind::Compaction,
                text,
            } => Some(text),
            _ => None,
        })
        .expect("persisted compaction marker");
    assert!(summary.contains("important result to retain"));
    assert!(!summary.contains("private reasoning is omitted"));

    let projected = rc_core::project(&loaded.messages)
        .into_iter()
        .map(|message| format!("{message:?}"))
        .collect::<String>();
    assert!(!projected.contains("bulky raw tool output must leave context"));
    assert!(projected.contains("important result to retain"));
}

#[tokio::test]
async fn goal_set_show_and_clear_are_persisted_session_state() {
    use rc_rt::SessionStore;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("goal.jsonl");
    let model = Arc::new(MockModel::new(vec![])) as Arc<dyn Model>;
    let tools = Arc::new(ToolRegistry::new(vec![]));
    let perm = Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>;
    let session = session();
    let store = SessionStore::create(path.clone(), &session).unwrap();
    let rt = Runtime::new(agent(model, tools, perm), session, Some(store));
    let mut rx = rt.subscribe();

    rt.action(UserAction::SetGoal(Some("ship the release".into())));
    let set = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    assert!(set.iter().any(|event| matches!(
        event,
        AgentEvent::Notice(text) if text == "Session goal set: ship the release"
    )));

    rt.action(UserAction::ShowGoal);
    let shown = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    assert!(shown.iter().any(|event| matches!(
        event,
        AgentEvent::Notice(text) if text == "Active goal: ship the release"
    )));

    rt.action(UserAction::SetGoal(None));
    let _ = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    rt.action(UserAction::ShowGoal);
    let cleared = drain_until(&mut rx, |event| matches!(event, AgentEvent::Idle)).await;
    rt.shutdown().await;
    assert!(cleared.iter().any(|event| matches!(
        event,
        AgentEvent::Notice(text) if text.contains("No active goal")
    )));

    let loaded = rc_session::load(&path).unwrap();
    let goals = loaded
        .messages
        .iter()
        .filter_map(|turn| match turn {
            Turn::SystemNote {
                kind: rc_core::NoteKind::Goal,
                text,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(goals, vec!["ship the release", ""]);
}
