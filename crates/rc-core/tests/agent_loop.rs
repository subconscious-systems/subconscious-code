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
use std::time::{Duration, SystemTime};
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
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
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
        Turn::Assistant { text, .. } => assert_eq!(text.as_ref(), "done"),
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
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
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

// ---- T3: turn wall-clock budget -------------------------------------------

#[tokio::test]
async fn turn_timeout_ends_a_long_loop() {
    // A model that always requests a tool call (so the loop would otherwise run
    // to the iteration budget) but sleeps 20 ms per call. A 50 ms turn budget
    // stops it after a few iterations — well before MAX_ITERS.
    struct Slow {
        delay: Duration,
        n: Arc<Mutex<u32>>,
    }
    #[async_trait]
    impl Model for Slow {
        async fn complete(&self, _req: ModelRequest, _sink: &dyn EventSink) -> Result<ModelResponse, ModelError> {
            let n = { let mut c = self.n.lock().unwrap(); *c += 1; *c };
            tokio::time::sleep(self.delay).await;
            Ok(ModelResponse {
                text: String::new(),
                reasoning: None,
                tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                    id: format!("c{n}"),
                    name: "Echo".into(),
                    arguments: r#"{"msg":"x"}"#.into(),
                })],
                finish_reason: rc_proto::FinishReason::ToolCalls,
                usage: None,
            })
        }
    }

    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let calls = Arc::new(Mutex::new(0u32));
    let model = Arc::new(Slow { delay: Duration::from_millis(20), n: calls.clone() }) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>)
        .with_turn_timeout(Some(Duration::from_millis(50)));

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let start = SystemTime::now();
    let outcome = agent
        .run(&mut session, "loop".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    let elapsed = start.elapsed().unwrap_or_default();
    let n = *calls.lock().unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::TimeUp);
    assert!(elapsed < Duration::from_secs(2), "should stop near 50 ms, took {elapsed:?}");
    assert!((2..50).contains(&n), "should run a few iters ({n}), not to the limit");
}

// ---- M3: cross-turn usage accumulation ------------------------------------

#[tokio::test]
async fn accumulates_usage_across_iterations() {
    fn usage(p: u64, c: u64) -> Usage {
        Usage { prompt_tokens: p, completion_tokens: c, total_tokens: p + c, prompt_tokens_details: None }
    }
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"x"}"#.into(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: Some(usage(10, 2)),
        },
        ModelResponse {
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: Some(usage(20, 3)),
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "go".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    let t = &session.total_usage;
    assert_eq!(t.prompt_tokens, 30, "prompt summed across iterations");
    assert_eq!(t.completion_tokens, 5, "completion summed across iterations");
    assert_eq!(t.total_tokens, 35, "total summed across iterations");
}

// ---- early-return invariant (§4.2 / project.rs:69) --------------------------
//
// A finish_reason that isn't `ToolCalls` but the assistant DID emit a tool call
// (provider anomaly, or a stream cut mid-tool-call). The loop must NOT execute
// the tool, and must synthesize an `Interrupted` result so the tool-answer
// invariant holds — otherwise the next turn's projection has an unanswered
// assistant tool_call (debug_assert panic / malformed request to the provider).

#[tokio::test]
async fn stop_with_outstanding_call_synthesizes_an_interrupted_result() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![ModelResponse {
        text: String::new(),
        reasoning: None,
        tool_calls: vec![FinalizedToolCall::Call(ToolCall {
            id: "c1".into(),
            name: "Echo".into(),
            arguments: r#"{"msg":"hi"}"#.to_string().into(),
        })],
        finish_reason: rc_proto::FinishReason::Stop,
        usage: None,
    }];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(&mut session, "do it".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);

    // User, Assistant(calls), ToolResult(Interrupted) — the tool was NOT executed.
    assert_eq!(session.messages.len(), 3);
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => {
            assert!(matches!(result, ToolResultBody::Interrupted), "expected Interrupted, got {result:?}");
        }
        other => panic!("expected a synthesized tool result, got {other:?}"),
    }
    assert!(verify_invariant(&project(&session.messages)).is_ok(), "invariant must hold");
}

