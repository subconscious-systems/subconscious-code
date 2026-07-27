//! The agent loop (§4.2): streaming + tool calling with concurrency classes
//! (§4.3), the iteration budget, and the tool-answer invariant.

use crate::model::{EventSink, FinalizedToolCall, Model, ModelError, ModelRequest, ModelResponse};
use crate::project::{project, verify_invariant};
use crate::registry::ToolRegistry;
use crate::tool::{Concurrency, Tool, ToolCtx, ToolOutcome};
use crate::turn::{Session, ToolCall, ToolResultBody, Turn};
use rc_proto::{FinishReason, ToolChoiceValue};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(thiserror::Error, Debug)]
pub enum LoopError {
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("tool-answer invariant violated: {0}")]
    Invariant(String),
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopOutcome {
    /// `finish_reason: stop`.
    Stop,
    /// `finish_reason: length` (M1 stops; auto-continue is A13/P2).
    Length,
    /// The iteration budget was reached (§4.2, default 100).
    ItersExceeded,
}

const MAX_ITERS: u32 = 100;
const PARALLEL_BOUND: usize = 8;

/// The agent loop. Headless in M1; the TUI (M4) and cancellation-via-Esc plug in
/// through the [`EventSink`] and a per-turn [`CancellationToken`].
pub struct AgentLoop {
    pub model: Arc<dyn Model>,
    pub tools: Arc<ToolRegistry>,
    pub max_iters: u32,
}

impl AgentLoop {
    pub fn new(model: Arc<dyn Model>, tools: Arc<ToolRegistry>) -> Self {
        Self { model, tools, max_iters: MAX_ITERS }
    }

    /// Run a full turn for `user_input`. Mutates `session` (pushes turns).
    /// Returns when the model stops, hits `length`, or the iteration budget.
    pub async fn run(
        &self,
        session: &mut Session,
        user_input: String,
        _sink: &dyn EventSink,
        cancel: CancellationToken,
    ) -> Result<LoopOutcome, LoopError> {
        session.messages.push(Turn::User { content: user_input, ts: SystemTime::now() });

        let ctx = ToolCtx {
            cwd: session.cwd.clone(),
            allowed_roots: allowed_roots(session),
            cancel,
            read_registry: session.read_registry.clone(),
        };

        let mut iters = 0;
        loop {
            iters += 1;
            if iters > self.max_iters {
                return Ok(LoopOutcome::ItersExceeded);
            }

            let messages = project(&session.messages);
            // Safety check (§4.2). In debug this panics on a violation (a bug in
            // the loop); in release it logs so a malformed request is still sent.
            let inv = verify_invariant(&messages);
            debug_assert!(inv.is_ok(), "tool-answer invariant: {:?}", inv);
            if let Err(e) = inv {
                tracing::error!("tool-answer invariant violated: {e}");
            }

            let req = ModelRequest {
                messages,
                tools: self.tools.definitions().to_vec(),
                tool_choice: ToolChoiceValue::Auto,
                opts: Default::default(),
            };
            let ModelResponse { text, reasoning, tool_calls, finish_reason, usage } =
                self.model.complete(req, _sink).await?;

            // Build the assistant turn's domain calls (for projection) and the
            // execution list (for running), preserving original order.
            let mut assistant_calls = Vec::new();
            let mut exec_list: Vec<ExecItem> = Vec::new();
            for fc in tool_calls {
                match fc {
                    FinalizedToolCall::Call(c) => {
                        assistant_calls.push(c.clone());
                        exec_list.push(ExecItem::Call(c));
                    }
                    FinalizedToolCall::ParseError { id, name, raw, error } => {
                        let call_id = id
                            .clone()
                            .unwrap_or_else(|| format!("parseerr_{}", assistant_calls.len()));
                        let tool_name = name.unwrap_or_default();
                        assistant_calls.push(ToolCall {
                            id: call_id.clone(),
                            name: tool_name.clone(),
                            arguments: raw,
                        });
                        exec_list.push(ExecItem::ParseError { call_id, tool_name, error });
                    }
                }
            }

            session.messages.push(Turn::Assistant { text, reasoning, calls: assistant_calls, usage });

            match finish_reason {
                FinishReason::Stop => return Ok(LoopOutcome::Stop),
                FinishReason::Length => {
                    tracing::warn!("finish_reason=length; stopping (auto-continue is A13)");
                    return Ok(LoopOutcome::Length);
                }
                FinishReason::ToolCalls => {} // execute and continue
                FinishReason::ContentFilter | FinishReason::Other(_) => {
                    return Ok(LoopOutcome::Stop);
                }
            }

            let results = execute_batch(&exec_list, &self.tools, &ctx).await;
            for (call_id, tool, result, duration) in results {
                session
                    .messages
                    .push(Turn::ToolResult { call_id, tool, result, duration });
            }
        }
    }
}

