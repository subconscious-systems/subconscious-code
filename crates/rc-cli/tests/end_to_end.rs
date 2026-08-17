//! End-to-end: the real `Read` tool through the agent loop — the M1 "Done when"
//! (`rc -p "what's in <file>"` reads the file and answers) — with a `MockModel`
//! and zero network.

use async_trait::async_trait;
use rc_core::tool::Tool;
use rc_core::{
    project, verify_invariant, AgentLoop, AllowAllChecker, EventSink, FinalizedToolCall,
    FinishReason, Model, ModelError, ModelRequest, ModelResponse, NullPrompter, NullSink,
    PermissionChecker, Session, ToolCall, ToolRegistry, Turn,
};
use rc_tools::Read;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

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
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted responses exhausted"))
    }
}

#[tokio::test]
async fn read_tool_runs_through_the_loop() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "the answer is 42\n").unwrap();
    let path = file.to_string_lossy().to_string();

    let registry = Arc::new(ToolRegistry::new(vec![
        Arc::new(Read::new()) as Arc<dyn Tool>
    ]));
    let responses = vec![
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![FinalizedToolCall::Call(ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: serde_json::json!({ "file_path": path }).to_string().into(),
            })],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        ModelResponse {
            text: "the file says the answer is 42".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(
        model,
        registry,
        Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>,
    );

    let mut session = Session::new("s".into(), dir.path().to_path_buf(), "mock".into());
    agent
        .run(
            &mut session,
            "what's in the file".into(),
            &NullSink,
            &NullPrompter,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // The tool-answer invariant holds on the final projection.
    assert!(verify_invariant(&project(&session.messages)).is_ok());

    // The real Read tool produced a result containing the file's line.
    let tool_text = session
        .messages
        .iter()
        .find_map(|t| match t {
            Turn::ToolResult { result, .. } => Some(result.render()),
            _ => None,
        })
        .expect("a tool result");
    assert!(
        tool_text.contains("the answer is 42"),
        "read result: {tool_text}"
    );

    // The model's final answer references it.
    let final_text = session
        .messages
        .iter()
        .rev()
        .find_map(|t| match t {
            Turn::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .expect("a final answer");
    assert!(final_text.contains("42"), "final answer: {final_text}");
}
