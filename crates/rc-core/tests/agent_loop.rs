//! Agent-loop test: a `MockModel` + a test tool, end-to-end with zero network.
//! (§13: "A MockModel that replays a scripted sequence of assistant messages
//! lets you test compaction, interrupts, and error paths deterministically.")

use async_trait::async_trait;
use rc_core::{
    agent::AgentLoop, model::EventSink, model::FinalizedToolCall, model::Model, model::ModelError,
    model::ModelRequest, model::ModelResponse, model::NullSink, project::project,
    project::verify_invariant, registry::ToolRegistry, tool::Concurrency, tool::Tool,
    tool::ToolCtx, tool::ToolError, tool::ToolOutcome, turn::Session, turn::ToolCall, turn::Turn,
    AllowAllChecker, Mode, NullPrompter, PermissionChecker, PermissionEngine,
};
use rc_core::{AskResponse, FinishReason, Prompter, ToolResultBody, Usage};
use rc_proto::WireMessage;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;

struct FailingProgressModel;

#[async_trait]
impl Model for FailingProgressModel {
    async fn complete(
        &self,
        _req: ModelRequest,
        sink: &dyn EventSink,
    ) -> Result<ModelResponse, ModelError> {
        sink.on_response_headers(Duration::from_millis(7));
        sink.on_transport_activity();
        sink.on_reasoning("partial thought");
        sink.on_text("partial answer");
        sink.on_tool_delta(
            0,
            Some("call_partial"),
            Some("Write"),
            r#"{"file_path":"plan.md","content":"unfinished"#,
        );
        Err(ModelError::Proto {
            error: rc_proto::ProtoError::Idle(Duration::from_secs(120)),
            retries: 0,
        })
    }
}

#[tokio::test]
async fn failed_stream_persists_bounded_partial_diagnostics() {
    let agent = AgentLoop::new(
        Arc::new(FailingProgressModel),
        Arc::new(ToolRegistry::new(Vec::new())),
        Arc::new(AllowAllChecker),
    );
    let mut session = Session::new(
        "partial-failure".into(),
        std::env::temp_dir(),
        "mock".into(),
    );
    let result = agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    match session.messages.last().expect("persisted error") {
        Turn::Error {
            trace: Some(trace),
            partial: Some(partial),
            ..
        } => {
            assert_eq!(trace.transport_events, 1);
            assert_eq!(trace.semantic_events, 3);
            assert_eq!(trace.partial_text_chars, "partial answer".chars().count());
            assert_eq!(
                trace.partial_reasoning_chars,
                "partial thought".chars().count()
            );
            assert!(trace.last_transport_activity_ms.is_some());
            assert!(trace.last_semantic_activity_ms.is_some());
            assert_eq!(partial.text, "partial answer");
            assert_eq!(partial.reasoning, "partial thought");
            assert_eq!(partial.tool_calls.len(), 1);
            assert_eq!(partial.tool_calls[0].name.as_deref(), Some("Write"));
            assert!(partial.tool_calls[0].arguments.contains("unfinished"));
        }
        other => panic!("expected error diagnostics, got {other:?}"),
    }
}

/// A model that replays a scripted queue of responses.
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
        _sink: &dyn EventSink,
    ) -> Result<ModelResponse, ModelError> {
        let mut q = self.responses.lock().unwrap();
        Ok(q.pop_front().expect("scripted responses exhausted"))
    }
}

/// A test tool that echoes its `msg` argument.
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

#[tokio::test]
async fn loop_runs_tool_then_answers() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
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
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s1".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(
            &mut session,
            "do it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
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
async fn benchmark_completion_review_is_bounded_and_runs_after_tool_work() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"implemented"}"#.into(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "premature completion".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "reviewed completion".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let agent = AgentLoop::new(
        Arc::new(MockModel::new(responses)),
        registry,
        Arc::new(AllowAllChecker),
    )
    .with_completion_review(true);
    let mut session = Session::new(
        "benchmark-completion-review".into(),
        std::env::temp_dir(),
        "mock".into(),
    );

    let outcome = agent
        .run(
            &mut session,
            "implement the change".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::Stop);
    let completion_notes = session
        .messages
        .iter()
        .filter(|turn| {
            matches!(
                turn,
                Turn::SystemNote {
                    kind: rc_core::NoteKind::Recovery,
                    text,
                } if text.contains("completion audit")
            )
        })
        .count();
    assert_eq!(completion_notes, 1, "review must be requested exactly once");
    assert!(matches!(
        session.messages.last(),
        Some(Turn::Assistant { text, .. }) if text.as_ref() == "reviewed completion"
    ));
}