#[tokio::test]
async fn length_with_outstanding_call_synthesizes_an_interrupted_result() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![ModelResponse {
        text: "partial".into(),
        reasoning: None,
        tool_calls: vec![FinalizedToolCall::Call(ToolCall {
            id: "c1".into(),
            name: "Echo".into(),
            arguments: r#"{"msg":"hi"}"#.to_string().into(),
        })],
        finish_reason: rc_proto::FinishReason::Length,
        usage: None,
    }];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(&mut session, "do it".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Length);
    assert!(
        verify_invariant(&project(&session.messages)).is_ok(),
        "length exit with outstanding calls must still satisfy the invariant"
    );
    assert!(session.messages.iter().any(|t| matches!(
        t,
        Turn::ToolResult { result: ToolResultBody::Interrupted, .. }
    )));
}

// ---- A13: auto-continue on finish_reason=length ----------------------------

#[tokio::test]
async fn length_without_tool_calls_auto_continues() {
    // The model returns finish_reason=Length with text but no tool calls. The
    // loop must inject a "continue" user turn and re-request, so the model
    // finishes the answer instead of stopping with a warning.
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let responses = vec![
        ModelResponse {
            text: "partial answer".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
        ModelResponse {
            text: " done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(&mut session, "explain".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);

    // User, Assistant(partial), User(continue), Assistant(done).
    assert_eq!(session.messages.len(), 4);
    // The auto-injected continue turn.
    match &session.messages[2] {
        Turn::User { content, .. } => assert_eq!(content.as_ref(), "continue", "injected turn"),
        other => panic!("expected an injected user turn, got {other:?}"),
    }
    match &session.messages[3] {
        Turn::Assistant { text, .. } => assert_eq!(text.as_ref(), " done"),
        other => panic!("expected the continued answer, got {other:?}"),
    }
    assert!(verify_invariant(&project(&session.messages)).is_ok(), "invariant must hold");
}

#[tokio::test]
async fn length_with_outstanding_call_stops_and_synthesizes() {
    // finish_reason=Length mid-tool-call: the stream was cut. The loop must NOT
    // auto-continue (the assistant message has outstanding calls); it
    // synthesizes Interrupted results and stops with LoopOutcome::Length.
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![ModelResponse {
        text: "partial".into(),
        reasoning: None,
        tool_calls: vec![FinalizedToolCall::Call(ToolCall {
            id: "c1".into(),
            name: "Echo".into(),
            arguments: r#"{"msg":"hi"}"#.to_string().into(),
        })],
        finish_reason: rc_proto::FinishReason::Length,
        usage: None,
    }];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(&mut session, "do it".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Length);
    assert!(
        verify_invariant(&project(&session.messages)).is_ok(),
        "length exit with outstanding calls must still satisfy the invariant"
    );
    assert!(session.messages.iter().any(|t| matches!(
        t,
        Turn::ToolResult { result: ToolResultBody::Interrupted, .. }
    )));
}

// ---- context assembler (M6) -------------------------------------------------

/// A model that captures the first request's messages so the test can inspect
/// the system prompt the loop actually sent.
struct CapturingModel {
    captured: Arc<Mutex<Option<ModelRequest>>>,
    response: ModelResponse,
}
#[async_trait]
impl Model for CapturingModel {
    async fn complete(&self, req: ModelRequest, _sink: &dyn EventSink) -> Result<ModelResponse, ModelError> {
        *self.captured.lock().unwrap() = Some(req);
        Ok(self.response.clone())
    }
}

/// A minimal context assembler that emits a fixed system prompt and otherwise
/// forwards to the legacy `project` path — enough to prove the loop uses it.
struct FixedAssembler {
    prompt: String,
}
impl rc_core::ContextAssembler for FixedAssembler {
    fn assemble(&self, turns: &[Turn]) -> Vec<rc_proto::WireMessage> {
        rc_core::project_with(turns, &self.prompt)
    }
    fn system_prompt(&self) -> Option<&str> {
        Some(&self.prompt)
    }
}

#[tokio::test]
async fn loop_uses_the_wired_context_assembler() {
    // With an assembler set, the loop must send its system prompt, not the
    // legacy default. We capture the request and inspect message[0].
    let captured = Arc::new(Mutex::new(None));
    let response = ModelResponse {
        text: "done".into(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: rc_proto::FinishReason::Stop,
        usage: None,
    };
    let model = Arc::new(CapturingModel { captured: captured.clone(), response }) as Arc<dyn Model>;
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>)
        .with_assembler(Arc::new(FixedAssembler { prompt: "SENTINEL SYSTEM PROMPT".into() })
            as Arc<dyn rc_core::ContextAssembler>);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "hi".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    let req = captured.lock().unwrap().clone().expect("a request was captured");
    use rc_proto::WireMessage;
    match req.messages.first() {
        Some(WireMessage::System { content }) => {
            assert_eq!(content.as_ref(), "SENTINEL SYSTEM PROMPT", "loop must use the wired assembler");
        }
        other => panic!("expected a system message, got {other:?}"),
    }
}

#[tokio::test]
async fn loop_without_assembler_uses_legacy_default_prompt() {
    // With no assembler set, the loop falls back to the legacy `project` path
    // (M1–M5 behavior) — the system message is the default identity prompt.
    let captured = Arc::new(Mutex::new(None));
    let response = ModelResponse {
        text: "done".into(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: rc_proto::FinishReason::Stop,
        usage: None,
    };
    let model = Arc::new(CapturingModel { captured: captured.clone(), response }) as Arc<dyn Model>;
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "hi".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    let req = captured.lock().unwrap().clone().expect("a request was captured");
    use rc_proto::WireMessage;
    match req.messages.first() {
        Some(WireMessage::System { content }) => {
            assert!(content.contains("You are `sc`"), "default prompt: {content}");
            assert!(!content.contains("SENTINEL"), "no custom prompt without an assembler");
        }
        other => panic!("expected a system message, got {other:?}"),
    }
}

// ---- F9: a panicking parallel tool is isolated, not propagated --------------

/// A parallel tool that panics inside `call` — a stand-in for a tool-impl bug.
struct PanicTool;
#[async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str { "PanicTool" }
    fn description(&self) -> &str { "Panics on purpose." }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }
    fn concurrency(&self) -> Concurrency { Concurrency::Parallel }
    async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        panic!("PanicTool: simulated tool-impl bug");
    }
}

#[tokio::test]
async fn a_panicking_parallel_tool_becomes_an_error_not_a_crash() {
    // A tool whose `call` panics must not take down the agent loop. The loop
    // surfaces a non-retryable error result for that call and continues to a
    // normal Stop — the invariant holds and no panic escapes `run`.
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(PanicTool) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "PanicTool".into(),
                arguments: "{}".into(),
            })],
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

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    // The run must NOT panic (the tool's panic is isolated by tokio).
    let outcome = agent
        .run(&mut session, "do it".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);

    // The panicked call becomes a non-retryable error result (not a propagated
    // panic), and the invariant still holds.
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => match result {
            ToolResultBody::Error { message, retryable } => {
                assert!(message.contains("panic"), "expected a panic error, got {message}");
                assert!(!*retryable, "a panic is not retryable");
            }
            other => panic!("expected an Error result, got {other:?}"),
        },
        other => panic!("expected a tool result, got {other:?}"),
    }
    assert!(verify_invariant(&project(&session.messages)).is_ok(), "invariant must hold");
}

