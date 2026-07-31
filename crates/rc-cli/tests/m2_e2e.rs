//! M2 end-to-end: the real `Read`→`Edit`→`Bash` tool trio through the agent
//! loop on a tiny fixture crate — the M2 "Done when" (`rc -p "add a --verbose
//! flag and make it compile"`), with a `MockModel` and zero network. `Bash`
//! actually invokes `rustc` to verify the edit compiles.

use async_trait::async_trait;
use rc_core::tool::Tool;
use rc_core::{
    AgentLoop, AllowAllChecker, EventSink, FinalizedToolCall, FinishReason, Model, ModelError,
    ModelRequest, ModelResponse, NullPrompter, NullSink, PermissionChecker, Session, ToolCall,
    ToolRegistry, Turn, project, verify_invariant,
};
use rc_tools::{Bash, Edit, Read};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

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
        Ok(self.responses.lock().unwrap().pop_front().expect("scripted responses exhausted"))
    }
}

fn call(id: &str, name: &str, args: serde_json::Value) -> FinalizedToolCall {
    FinalizedToolCall::Call(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string().into(),
    })
}

#[tokio::test]
async fn add_verbose_flag_and_compile() {
    let dir = tempdir().unwrap();
    let main_rs = dir.path().join("main.rs");
    std::fs::write(&main_rs, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    let path = main_rs.to_string_lossy().to_string();

    let new_string = "let verbose = std::env::args().any(|a| a == \"--verbose\");\n    if verbose {\n        println!(\"verbose hi\");\n    } else {\n        println!(\"hi\");\n    }";

    let registry = Arc::new(ToolRegistry::new(vec![
        Arc::new(Read::new()) as Arc<dyn Tool>,
        Arc::new(Edit::new()) as Arc<dyn Tool>,
        Arc::new(Bash::new()) as Arc<dyn Tool>,
    ]));
    let responses = vec![
        // 1) Read the file first (Edit requires a prior read).
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![call("c1", "Read", serde_json::json!({ "file_path": path }))],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        // 2) Edit in the --verbose handling.
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![call(
                "c2",
                "Edit",
                serde_json::json!({
                    "file_path": path,
                    "old_string": "println!(\"hi\");",
                    "new_string": new_string,
                }),
            )],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        // 3) Compile-check with rustc.
        ModelResponse {
            text: String::new(),
            reasoning: None,
            tool_calls: vec![call(
                "c3",
                "Bash",
                serde_json::json!({ "command": "rustc --edition 2021 main.rs -o out && echo COMPILE_OK" }),
            )],
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        // 4) Final answer.
        ModelResponse {
            text: "Added the --verbose flag; it compiles.".into(),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ];
    let model = Arc::new(MockModel::new(responses)) as Arc<dyn Model>;
    let agent = AgentLoop::new(model, registry, Arc::new(AllowAllChecker) as Arc<dyn PermissionChecker>);

    let mut session = Session::new("s".into(), dir.path().to_path_buf(), "mock".into());
    agent
        .run(&mut session, "add a --verbose flag and make it compile".into(), &NullSink, &NullPrompter, CancellationToken::new())
        .await
        .unwrap();

    // The tool-answer invariant holds across the whole multi-turn projection.
    assert!(verify_invariant(&project(&session.messages)).is_ok());

    // The edit landed.
    let on_disk = std::fs::read_to_string(&main_rs).unwrap();
    assert!(on_disk.contains("--verbose"), "{on_disk}");

    // The compile check ran and passed.
    let bash_text = session
        .messages
        .iter()
        .find_map(|t| match t {
            Turn::ToolResult { tool, result, .. } if tool == "Bash" => Some(result.render()),
            _ => None,
        })
        .expect("a Bash tool result");
    assert!(bash_text.contains("COMPILE_OK"), "bash result: {bash_text}");

    // Final answer references the work.
    let final_text = session
        .messages
        .iter()
        .rev()
        .find_map(|t| match t {
            Turn::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .expect("a final answer");
    assert!(final_text.contains("verbose"), "{final_text}");
}