#[tokio::test]
async fn one_response_can_execute_multiple_tool_calls() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![
                FinalizedToolCall::Call(ToolCall {
                    id: "c1".into(),
                    name: "Echo".into(),
                    arguments: r#"{"msg":"first"}"#.into(),
                }),
                FinalizedToolCall::Call(ToolCall {
                    id: "c2".into(),
                    name: "Echo".into(),
                    arguments: r#"{"msg":"second"}"#.into(),
                }),
            ],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let sink = RecordingSink::default();
    let mut session = Session::new("multi".into(), std::env::temp_dir(), "mock".into());

    let outcome = agent
        .run(
            &mut session,
            "run both".into(),
            &sink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);
    assert_eq!(session.messages.len(), 5);
    let results: Vec<_> = session
        .messages
        .iter()
        .filter_map(|turn| match turn {
            Turn::ToolResult {
                call_id, result, ..
            } => Some((call_id.as_str(), result.render())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "c1");
    assert!(results[0].1.contains("echo: first"));
    assert_eq!(results[1].0, "c2");
    assert!(results[1].1.contains("echo: second"));
    assert_eq!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "tool_end")
            .count(),
        2
    );
    assert!(verify_invariant(&project(&session.messages)).is_ok());
}

#[tokio::test]
async fn repeated_investigation_rounds_receive_a_batching_nudge() {
    struct ResearchModel {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    #[async_trait]
    impl Model for ResearchModel {
        async fn complete(
            &self,
            req: ModelRequest,
            _sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            let round = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(req);
                requests.len()
            };
            if round <= 3 {
                Ok(ModelResponse {
                    retries: 0,
                    text: String::new(),
                    reasoning: None,
                    tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                        id: format!("research-{round}"),
                        name: "Echo".into(),
                        arguments: format!(r#"{{"msg":"round {round}"}}"#).into(),
                    })],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                })
            } else {
                Ok(ModelResponse {
                    retries: 0,
                    text: "done".into(),
                    reasoning: None,
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage: None,
                })
            }
        }
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = Arc::new(ResearchModel {
        requests: requests.clone(),
    }) as Arc<dyn Model>;
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let mut session = Session::new(
        "research-budget".into(),
        std::env::temp_dir(),
        "mock".into(),
    );

    agent
        .run(
            &mut session,
            "investigate".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in &requests[..3] {
        assert!(
            !request.messages.iter().any(|message| matches!(
                message,
                WireMessage::User {
                    content: rc_proto::wire::UserContent::Text(content)
                } if content.contains("agent guidance")
            )),
            "the budget should allow the first three rounds without a nudge"
        );
    }
    assert!(requests[3].messages.iter().any(|message| matches!(
        message,
        WireMessage::User {
            content: rc_proto::wire::UserContent::Text(content)
        } if content.contains("fetch all independent paths and queries together")
    )));
}