// ---- unlimited context (M8) -------------------------------------------------

/// The product claim, at the loop level: a large tool result reaches the wire
/// whole. This is the regression guard against reintroducing a silent per-result
/// cap anywhere between the tool and the request.
#[tokio::test]
async fn large_tool_results_reach_the_wire_uncapped() {
    use rc_core::{Tool, ToolCtx, ToolError, ToolOutcome};
    use serde_json::Value;

    // Comfortably past every cap that used to exist (16 KB tool result,
    // 30 KB Bash output).
    const BODY: usize = 512 * 1024;

    struct Fat;
    #[async_trait]
    impl Tool for Fat {
        fn name(&self) -> &str {
            "Fat"
        }
        fn description(&self) -> &str {
            "Returns a large body."
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome::ok("q".repeat(BODY)))
        }
    }

    // Turn 1 calls the tool; turn 2 answers. The second request carries the
    // tool result, which is the one we inspect.
    let requests = Arc::new(Mutex::new(Vec::new()));
    struct TwoTurn {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }
    #[async_trait]
    impl Model for TwoTurn {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            let first = {
                let mut r = self.requests.lock().unwrap();
                r.push(req);
                r.len() == 1
            };
            Ok(if first {
                ModelResponse {
                    text: String::new(),
                    reasoning: None,
                    tool_calls: vec![rc_core::model::FinalizedToolCall::Call(
                        rc_core::ToolCall {
                            id: "c1".into(),
                            name: "Fat".into(),
                            arguments: "{}".into(),
                        },
                    )],
                    finish_reason: rc_proto::FinishReason::ToolCalls,
                    usage: None,
                }
            } else {
                ModelResponse {
                    text: "done".into(),
                    reasoning: None,
                    tool_calls: vec![],
                    finish_reason: rc_proto::FinishReason::Stop,
                    usage: None,
                }
            })
        }
    }

    let model = Arc::new(TwoTurn { requests: requests.clone() }) as Arc<dyn Model>;
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Fat) as Arc<dyn Tool>]));
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "go".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 2, "expected a tool turn then an answer turn");
    let tool_msg = reqs[1]
        .messages
        .iter()
        .find_map(|m| match m {
            rc_proto::WireMessage::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("the tool result is on the wire");
    assert_eq!(
        tool_msg.len(),
        BODY,
        "the tool body must reach the model whole — a cap crept back in"
    );
    assert!(!tool_msg.contains("truncated"), "no truncation sentinel: {}", &tool_msg[..80]);
}

// ---- hard runaway backstop --------------------------------------------------

#[tokio::test]
async fn hard_tool_result_cap_clips_a_runaway_output() {
    // A tool returns a body over the hard backstop. The loop must clip it
    // before it enters the session, so the model sees a truncated result with
    // a sentinel — not the full runaway output. Set the cap small for the test.
    use rc_core::{Tool, ToolCtx, ToolError, ToolOutcome};

    struct Huge;
    #[async_trait]
    impl Tool for Huge {
        fn name(&self) -> &str { "Huge" }
        fn description(&self) -> &str { "Returns a runaway body." }
        fn schema(&self) -> Value { json!({"type": "object", "properties": {}}) }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            // 10 KB — over the 4 KB test cap, under the 100 MB default.
            Ok(ToolOutcome::ok("z".repeat(10_000)))
        }
    }

    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Huge) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Huge".into(),
                arguments: "{}".into(),
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
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>)
        .with_hard_tool_result_cap(4_000);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "go".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    // The tool result in the session must be truncated.
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => match result {
            ToolResultBody::Ok { content, truncated } => {
                assert!(*truncated, "must be flagged truncated by the hard cap");
                assert!(content.contains("truncated"), "sentinel present: {content}");
                assert!(
                    content.len() < 10_000,
                    "body was not clipped: {} bytes",
                    content.len()
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        },
        other => panic!("expected a tool result, got {other:?}"),
    }
    assert!(verify_invariant(&project(&session.messages)).is_ok());
}

#[tokio::test]
async fn hard_tool_result_cap_zero_disables_it() {
    // With the cap at 0, even a very large tool result passes through whole.
    use rc_core::{Tool, ToolCtx, ToolError, ToolOutcome};

    struct Big;
    #[async_trait]
    impl Tool for Big {
        fn name(&self) -> &str { "Big" }
        fn description(&self) -> &str { "Returns a big body." }
        fn schema(&self) -> Value { json!({"type": "object", "properties": {}}) }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome::ok("y".repeat(50_000)))
        }
    }

    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Big) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Big".into(),
                arguments: "{}".into(),
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
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>)
        .with_hard_tool_result_cap(0); // disabled

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "go".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    match &session.messages[2] {
        Turn::ToolResult { result, .. } => match result {
            ToolResultBody::Ok { content, truncated } => {
                assert!(!*truncated, "cap=0 must not truncate");
                assert_eq!(content.len(), 50_000, "whole body kept");
            }
            other => panic!("expected Ok, got {other:?}"),
        },
        other => panic!("expected a tool result, got {other:?}"),
    }
}
