//! Headless runtime tests (§13): drive `Runtime` with a `MockModel` and assert
//! on the `AgentEvent` stream — no terminal, no network. Mirrors the
//! `MockModel`+`Echo` pattern in `rc-core/tests/agent_loop.rs` and
//! `rc-cli/tests/*.rs`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rc_core::{
    AgentLoop, AgentMode, AllowAllChecker, AskResponse, Concurrency, EventSink, FinalizedToolCall,
    FinishReason, LoopOutcome, Mode, Model, ModelError, ModelRequest, ModelResponse,
    PermissionChecker, PermissionEngine, Session, Tool, ToolCall, ToolCtx, ToolError, ToolOutcome,
    ToolRegistry, Turn,
};
use rc_rt::{AgentEvent, EventStream, Runtime, UserAction};
use serde_json::{json, Value};

// ---- scripted model + echo tool -------------------------------------------

struct MockModel {
    responses: Mutex<VecDeque<ModelResponse>>,
}
impl MockModel {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self { responses: Mutex::new(responses.into_iter().collect()) }
    }
}
#[async_trait]
impl Model for MockModel {
    async fn complete(&self, _req: ModelRequest, sink: &dyn EventSink) -> Result<ModelResponse, ModelError> {
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
    fn name(&self) -> &str { "Echo" }
    fn description(&self) -> &str { "Echo back the `msg` argument." }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "msg": { "type": "string" } }, "required": ["msg"] })
    }
    fn concurrency(&self) -> Concurrency { Concurrency::Parallel }
    async fn call(&self, input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let msg = input.get("msg").and_then(|v| v.as_str()).unwrap_or("(none)");
        Ok(ToolOutcome::ok(format!("echo: {msg}")))
    }
}

fn resp_with_call(id: &str, name: &str, args: Value) -> ModelResponse {
    ModelResponse {
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
        text: text.to_string(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
        usage: None,
    }
}

fn agent(model: Arc<dyn Model>, tools: Arc<ToolRegistry>, perm: Arc<dyn PermissionChecker>) -> Arc<AgentLoop> {
    Arc::new(AgentLoop::new(model, tools, perm))
}

fn session() -> Session {
    Session::new("t".into(), std::env::temp_dir(), "mock".into())
}

// ---- broadcast drain helpers ----------------------------------------------

async fn drain_until(
    rx: &mut EventStream,
    stop: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
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

async fn wait_for(
    rx: &mut EventStream,
    pred: impl Fn(&AgentEvent) -> bool,
) -> AgentEvent {
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
        agent(model, tools, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>),
        session(),
        None,
    );
    let mut rx = rt.subscribe();
    rt.action(UserAction::Submit("hi".into()));

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    assert!(got.iter().any(|e| matches!(e, AgentEvent::Ready)), "Ready");
    assert!(got.iter().any(|e| matches!(e, AgentEvent::Text(t) if t == "done")), "final text delta");
    assert!(got.iter().any(|e| matches!(e, AgentEvent::ToolStart { call } if call.name == "Echo")), "ToolStart Echo");
    assert!(
        got.iter().any(|e| matches!(e, AgentEvent::ToolEnd { tool, result, .. } if tool == "Echo" && result.render().contains("echo: hi"))),
        "ToolEnd Echo -> echo: hi"
    );
    assert!(got.iter().any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Stop))), "Outcome Stop");
    rt.shutdown();
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
    rt.action(UserAction::PermissionAnswer { id, response: AskResponse::Once });

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    assert!(
        got.iter().any(|e| matches!(e, AgentEvent::PermissionDecision { response: AskResponse::Once, .. })),
        "PermissionDecision Once"
    );
    // Edit isn't registered, but the Ask was approved → unknown-tool error result.
    assert!(got.iter().any(|e| matches!(e, AgentEvent::ToolEnd { tool, .. } if tool == "Edit")), "ToolEnd Edit");
    assert!(got.iter().any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Stop))), "Outcome Stop");
    rt.shutdown();
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
    // Cancel drains the pending ask as Deny → the loop makes a denied result and
    // the model replies (M4a can't abort the turn mid-stream; it winds to Stop).
    assert!(
        got.iter().any(|e| matches!(e, AgentEvent::PermissionDecision { response: AskResponse::Deny(_), .. })),
        "PermissionDecision Deny"
    );
    assert!(
        got.iter().any(|e| matches!(e, AgentEvent::ToolEnd { tool, result, .. } if tool == "Edit" && matches!(result, rc_core::ToolResultBody::Denied { .. }))),
        "ToolEnd Edit Denied"
    );
    assert!(got.iter().any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Stop))), "Outcome Stop");
    rt.shutdown();
}

#[tokio::test]
async fn set_mode_bypasses_ask_for_a_mutating_call() {
    // An Edit call that would Ask in Default mode runs without an Ask once the
    // mode is cycled to BypassPermissions (enforcement changes atomically).
    let tools = Arc::new(ToolRegistry::new(vec![]));
    let perm = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let model = Arc::new(MockModel::new(vec![
        resp_with_call("c1", "Edit", json!({})),
        resp_stop("ok"),
    ])) as Arc<dyn Model>;
    let rt = Runtime::new(agent(model, tools, perm), session(), None);
    let mut rx = rt.subscribe();

    rt.action(UserAction::SetMode(AgentMode::BypassPermissions));
    let _ =
        wait_for(&mut rx, |e| matches!(e, AgentEvent::ModeChanged(AgentMode::BypassPermissions))).await;
    rt.action(UserAction::Submit("edit it".into()));

    let got = drain_until(&mut rx, |e| matches!(e, AgentEvent::Idle)).await;
    assert!(!got.iter().any(|e| matches!(e, AgentEvent::PermissionAsk { .. })), "no Ask in bypass");
    assert!(got.iter().any(|e| matches!(e, AgentEvent::ToolEnd { tool, .. } if tool == "Edit")), "ToolEnd Edit");
    assert!(got.iter().any(|e| matches!(e, AgentEvent::Outcome(LoopOutcome::Stop))), "Outcome Stop");
    rt.shutdown();
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
    rt.shutdown();

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
    assert!(matches!(&loaded.messages[0], Turn::User { content, .. } if content.as_ref() == "hello"));
    // The final assistant turn carries the "done" text.
    assert!(matches!(&loaded.messages[3], Turn::Assistant { text, .. } if text.as_ref() == "done"));
}