#[tokio::test]
async fn loop_feeds_back_a_parse_error_as_a_tool_result() {
    // The model emits a tool call with arguments that won't parse. The loop must
    // synthesize a `role:tool` error result (§3.3) so the model can retry.
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
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
            retries: 0,
            text: "recovered".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s2".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "x".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // The parse-error tool result must answer the (synthesized) call id c1.
    let wire = project(&session.messages);
    assert!(
        verify_invariant(&wire).is_ok(),
        "parse-error result must satisfy the invariant"
    );
    let WireMessage::Assistant { tool_calls, .. } = &wire[2] else {
        panic!("expected the parse-error assistant call")
    };
    let arguments = &tool_calls[0].function.arguments;
    assert!(
        matches!(
            serde_json::from_str::<serde_json::Value>(arguments),
            Ok(serde_json::Value::Object(_))
        ),
        "replayed tool arguments must be a valid JSON object: {arguments}"
    );
    assert!(arguments.contains("_sc_invalid_tool_arguments"));

    match &session.messages[2] {
        Turn::ToolResult { result, .. } => {
            assert!(result.render().contains("invalid tool arguments"))
        }
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
            retries: 0,
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
            retries: 0,
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
        .run(
            &mut session,
            "edit it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(verify_invariant(&project(&session.messages)).is_ok());
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => {
            assert!(
                result.render().contains("denied"),
                "expected a denied result, got {:?}",
                result.render()
            )
        }
        other => panic!("expected a denied tool result, got {other:?}"),
    }
}

// ---- loop event surface (M4) ------------------------------------------------

/// An `EventSink` that records the order of sink calls for assertion.
#[derive(Default)]
struct RecordingSink(Mutex<Vec<String>>);
impl EventSink for RecordingSink {
    fn on_text(&self, _d: &str) {
        self.0.lock().unwrap().push("text".into());
    }
    fn on_reasoning(&self, _d: &str) {
        self.0.lock().unwrap().push("reasoning".into());
    }
    fn on_tool_start(&self, _c: &ToolCall) {
        self.0.lock().unwrap().push("tool_start".into());
    }
    fn on_tool_end(&self, _id: &str, _t: &str, _r: &ToolResultBody) {
        self.0.lock().unwrap().push("tool_end".into());
    }
    fn on_iter(&self, c: u32, _m: u32) {
        self.0.lock().unwrap().push(format!("iter{c}"));
    }
    fn on_usage(&self, _u: &Usage) {
        self.0.lock().unwrap().push("usage".into());
    }
    fn on_finish(&self, _r: &FinishReason) {
        self.0.lock().unwrap().push("finish".into());
    }
}

#[tokio::test]
async fn loop_emits_iter_tool_end_and_usage() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
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
            retries: 0,
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
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let sink = RecordingSink::default();
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "do it".into(),
            &sink,
            &NullPrompter,
            CancellationToken::new(),
        )
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
        retries: 0,
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
        retries: 0,
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
    let model =
        Arc::new(MockModel::new(vec![edit_call("c1"), stop_response("ok")])) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, engine);
    let prompter = MockPrompter {
        response: AskResponse::Once,
    };
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "edit it".into(),
            &NullSink,
            &prompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        session.perm_grants.is_empty(),
        "Once adds no grant: {:?}",
        session.perm_grants
    );
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => {
            assert!(
                matches!(result, ToolResultBody::Error { .. }),
                "allowed -> unknown tool: {:?}",
                result
            );
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn async_prompter_session_adds_a_grant() {
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let engine = Arc::new(PermissionEngine::new(Mode::Default, vec![], vec![], vec![]))
        as Arc<dyn PermissionChecker>;
    let model =
        Arc::new(MockModel::new(vec![edit_call("c1"), stop_response("ok")])) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, engine);
    let prompter = MockPrompter {
        response: AskResponse::Session("Edit".into()),
    };
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "edit it".into(),
            &NullSink,
            &prompter,
            CancellationToken::new(),
        )
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
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            let n = {
                let mut c = self.n.lock().unwrap();
                *c += 1;
                *c
            };
            tokio::time::sleep(self.delay).await;
            Ok(ModelResponse {
                retries: 0,
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
    let model = Arc::new(Slow {
        delay: Duration::from_millis(20),
        n: calls.clone(),
    }) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    )
    .with_turn_timeout(Some(Duration::from_millis(50)));

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let start = SystemTime::now();
    let outcome = agent
        .run(
            &mut session,
            "loop".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let elapsed = start.elapsed().unwrap_or_default();
    let n = *calls.lock().unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::TimeUp);
    assert!(
        elapsed < Duration::from_secs(2),
        "should stop near 50 ms, took {elapsed:?}"
    );
    assert!(
        (2..50).contains(&n),
        "should run a few iters ({n}), not to the limit"
    );
}

#[tokio::test]
async fn turn_timeout_interrupts_an_in_flight_model_request() {
    struct BlockedModel;
    #[async_trait]
    impl Model for BlockedModel {
        async fn complete(
            &self,
            _req: ModelRequest,
            _sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            unreachable!("the turn deadline must drop this request")
        }
    }

    let agent = AgentLoop::new(
        Arc::new(BlockedModel),
        Arc::new(ToolRegistry::new(vec![])),
        Arc::new(AllowAllChecker),
    )
    .with_turn_timeout(Some(Duration::from_millis(40)));
    let mut session = Session::new("hard-deadline".into(), std::env::temp_dir(), "mock".into());
    let started = tokio::time::Instant::now();
    let outcome = agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::TimeUp);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "deadline did not interrupt the active request"
    );
}

