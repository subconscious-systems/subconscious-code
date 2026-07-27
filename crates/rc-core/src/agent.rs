//! The agent loop (§4.2): streaming + tool calling with concurrency classes
//! (§4.3), the iteration budget, the tool-answer invariant, and per-call
//! permission checks (§7) — Allow→run, Deny→a denied tool result, Ask→prompter.

use crate::model::{EventSink, FinalizedToolCall, Model, ModelError, ModelRequest, ModelResponse};
use crate::prompt::{AskResponse, Prompter};
use crate::project::{project, verify_invariant};
use crate::registry::ToolRegistry;
use crate::tool::{Concurrency, Tool, ToolCtx, ToolOutcome};
use crate::turn::{Session, ToolCall, ToolResultBody, Turn};
use rc_perm::{Decision, PermissionChecker};
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
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopOutcome {
    Stop,
    Length,
    ItersExceeded,
}

const MAX_ITERS: u32 = 100;
const PARALLEL_BOUND: usize = 8;

/// The agent loop. Headless; the TUI (M4) and cancellation-via-Esc plug in
/// through the [`EventSink`] and a per-turn [`CancellationToken`]. Permission
/// decisions come from [`Self::permission`] (an `rc_perm::PermissionChecker`);
/// Ask escalations go to [`Self::run`]'s `prompter`.
pub struct AgentLoop {
    pub model: Arc<dyn Model>,
    pub tools: Arc<ToolRegistry>,
    pub permission: Arc<dyn PermissionChecker>,
    pub max_iters: u32,
}

impl AgentLoop {
    pub fn new(
        model: Arc<dyn Model>,
        tools: Arc<ToolRegistry>,
        permission: Arc<dyn PermissionChecker>,
    ) -> Self {
        Self { model, tools, permission, max_iters: MAX_ITERS }
    }

    /// Run a full turn for `user_input`. Mutates `session` (pushes turns).
    pub async fn run(
        &self,
        session: &mut Session,
        user_input: String,
        _sink: &dyn EventSink,
        prompter: &dyn Prompter,
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

            let mut assistant_calls = Vec::new();
            let mut exec_list: Vec<ExecItem> = Vec::new();
            for fc in tool_calls {
                match fc {
                    FinalizedToolCall::Call(c) => {
                        assistant_calls.push(c.clone());
                        exec_list.push(ExecItem::Call(c));
                    }
                    FinalizedToolCall::ParseError { id, name, raw, error } => {
                        let call_id =
                            id.clone().unwrap_or_else(|| format!("parseerr_{}", assistant_calls.len()));
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
                FinishReason::ToolCalls => {}
                FinishReason::ContentFilter | FinishReason::Other(_) => {
                    return Ok(LoopOutcome::Stop);
                }
            }

            let results = execute_batch(
                &exec_list,
                &self.tools,
                &ctx,
                self.permission.as_ref(),
                prompter,
                &mut session.perm_grants,
            )
            .await;
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

/// Execute a batch of tool calls per the concurrency policy (§4.3), after a
/// per-call permission check (§7). Denied/Ask-denied calls become denied tool
/// results without running; Ask-approved calls run (a Session grant is added to
/// `grants`). Results are returned in the original tool-call order.
async fn execute_batch(
    items: &[ExecItem],
    tools: &ToolRegistry,
    ctx: &ToolCtx,
    permission: &dyn PermissionChecker,
    prompter: &dyn Prompter,
    grants: &mut Vec<String>,
) -> Vec<(String, String, ToolResultBody, Duration)> {
    let mut results: HashMap<usize, (String, String, ToolResultBody, Duration)> = HashMap::new();
    let mut order: Vec<usize> = Vec::new();
    let mut parallel: Vec<(usize, Arc<dyn Tool>, ToolCall)> = Vec::new();
    let mut serial: Vec<(usize, Arc<dyn Tool>, ToolCall)> = Vec::new();
    let mut exclusive: Vec<(usize, Arc<dyn Tool>, ToolCall)> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        order.push(i);
        match item {
            ExecItem::Call(call) => {
                let input: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
                let allowed = match permission.check(
                    &call.name,
                    &input,
                    &ctx.cwd,
                    &ctx.allowed_roots,
                    grants,
                ) {
                    Decision::Allow => true,
                    Decision::Deny(reason) => {
                        results.insert(
                            i,
                            (
                                call.id.clone(),
                                call.name.clone(),
                                ToolResultBody::Denied { reason },
                                Duration::ZERO,
                            ),
                        );
                        false
                    }
                    Decision::Ask(reason) => match prompter.ask(&call.name, &input, &reason) {
                        AskResponse::Once => true,
                        AskResponse::Session(rule) | AskResponse::Always(rule) => {
                            grants.push(rule);
                            true
                        }
                        AskResponse::Deny(reason) => {
                            results.insert(
                                i,
                                (
                                    call.id.clone(),
                                    call.name.clone(),
                                    ToolResultBody::Denied { reason },
                                    Duration::ZERO,
                                ),
                            );
                            false
                        }
                    },
                };
                if !allowed {
                    continue;
                }
                match tools.get(&call.name) {
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
                }
            }
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
    for (i, tool, call) in serial {
        results.insert(i, run_one(tool, call, ctx.clone()).await);
    }
    for (i, tool, call) in exclusive {
        results.insert(i, run_one(tool, call, ctx.clone()).await);
    }

    order
        .into_iter()
        .map(|i| results.remove(&i).expect("a result for every batch item"))
        .collect()
}

/// Run one tool call: invoke, map the outcome, time it. (Permission was already
/// checked by the caller; this only runs Allow'd calls.)
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
