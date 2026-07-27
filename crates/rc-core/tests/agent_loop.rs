//! Agent-loop test: a `MockModel` + a test tool, end-to-end with zero network.
//! (§13: "A MockModel that replays a scripted sequence of assistant messages
//! lets you test compaction, interrupts, and error paths deterministically.")

use async_trait::async_trait;
use rc_core::{
    agent::AgentLoop, model::EventSink, model::FinalizedToolCall, model::Model, model::ModelError,
    model::ModelRequest, model::ModelResponse, model::NullSink, project::project,
    project::verify_invariant, registry::ToolRegistry, tool::Concurrency, tool::Tool, tool::ToolCtx,
    tool::ToolError, tool::ToolOutcome, turn::Session, turn::ToolCall, turn::Turn, AllowAllChecker,
    Mode, NullPrompter, PermissionChecker, PermissionEngine,
};
use rc_core::{AskResponse, FinishReason, Prompter, ToolResultBody, Usage};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// A model that replays a scripted queue of responses.
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
    async fn complete(&self, _req: ModelRequest, _sink: &dyn EventSink) -> Result<ModelResponse, ModelError> {
        let mut q = self.responses.lock().unwrap();
        Ok(q.pop_front().expect("scripted responses exhausted"))
    }
}

/// A test tool that echoes its `msg` argument.
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

#[tokio::test]
async fn loop_runs_tool_then_answers() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s1".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(&mut session, "do it".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);

    // User, Assistant(calls), ToolResult, Assistant(text).
    assert_eq!(session.messages.len(), 4);

    // The loop must keep the tool-answer invariant true on every projection.
    let wire = project(&session.messages);
    assert!(verify_invariant(&wire).is_ok());

    match &session.messages[2] {
        Turn::ToolResult { result, .. } => assert!(result.render().contains("echo: hi")),
        other => panic!("expected a tool result, got {other:?}"),
    }
    match &session.messages[3] {
        Turn::Assistant { text, .. } => assert_eq!(text, "done"),
        other => panic!("expected final assistant text, got {other:?}"),
    }
}

#[tokio::test]
async fn loop_feeds_back_a_parse_error_as_a_tool_result() {
    // The model emits a tool call with arguments that won't parse. The loop must
    // synthesize a `role:tool` error result (§3.3) so the model can retry.
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::ParseError {
                id: Some("c1".into()),
                name: Some("Echo".into()),
                raw: "{not json".into(),
                error: "expected `}`".into(),
            }],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            text: "recovered".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s2".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "x".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    // The parse-error tool result must answer the (synthesized) call id c1.
    let wire = project(&session.messages);
    assert!(verify_invariant(&wire).is_ok(), "parse-error result must satisfy the invariant");

    match &session.messages[2] {
        Turn::ToolResult { result, .. } => assert!(result.render().contains("invalid tool arguments")),
        other => panic!("expected a parse-error tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn ask_escalation_denied_by_null_prompter_becomes_a_denied_result() {
    // Default mode, no rules: a mutating tool call escalates to Ask; the
    // NullPrompter denies it, so the model sees a denied tool result (the tool
    // never runs). "Edit" isn't even registered — permission is checked first.
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let engine = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Edit".into(),
                arguments: "{}".into(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            text: "ok".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, engine);

    let mut session = Session::new("s3".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "edit it".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    assert!(verify_invariant(&project(&session.messages)).is_ok());
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => {
            assert!(result.render().contains("denied"), "expected a denied result, got {:?}", result.render())
        }
        other => panic!("expected a denied tool result, got {other:?}"),
    }
}

// ---- loop event surface (M4) ------------------------------------------------

/// An `EventSink` that records the order of sink calls for assertion.
#[derive(Default)]
struct RecordingSink(Mutex<Vec<String>>);
impl EventSink for RecordingSink {
    fn on_text(&self, _d: &str) { self.0.lock().unwrap().push("text".into()); }
    fn on_reasoning(&self, _d: &str) { self.0.lock().unwrap().push("reasoning".into()); }
    fn on_tool_start(&self, _c: &ToolCall) { self.0.lock().unwrap().push("tool_start".into()); }
    fn on_tool_end(&self, _id: &str, _t: &str, _r: &ToolResultBody) {
        self.0.lock().unwrap().push("tool_end".into());
    }
    fn on_iter(&self, c: u32, _m: u32) { self.0.lock().unwrap().push(format!("iter{c}")); }
    fn on_usage(&self, _u: &Usage) { self.0.lock().unwrap().push("usage".into()); }
    fn on_finish(&self, _r: &FinishReason) { self.0.lock().unwrap().push("finish".into()); }
}

#[tokio::test]
async fn loop_emits_iter_tool_end_and_usage() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
                prompt_tokens_details: None,
            }),
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);
    let sink = RecordingSink::default();
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "do it".into(), &sink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    let events = sink.0.lock().unwrap().clone();
    let joined = events.join(",");
    assert!(joined.contains("iter1"), "iter1: {joined}");
    assert!(joined.contains("tool_end"), "tool_end: {joined}");
    assert!(joined.contains("usage"), "usage: {joined}");
    // The tool_end (iter 1) precedes the usage event (iter 2).
    let tool_end_at = events.iter().position(|e| e == "tool_end").unwrap();
    let usage_at = events.iter().position(|e| e == "usage").unwrap();
    assert!(tool_end_at < usage_at, "tool_end before usage: {joined}");
}

// ---- async prompter (M4) ----------------------------------------------------

/// A prompter that returns a fixed answer (to test Once vs Session grants).
struct MockPrompter {
    response: AskResponse,
}
#[async_trait]
impl Prompter for MockPrompter {
    async fn ask(&self, _tool: &str, _input: &Value, _reason: &str) -> AskResponse {
        self.response.clone()
    }
}

fn edit_call(id: &str) -> ModelResponse {
    ModelResponse {
        text: String::new(),
        reasoning: None,
        tool_calls: vec![FinalizedToolCall::Call(ToolCall {
            id: id.into(),
            name: "Edit".into(),
            arguments: "{}".into(),
        })],
        finish_reason: rc_proto::FinishReason::ToolCalls,
        usage: None,
    }
}

fn stop_response(text: &str) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: rc_proto::FinishReason::Stop,
        usage: None,
    }
}

#[tokio::test]
async fn async_prompter_once_allows_the_call_without_a_grant() {
    // Default mode, no rules: Edit (unregistered) escalates to Ask. `Once` allows
    // it; the tool lookup then fails (unknown tool), and no grant is kept.
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let engine = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let model = Arc::new(MockModel::new(vec![edit_call("c1"), stop_response("ok")])) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, engine);
    let prompter = MockPrompter { response: AskResponse::Once };
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "edit it".into(), &NullSink, &prompter, CancellationToken::new())
        .await
        .unwrap();
    assert!(session.perm_grants.is_empty(), "Once adds no grant: {:?}", session.perm_grants);
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => {
            assert!(matches!(result, ToolResultBody::Error { .. }), "allowed -> unknown tool: {:?}", result);
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn async_prompter_session_adds_a_grant() {
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let engine = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let model = Arc::new(MockModel::new(vec![edit_call("c1"), stop_response("ok")])) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, engine);
    let prompter = MockPrompter { response: AskResponse::Session("Edit".into()) };
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "edit it".into(), &NullSink, &prompter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(session.perm_grants, vec!["Edit".to_string()]);
}