#[tokio::test]
async fn turn_timeout_preserves_completed_tool_results_and_interrupts_the_rest() {
    struct TimedTool {
        name: &'static str,
        delay: Duration,
    }
    #[async_trait]
    impl Tool for TimedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "A serial timeout test tool."
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn concurrency(&self) -> Concurrency {
            Concurrency::SerialWrite
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            tokio::time::sleep(self.delay).await;
            Ok(ToolOutcome::ok(format!("{} complete", self.name)))
        }
    }
    #[derive(Default)]
    struct EndSink(Mutex<Vec<String>>);
    impl EventSink for EndSink {
        fn on_tool_end(&self, _call_id: &str, tool: &str, _result: &ToolResultBody) {
            self.0.lock().unwrap().push(tool.to_string());
        }
    }

    let tools = Arc::new(ToolRegistry::new(vec![
        Arc::new(TimedTool {
            name: "FastWrite",
            delay: Duration::ZERO,
        }),
        Arc::new(TimedTool {
            name: "BlockedWrite",
            delay: Duration::from_secs(30),
        }),
    ]));
    let calls = ["FastWrite", "BlockedWrite"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            FinalizedToolCall::Call(ToolCall {
                id: format!("call-{index}"),
                name: name.into(),
                arguments: "{}".into(),
            })
        })
        .collect();
    let model = Arc::new(MockModel::new(vec![ModelResponse {
        retries: 0,
        text: String::new(),
        reasoning: None,
        tool_calls: calls,
        finish_reason: rc_proto::FinishReason::ToolCalls,
        usage: None,
    }]));
    let agent = AgentLoop::new(model, tools, Arc::new(AllowAllChecker))
        .with_turn_timeout(Some(Duration::from_millis(50)));
    let sink = EndSink::default();
    let mut session = Session::new("partial-tools".into(), std::env::temp_dir(), "mock".into());

    let outcome = agent
        .run(
            &mut session,
            "go".into(),
            &sink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::TimeUp);
    assert!(matches!(
        &session.messages[2],
        Turn::ToolResult {
            tool,
            result: ToolResultBody::Ok { .. },
            ..
        } if tool == "FastWrite"
    ));
    assert!(matches!(
        &session.messages[3],
        Turn::ToolResult {
            tool,
            result: ToolResultBody::Interrupted,
            ..
        } if tool == "BlockedWrite"
    ));
    assert_eq!(
        *sink.0.lock().unwrap(),
        vec!["FastWrite", "BlockedWrite"],
        "every tool gets exactly one terminal event"
    );
    assert!(verify_invariant(&project(&session.messages)).is_ok());
}

// ---- M3: cross-turn usage accumulation ------------------------------------

#[tokio::test]
async fn accumulates_usage_across_iterations() {
    fn usage(p: u64, c: u64) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: p + c,
            prompt_tokens_details: None,
        }
    }
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
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
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: Some(usage(20, 3)),
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let t = &session.total_usage;
    assert_eq!(t.prompt_tokens, 30, "prompt summed across iterations");
    assert_eq!(
        t.completion_tokens, 5,
        "completion summed across iterations"
    );
    assert_eq!(t.total_tokens, 35, "total summed across iterations");
}

// ---- unconfirmed tool-call recovery (§4.2 / project.rs:69) -----------------
//
// A provider can emit a tool call without the corresponding `ToolCalls` finish
// reason. The loop must not execute that unconfirmed call, but it also must not
// abandon the user's turn. It pairs the call with a retryable error and asks the
// model again; a confirmed reissue can then execute normally.

#[tokio::test]
async fn stop_with_outstanding_call_recovers_in_the_same_turn() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "unconfirmed".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
            })],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "confirmed".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(
            &mut session,
            "do it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);

    // User, unconfirmed Assistant + retry error, confirmed Assistant + real
    // result, final Assistant. The unconfirmed call was never executed.
    assert_eq!(session.messages.len(), 6);
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => match result {
            ToolResultBody::Error { message, retryable } => {
                assert!(*retryable);
                assert!(message.contains("not executed"));
                assert!(message.contains("Retry"));
            }
            other => panic!("expected a retryable error, got {other:?}"),
        },
        other => panic!("expected a synthesized tool result, got {other:?}"),
    }
    match &session.messages[4] {
        Turn::ToolResult { result, .. } => assert!(result.render().contains("echo: hi")),
        other => panic!("expected the confirmed tool result, got {other:?}"),
    }
    assert!(
        verify_invariant(&project(&session.messages)).is_ok(),
        "invariant must hold"
    );
}

#[tokio::test]
async fn length_with_outstanding_call_recovers_in_the_same_turn() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: "partial".into(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "unconfirmed".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
            })],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "confirmed".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(
            &mut session,
            "do it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);
    assert!(
        verify_invariant(&project(&session.messages)).is_ok(),
        "recovery must preserve the invariant"
    );
    assert!(session.messages.iter().any(|t| matches!(
        t,
        Turn::ToolResult {
            result: ToolResultBody::Error {
                retryable: true,
                ..
            },
            ..
        }
    )));
    assert!(session.messages.iter().any(|t| matches!(
        t,
        Turn::ToolResult {
            result: ToolResultBody::Ok { .. },
            ..
        }
    )));
}