fn allowed_roots(session: &Session) -> Vec<std::path::PathBuf> {
    let mut roots = vec![session.cwd.clone()];
    roots.extend(session.extra_dirs.iter().cloned());
    roots
}

enum ExecItem {
    Call(ToolCall),
    ParseError { call_id: String, tool_name: String, error: String },
}

/// Execute a batch of tool calls per the concurrency policy (§4.3): Parallel
/// run concurrently (bounded by [`PARALLEL_BOUND`]); SerialWrite sequentially
/// in model order; Exclusive one at a time after everything else. Results are
/// returned in the original tool-call order regardless of execution order.
async fn execute_batch(
    items: &[ExecItem],
    tools: &ToolRegistry,
    ctx: &ToolCtx,
) -> Vec<(String, String, ToolResultBody, Duration)> {
    let mut results: HashMap<usize, (String, String, ToolResultBody, Duration)> = HashMap::new();
    let mut order: Vec<usize> = Vec::new();
    let mut parallel: Vec<(usize, Arc<dyn Tool>, ToolCall)> = Vec::new();
    let mut serial: Vec<(usize, Arc<dyn Tool>, ToolCall)> = Vec::new();
    let mut exclusive: Vec<(usize, Arc<dyn Tool>, ToolCall)> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        order.push(i);
        match item {
            ExecItem::Call(call) => match tools.get(&call.name) {
                Some(tool) => match tool.concurrency() {
                    Concurrency::Parallel => parallel.push((i, tool.clone(), call.clone())),
                    Concurrency::SerialWrite => serial.push((i, tool.clone(), call.clone())),
                    Concurrency::Exclusive => exclusive.push((i, tool.clone(), call.clone())),
                },
                None => {
                    results.insert(
                        i,
                        (
                            call.id.clone(),
                            call.name.clone(),
                            ToolResultBody::Error {
                                message: format!("unknown tool: {}", call.name),
                                retryable: false,
                            },
                            Duration::ZERO,
                        ),
                    );
                }
            },
            ExecItem::ParseError { call_id, tool_name, error } => {
                results.insert(
                    i,
                    (
                        call_id.clone(),
                        tool_name.clone(),
                        ToolResultBody::Error {
                            message: format!("invalid tool arguments: {error}"),
                            retryable: true,
                        },
                        Duration::ZERO,
                    ),
                );
            }
        }
    }

    // Parallel: bounded by a semaphore (§4.3).
    let sem = Arc::new(Semaphore::new(PARALLEL_BOUND));
    let mut set: JoinSet<(usize, (String, String, ToolResultBody, Duration))> = JoinSet::new();
    for (i, tool, call) in parallel {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let ctx = ctx.clone();
        set.spawn(async move {
            let r = run_one(tool, call, ctx).await;
            drop(permit);
            (i, r)
        });
    }
    while let Some(res) = set.join_next().await {
        let (i, r) = res.unwrap();
        results.insert(i, r);
    }
    // Serial writes: in model order.
    for (i, tool, call) in serial {
        results.insert(i, run_one(tool, call, ctx.clone()).await);
    }
    // Exclusive: one at a time, after everything else.
    for (i, tool, call) in exclusive {
        results.insert(i, run_one(tool, call, ctx.clone()).await);
    }

    order
        .into_iter()
        .map(|i| results.remove(&i).expect("a result for every batch item"))
        .collect()
}

/// Run one tool call: parse args, invoke, map the outcome, time it.
async fn run_one(
    tool: Arc<dyn Tool>,
    call: ToolCall,
    ctx: ToolCtx,
) -> (String, String, ToolResultBody, Duration) {
    let start = SystemTime::now();
    let input: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let outcome = match tool.call(input, &ctx).await {
        Ok(o) => o,
        Err(e) => ToolOutcome::Error { message: e.to_string(), retryable: false },
    };
    let dur = start.elapsed().unwrap_or(Duration::ZERO);
    (call.id, call.name, outcome.into(), dur)
}
