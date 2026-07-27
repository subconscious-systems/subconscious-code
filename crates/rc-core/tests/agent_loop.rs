//! Agent-loop test: a `MockModel` + a test tool, end-to-end with zero network.
//! (§13: "A MockModel that replays a scripted sequence of assistant messages
//! lets you test compaction, interrupts, and error paths deterministically.")

use async_trait::async_trait;
use rc_core::{
    agent::AgentLoop, model::EventSink, model::FinalizedToolCall, model::Model, model::ModelError,
    model::ModelRequest, model::ModelResponse, model::NullSink, project::project,
    project::verify_invariant, registry::ToolRegistry, tool::Concurrency, tool::Tool, tool::ToolCtx,
    tool::ToolError, tool::ToolOutcome, turn::Session, turn::ToolCall, turn::Turn,
};
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
    let agent = AgentLoop::new(model, registry);

    let mut session = Session::new("s1".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(&mut session, "do it".into(), &NullSink, CancellationToken::new())
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
    let agent = AgentLoop::new(model, registry);

    let mut session = Session::new("s2".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(&mut session, "x".into(), &NullSink, CancellationToken::new())
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