#[tokio::test]
async fn repeated_unconfirmed_tool_call_stops_after_one_reissue() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let response = |id: &str| ModelResponse {
        retries: 0,
        text: String::new(),
        reasoning: None,
        tool_calls: vec![FinalizedToolCall::Call(ToolCall {
            id: id.into(),
            name: "Echo".into(),
            arguments: r#"{"msg":"unsafe until confirmed"}"#.into(),
        })],
        finish_reason: rc_proto::FinishReason::Length,
        usage: None,
    };
    let model =
        Arc::new(MockModel::new(vec![response("first"), response("second")])) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let mut session = Session::new(
        "bounded-unconfirmed".into(),
        std::env::temp_dir(),
        "mock".into(),
    );

    let outcome = agent
        .run(
            &mut session,
            "do it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::Incomplete);
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|turn| matches!(turn, Turn::ToolResult { .. }))
            .count(),
        2
    );
    assert!(session.messages.iter().all(|turn| !matches!(
        turn,
        Turn::ToolResult {
            result: ToolResultBody::Ok { .. },
            ..
        }
    )));
    assert!(verify_invariant(&project(&session.messages)).is_ok());
}

// ---- A13: bounded recovery on finish_reason=length -------------------------

#[tokio::test]
async fn visible_length_response_gets_a_synthetic_recovery_note() {
    // The model returns finish_reason=Length with text but no tool calls. The
    // loop may request one continuation, but must record it as harness control
    // state rather than attributing a fake `continue` message to the user.
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: "partial answer".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: " done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(
            &mut session,
            "explain".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);

    // User, Assistant(partial), SystemNote(recovery), Assistant(done).
    assert_eq!(session.messages.len(), 4);
    match &session.messages[2] {
        Turn::SystemNote { kind, text } => {
            assert!(matches!(kind, rc_core::NoteKind::Recovery));
            assert!(text.contains("visible partial response"));
        }
        other => panic!("expected a synthetic recovery note, got {other:?}"),
    }
    match &session.messages[3] {
        Turn::Assistant { text, .. } => assert_eq!(text.as_ref(), " done"),
        other => panic!("expected the continued answer, got {other:?}"),
    }
    assert!(
        verify_invariant(&project(&session.messages)).is_ok(),
        "invariant must hold"
    );
}

#[tokio::test]
async fn repeated_visible_length_stops_after_one_continuation() {
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: "first partial".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "second partial".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
    ];
    let agent = AgentLoop::new(
        Arc::new(MockModel::new(responses)),
        registry,
        Arc::new(AllowAllChecker),
    );
    let mut session = Session::new(
        "bounded-visible-length".into(),
        std::env::temp_dir(),
        "mock".into(),
    );

    let outcome = agent
        .run(
            &mut session,
            "explain".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::Length);
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|turn| matches!(turn, Turn::SystemNote { .. }))
            .count(),
        1
    );
    assert!(!session
        .messages
        .iter()
        .any(|turn| matches!(turn, Turn::User { content, .. } if content.as_ref() == "continue")));
}

#[tokio::test]
async fn empty_reasoning_response_gets_one_force_action_recovery() {
    // The GLM route has been observed returning `stop` at its implicit 4096
    // completion-token ceiling after producing reasoning but no answer. Treat
    // that shape as length exhaustion, replay its reasoning privately, and
    // issue one force-action recovery without impersonating the user.
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: Some("reasoning that consumed the whole allowance".into()),
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 4096,
                total_tokens: 4196,
                prompt_tokens_details: None,
            }),
        },
        ModelResponse {
            retries: 0,
            text: "recovered answer".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let mut session = Session::new(
        "implicit-length".into(),
        std::env::temp_dir(),
        "mock".into(),
    );

    let outcome = agent
        .run(
            &mut session,
            "do the work".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);
    assert!(matches!(
        &session.messages[2],
        Turn::SystemNote { kind: rc_core::NoteKind::Recovery, text }
            if text.contains("Take the next observable action now")
    ));
    let Turn::Assistant {
        trace: Some(trace), ..
    } = &session.messages[1]
    else {
        panic!("first assistant turn should carry request trace metadata")
    };
    assert!(trace.implicit_length);
    assert_eq!(trace.reported_finish_reason, "stop");
    assert_eq!(trace.effective_finish_reason, "length");
    let wire = project(&session.messages[..3]);
    assert!(matches!(
        &wire[2],
        WireMessage::Assistant {
            content: None,
            reasoning_content: Some(reasoning),
            ..
        } if reasoning.as_ref() == "reasoning that consumed the whole allowance"
    ));
    assert!(matches!(
        &session.messages[3],
        Turn::Assistant { text, .. } if text.as_ref() == "recovered answer"
    ));
}

#[tokio::test]
async fn repeated_reasoning_only_length_stops_as_no_progress() {
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: Some("first private attempt".into()),
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: Some("second private attempt".into()),
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let mut session = Session::new("no-progress".into(), std::env::temp_dir(), "mock".into());

    let outcome = agent
        .run(
            &mut session,
            "do the work".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::agent::LoopOutcome::NoProgress);
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|turn| matches!(
                turn,
                Turn::SystemNote {
                    kind: rc_core::NoteKind::Recovery,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(!session
        .messages
        .iter()
        .any(|turn| matches!(turn, Turn::User { content, .. } if content.as_ref() == "continue")));
}

#[tokio::test]
async fn force_action_recovery_is_used_only_once_even_after_tool_progress() {
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: Some("first hidden-only attempt".into()),
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "progress".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"visible action"}"#.into(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: Some("another hidden-only attempt".into()),
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Length,
            usage: None,
        },
    ];
    let agent = AgentLoop::new(
        Arc::new(MockModel::new(responses)),
        registry,
        Arc::new(AllowAllChecker),
    );
    let mut session = Session::new("one-recovery".into(), std::env::temp_dir(), "mock".into());

    let outcome = agent
        .run(
            &mut session,
            "do it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::LoopOutcome::NoProgress);
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|turn| matches!(
                turn,
                Turn::SystemNote {
                    kind: rc_core::NoteKind::Recovery,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn terminal_stream_end_is_incomplete_not_stop() {
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let model = Arc::new(MockModel::new(vec![ModelResponse {
        retries: 0,
        text: String::new(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: rc_proto::FinishReason::Other("stream-ended".into()),
        usage: None,
    }])) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let mut session = Session::new("stream-ended".into(), std::env::temp_dir(), "mock".into());

    let outcome = agent
        .run(
            &mut session,
            "do the work".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome, rc_core::agent::LoopOutcome::Incomplete);
}

#[tokio::test]
async fn successful_model_request_persists_diagnostic_trace_fields() {
    struct InstrumentedModel;
    #[async_trait]
    impl Model for InstrumentedModel {
        async fn complete(
            &self,
            _req: ModelRequest,
            sink: &dyn EventSink,
        ) -> Result<ModelResponse, ModelError> {
            sink.on_request_payload(10_000, 1_500);
            sink.on_response_headers(Duration::from_millis(12));
            sink.on_retry(2);
            sink.on_reasoning("first output");
            Ok(ModelResponse {
                retries: 2,
                text: "done".into(),
                reasoning: Some("first output".into()),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: None,
            })
        }
    }

    let agent = AgentLoop::new(
        Arc::new(InstrumentedModel) as Arc<dyn Model>,
        Arc::new(ToolRegistry::new(vec![])),
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );
    let mut session = Session::new("trace".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let Turn::Assistant {
        trace: Some(trace), ..
    } = &session.messages[1]
    else {
        panic!("assistant response must persist request metrics")
    };
    assert_eq!(trace.request_bytes, 10_000);
    assert_eq!(trace.wire_bytes, 1_500);
    assert_eq!(trace.response_headers_ms, Some(12));
    assert!(trace.ttft_ms.is_some());
    assert_eq!(trace.retries, 2);
    assert_eq!(trace.reported_finish_reason, "stop");
    assert_eq!(trace.effective_finish_reason, "stop");
    assert!(trace.completed_ms >= trace.started_ms);
    assert!(trace.context_chars > 0);
}

#[tokio::test]
async fn stream_end_with_outstanding_call_recovers_without_executing_it() {
    // A missing/unrecognized provider finish marker follows the same safe retry
    // path. Only the subsequent ToolCalls-confirmed reissue may execute.
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Echo) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: "partial".into(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "unconfirmed".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
            })],
            finish_reason: rc_proto::FinishReason::Other("stream-ended".into()),
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "confirmed".into(),
                name: "Echo".into(),
                arguments: r#"{"msg":"hi"}"#.to_string().into(),
            })],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    let outcome = agent
        .run(
            &mut session,
            "do it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);
    assert!(
        verify_invariant(&project(&session.messages)).is_ok(),
        "stream-end recovery must preserve the invariant"
    );
    let tool_results: Vec<_> = session
        .messages
        .iter()
        .filter_map(|turn| match turn {
            Turn::ToolResult { result, .. } => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
    assert!(matches!(
        tool_results[0],
        ToolResultBody::Error {
            retryable: true,
            ..
        }
    ));
    assert!(matches!(tool_results[1], ToolResultBody::Ok { .. }));
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
    async fn complete(
        &self,
        req: ModelRequest,
        _sink: &dyn EventSink,
    ) -> Result<ModelResponse, ModelError> {
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
        retries: 0,
        text: "done".into(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: rc_proto::FinishReason::Stop,
        usage: None,
    };
    let model = Arc::new(CapturingModel {
        captured: captured.clone(),
        response,
    }) as Arc<dyn Model>;
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    )
    .with_assembler(Arc::new(FixedAssembler {
        prompt: "SENTINEL SYSTEM PROMPT".into(),
    }) as Arc<dyn rc_core::ContextAssembler>);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "hi".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let req = captured
        .lock()
        .unwrap()
        .clone()
        .expect("a request was captured");
    assert_eq!(
        req.opts.session_id.as_deref(),
        Some("s"),
        "every model request must carry the active session identity"
    );
    use rc_proto::WireMessage;
    match req.messages.first() {
        Some(WireMessage::System { content }) => {
            assert_eq!(
                content.as_ref(),
                "SENTINEL SYSTEM PROMPT",
                "loop must use the wired assembler"
            );
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
        retries: 0,
        text: "done".into(),
        reasoning: None,
        tool_calls: vec![],
        finish_reason: rc_proto::FinishReason::Stop,
        usage: None,
    };
    let model = Arc::new(CapturingModel {
        captured: captured.clone(),
        response,
    }) as Arc<dyn Model>;
    let registry = Arc::new(ToolRegistry::new(vec![]));
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "hi".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let req = captured
        .lock()
        .unwrap()
        .clone()
        .expect("a request was captured");
    use rc_proto::WireMessage;
    match req.messages.first() {
        Some(WireMessage::System { content }) => {
            assert!(
                content.contains("You are `sc`"),
                "default prompt: {content}"
            );
            assert!(
                !content.contains("SENTINEL"),
                "no custom prompt without an assembler"
            );
        }
        other => panic!("expected a system message, got {other:?}"),
    }
}

// ---- F9: a panicking parallel tool is isolated, not propagated --------------

/// A parallel tool that panics inside `call` — a stand-in for a tool-impl bug.
struct PanicTool;
#[async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &str {
        "PanicTool"
    }
    fn description(&self) -> &str {
        "Panics on purpose."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Parallel
    }
    async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        panic!("PanicTool: simulated tool-impl bug");
    }
}

#[tokio::test]
async fn a_panicking_parallel_tool_becomes_an_error_not_a_crash() {
    // A tool whose `call` panics must not take down the agent loop. The loop
    // surfaces a non-retryable error result for that call and continues to a
    // normal Stop — the invariant holds and no panic escapes `run`.
    let registry = Arc::new(ToolRegistry::new(
        vec![Arc::new(PanicTool) as Arc<dyn Tool>],
    ));
    let responses = vec![
        ModelResponse {
            retries: 0,
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
            retries: 0,
            text: "recovered".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    // The run must NOT panic (the tool's panic is isolated by tokio).
    let outcome = agent
        .run(
            &mut session,
            "do it".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome, rc_core::agent::LoopOutcome::Stop);

    // The panicked call becomes a non-retryable error result (not a propagated
    // panic), and the invariant still holds.
    match &session.messages[2] {
        Turn::ToolResult { result, .. } => match result {
            ToolResultBody::Error { message, retryable } => {
                assert!(
                    message.contains("panic"),
                    "expected a panic error, got {message}"
                );
                assert!(!*retryable, "a panic is not retryable");
            }
            other => panic!("expected an Error result, got {other:?}"),
        },
        other => panic!("expected a tool result, got {other:?}"),
    }
    assert!(
        verify_invariant(&project(&session.messages)).is_ok(),
        "invariant must hold"
    );
}

// ---- explicitly unlimited context (M8) --------------------------------------

/// Disabling the hard backstop explicitly still lets a large tool result reach
/// the wire whole.
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
                    retries: 0,
                    text: String::new(),
                    reasoning: None,
                    tool_calls: vec![rc_core::model::FinalizedToolCall::Call(rc_core::ToolCall {
                        id: "c1".into(),
                        name: "Fat".into(),
                        arguments: "{}".into(),
                    })],
                    finish_reason: rc_proto::FinishReason::ToolCalls,
                    usage: None,
                }
            } else {
                ModelResponse {
                    retries: 0,
                    text: "done".into(),
                    reasoning: None,
                    tool_calls: vec![],
                    finish_reason: rc_proto::FinishReason::Stop,
                    usage: None,
                }
            })
        }
    }

    let model = Arc::new(TwoTurn {
        requests: requests.clone(),
    }) as Arc<dyn Model>;
    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Fat) as Arc<dyn Tool>]));
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    )
    .with_hard_tool_result_cap(0);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
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
    assert!(
        !tool_msg.contains("truncated"),
        "no truncation sentinel: {}",
        &tool_msg[..80]
    );
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
        fn name(&self) -> &str {
            "Huge"
        }
        fn description(&self) -> &str {
            "Returns a runaway body."
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            // 10 KB — over the 4 KB test cap, under the 1 MiB default.
            Ok(ToolOutcome::ok("z".repeat(10_000)))
        }
    }

    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Huge) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
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
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    )
    .with_hard_tool_result_cap(4_000);

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
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
        fn name(&self) -> &str {
            "Big"
        }
        fn description(&self) -> &str {
            "Returns a big body."
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome::ok("y".repeat(50_000)))
        }
    }

    let registry = Arc::new(ToolRegistry::new(vec![Arc::new(Big) as Arc<dyn Tool>]));
    let responses = vec![
        ModelResponse {
            retries: 0,
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
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    )
    .with_hard_tool_result_cap(0); // disabled

    let mut session = Session::new("s".into(), std::env::temp_dir(), "mock".into());
    agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
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

#[tokio::test]
async fn mixed_tool_batch_preserves_model_effect_order() {
    struct OrderedTool {
        name: &'static str,
        concurrency: Concurrency,
        observed: Arc<Mutex<Vec<&'static str>>>,
    }
    #[async_trait]
    impl Tool for OrderedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Records its execution order."
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn concurrency(&self) -> Concurrency {
            self.concurrency
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            self.observed.lock().unwrap().push(self.name);
            Ok(ToolOutcome::ok(self.name.to_string()))
        }
    }

    let observed = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(ToolRegistry::new(vec![
        Arc::new(OrderedTool {
            name: "WriteState",
            concurrency: Concurrency::SerialWrite,
            observed: observed.clone(),
        }),
        Arc::new(OrderedTool {
            name: "ReadState",
            concurrency: Concurrency::Parallel,
            observed: observed.clone(),
        }),
    ]));
    let calls = ["WriteState", "ReadState", "WriteState", "ReadState"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            FinalizedToolCall::Call(ToolCall {
                id: format!("call-{index}"),
                name: name.into(),
                arguments: "{}".into(),
            })
        })
        .collect();
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: calls,
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let agent = AgentLoop::new(
        Arc::new(MockModel::new(responses)),
        registry,
        Arc::new(AllowAllChecker),
    );
    let mut session = Session::new("ordered-tools".into(), std::env::temp_dir(), "mock".into());

    agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        *observed.lock().unwrap(),
        vec!["WriteState", "ReadState", "WriteState", "ReadState"]
    );
}

#[tokio::test]
async fn permission_check_uses_cwd_from_an_earlier_batch_barrier() {
    struct ChangeDirectory;
    #[async_trait]
    impl Tool for ChangeDirectory {
        fn name(&self) -> &str {
            "ChangeDirectory"
        }
        fn description(&self) -> &str {
            "Changes the shared cwd for the test."
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn concurrency(&self) -> Concurrency {
            Concurrency::Exclusive
        }
        async fn call(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            let next = ctx.cwd.join("subdir");
            ctx.shell_state.lock().unwrap().cwd = next;
            Ok(ToolOutcome::ok("changed cwd".into()))
        }
    }
    struct Mutate;
    #[async_trait]
    impl Tool for Mutate {
        fn name(&self) -> &str {
            "Mutate"
        }
        fn description(&self) -> &str {
            "A relative mutation."
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn concurrency(&self) -> Concurrency {
            Concurrency::SerialWrite
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome::ok("mutated".into()))
        }
    }
    struct CwdChecker(Arc<Mutex<Vec<(String, std::path::PathBuf)>>>);
    impl PermissionChecker for CwdChecker {
        fn check(
            &self,
            tool: &str,
            _input: &Value,
            cwd: &std::path::Path,
            _roots: &[std::path::PathBuf],
            _grants: &[String],
        ) -> rc_perm::Decision {
            self.0
                .lock()
                .unwrap()
                .push((tool.to_string(), cwd.to_path_buf()));
            rc_perm::Decision::Allow
        }
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let tools = Arc::new(ToolRegistry::new(vec![
        Arc::new(ChangeDirectory),
        Arc::new(Mutate),
    ]));
    let responses = vec![
        ModelResponse {
            retries: 0,
            text: String::new(),
            reasoning: None,
            tool_calls: vec![
                FinalizedToolCall::Call(ToolCall {
                    id: "cd".into(),
                    name: "ChangeDirectory".into(),
                    arguments: "{}".into(),
                }),
                FinalizedToolCall::Call(ToolCall {
                    id: "edit".into(),
                    name: "Mutate".into(),
                    arguments: "{}".into(),
                }),
            ],
            finish_reason: rc_proto::FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            retries: 0,
            text: "done".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: rc_proto::FinishReason::Stop,
            usage: None,
        },
    ];
    let agent = AgentLoop::new(
        Arc::new(MockModel::new(responses)),
        tools,
        Arc::new(CwdChecker(observed.clone())),
    );
    let mut session = Session::new("live-cwd".into(), dir.path().to_path_buf(), "mock".into());
    agent
        .run(
            &mut session,
            "go".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let checks = observed.lock().unwrap();
    assert_eq!(checks[0].1, dir.path());
    assert_eq!(checks[1].1, dir.path().join("subdir"));
}
