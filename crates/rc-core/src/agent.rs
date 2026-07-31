//! The agent loop (§4.2): streaming + tool calling with concurrency classes
//! (§4.3), the iteration budget, the tool-answer invariant, and per-call
//! permission checks (§7) — Allow→run, Deny→a denied tool result, Ask→prompter.

use crate::context::ContextAssembler;
use crate::model::{EventSink, FinalizedToolCall, Model, ModelError, ModelRequest, ModelResponse};
use crate::prompt::{AskResponse, Prompter};
use crate::project::{project, verify_invariant};
use crate::registry::ToolRegistry;
use crate::tool::{Concurrency, SandboxPolicy, Tool, ToolCtx, ToolOutcome};
use crate::turn::{Session, ToolCall, ToolResultBody, Turn};
use rc_perm::{Decision, PermissionChecker};
use rc_proto::{CompleteOpts, FinishReason, ToolChoiceValue, WireMessage};
use rc_tokenize::Estimator;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Semaphore;
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
    /// T3: the turn exceeded its wall-clock budget (`turn_timeout`).
    TimeUp,
}

/// Default tool-loop iterations per turn. Not a context limit — a runaway
/// backstop. `AgentLoop::with_max_iters` raises it (the CLI defaults to 1000,
/// which is far above any legitimate task and still terminates).
const MAX_ITERS: u32 = 100;
const PARALLEL_BOUND: usize = 8;

/// Char length of a wire message's payload, for the pre-flight context estimate.
/// Counts the text that dominates the body; the structural JSON around it is
/// noise at any interesting size.
fn message_len(m: &WireMessage) -> usize {
    match m {
        WireMessage::System { content } => content.chars().count(),
        WireMessage::User { content } => match content {
            rc_proto::wire::UserContent::Text(t) => t.chars().count(),
        },
        WireMessage::Assistant { content, tool_calls } => {
            content.as_deref().map_or(0, |c| c.chars().count())
                + tool_calls
                    .iter()
                    .map(|c| c.function.arguments.chars().count() + c.function.name.len())
                    .sum::<usize>()
        }
        WireMessage::Tool { content, .. } => content.chars().count(),
    }
}

/// The agent loop. Headless; the TUI (M4) and cancellation-via-Esc plug in
/// through the [`EventSink`] and a per-turn [`CancellationToken`]. Permission
/// decisions come from [`Self::permission`] (an `rc_perm::PermissionChecker`);
/// Ask escalations go to [`Self::run`]'s `prompter`. Context assembly (§4.6
/// system prompt, `@file` expansion, tool-output truncation) comes from
/// [`Self::assembler`] — `None` uses the legacy [`crate::project::project`]
/// path (M1–M5 behavior).
pub struct AgentLoop {
    pub model: Arc<dyn Model>,
    pub tools: Arc<ToolRegistry>,
    pub permission: Arc<dyn PermissionChecker>,
    pub max_iters: u32,
    pub assembler: Option<Arc<dyn ContextAssembler>>,
    /// T2: max gap between stream chunks before the model stream aborts with
    /// `ProtoError::Idle`. `None` (default) disables the idle bound.
    pub idle_timeout: Option<Duration>,
    /// T3: wall-clock budget for a turn. `None` (default) disables it; the turn
    /// is then bounded only by `max_iters` (count) + per-request timeouts.
    pub turn_timeout: Option<Duration>,
    /// M4: per-response completion-token cap (`max_tokens` on the request). `None`
    /// (default) uses the provider's default. Bounds the length of each reply.
    pub max_tokens: Option<u32>,
    /// Sampling temperature. `None` (default) uses the provider's default; `0`
    /// for reproducible runs.
    pub temperature: Option<f32>,
    /// M7: opt-in kernel sandbox policy for `Bash` (§7.6). `None` (default) =
    /// no confinement.
    pub sandbox: Option<SandboxPolicy>,
    /// M8: the calibrated token estimator (§4.7). **Observability only** — it
    /// never gates, truncates, or compacts anything. Each response's
    /// authoritative `prompt_tokens` calibrates the chars-per-token factor, and
    /// the pre-flight estimate is reported to the sink so a UI can show how big
    /// the context has grown. Subconscious Code deliberately has no window
    /// threshold for this to trip.
    pub estimator: Estimator,
}

impl AgentLoop {
    pub fn new(
        model: Arc<dyn Model>,
        tools: Arc<ToolRegistry>,
        permission: Arc<dyn PermissionChecker>,
    ) -> Self {
        Self {
            model,
            tools,
            permission,
            max_iters: MAX_ITERS,
            assembler: None,
            idle_timeout: None,
            turn_timeout: None,
            max_tokens: None,
            temperature: None,
            sandbox: None,
            estimator: Estimator::new(),
        }
    }

    /// Supply a §4.6 context assembler (from `rc-ctx`). When set, the loop
    /// builds the model request from `assembler.assemble(...)` instead of the
    /// legacy [`project`] call. Builder-style; returns `self` for chaining.
    #[must_use]
    pub fn with_assembler(mut self, assembler: Arc<dyn ContextAssembler>) -> Self {
        self.assembler = Some(assembler);
        self
    }

    /// Tool-loop iterations allowed in one turn (the runaway backstop, not a
    /// context limit). `0` means unlimited — a confused model then spends
    /// without bound, so prefer a large finite value. Builder.
    #[must_use]
    pub fn with_max_iters(mut self, max_iters: u32) -> Self {
        self.max_iters = if max_iters == 0 { u32::MAX } else { max_iters };
        self
    }

    /// T2: max gap between stream chunks before the model stream aborts with
    /// `ProtoError::Idle` (a stall). `None` (default) disables the idle bound;
    /// the stream is then bounded only by the total request timeout. Builder.
    #[must_use]
    pub fn with_idle_timeout(mut self, idle: Option<Duration>) -> Self {
        self.idle_timeout = idle;
        self
    }

    /// T3: wall-clock budget for a turn. Checked at the top of each loop
    /// iteration; on expiry the turn ends with `LoopOutcome::TimeUp`. `None`
    /// (default) disables it. Builder.
    #[must_use]
    pub fn with_turn_timeout(mut self, turn: Option<Duration>) -> Self {
        self.turn_timeout = turn;
        self
    }

    /// M4: per-response completion-token cap. `None` uses the provider default.
    /// Builder.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Sampling temperature (`None` = provider default). Builder.
    #[must_use]
    pub fn with_temperature(mut self, temperature: Option<f32>) -> Self {
        self.temperature = temperature;
        self
    }

    /// M7: opt-in kernel sandbox policy for `Bash` (§7.6). `None` (default)
    /// disables confinement. When `Some`, every approved Bash command runs
    /// under `rc-sandbox` (Landlock + seccomp on Linux; no-op elsewhere).
    /// Builder.
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: Option<SandboxPolicy>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Run a full turn for `user_input`. Mutates `session` (pushes turns).
    pub async fn run(
        &self,
        session: &mut Session,
        user_input: String,
        sink: &dyn EventSink,
        prompter: &dyn Prompter,
        cancel: CancellationToken,
    ) -> Result<LoopOutcome, LoopError> {
        session.messages.push(Turn::User { content: user_input, ts: SystemTime::now() });
        let turn_start = SystemTime::now();

        // M7: sync the session cwd from the live shell state (a `cd` from the
        // previous turn persists here), and stamp the change journal with a new
        // turn number so `/rewind` can attribute this turn's file changes.
        if let Ok(shell) = session.shell_state.lock() {
            session.cwd = shell.cwd.clone();
        }
        if let Ok(mut journal) = session.change_journal.lock() {
            journal.advance_turn();
        }

        let ctx = ToolCtx {
            cwd: session.cwd.clone(),
            allowed_roots: allowed_roots(session),
            cancel,
            read_registry: session.read_registry.clone(),
            shell_state: session.shell_state.clone(),
            change_journal: session.change_journal.clone(),
            sandbox: self.sandbox,
        };

        let mut iters = 0;
        loop {
            iters += 1;
            if iters > self.max_iters {
                return Ok(LoopOutcome::ItersExceeded);
            }
            // T3: wall-clock turn budget. The check runs at the top, after the
            // prior iteration's tool results were pushed, so the invariant holds.
            if let Some(d) = self.turn_timeout {
                if turn_start.elapsed().unwrap_or(Duration::ZERO) >= d {
                    return Ok(LoopOutcome::TimeUp);
                }
            }
            sink.on_iter(iters, self.max_iters);

            let messages = match &self.assembler {
                Some(a) => a.assemble(&session.messages),
                None => project(&session.messages),
            };
            let inv = verify_invariant(&messages);
            debug_assert!(inv.is_ok(), "tool-answer invariant: {:?}", inv);
            if let Err(e) = inv {
                tracing::error!("tool-answer invariant violated: {e}");
            }

            // Pre-flight context size (§4.7 #2): the char length of everything
            // we're about to send, and the calibrated token estimate for it.
            // Reported, never enforced — there is no window to exceed.
            let context_chars: usize = messages.iter().map(message_len).sum();
            let context_tokens = self.estimator.estimate_chars(context_chars);
            sink.on_context(context_chars, context_tokens);
            tracing::debug!(
                context_chars,
                context_tokens,
                factor = self.estimator.factor(),
                "assembled context"
            );

            let req = ModelRequest {
                messages,
                tools: self.tools.definitions().to_vec(),
                tool_choice: ToolChoiceValue::Auto,
                opts: CompleteOpts {
                    max_tokens: self.max_tokens,
                    temperature: self.temperature,
                    idle_timeout: self.idle_timeout,
                },
            };
            let ModelResponse { text, reasoning, tool_calls, finish_reason, usage } =
                self.model.complete(req, sink).await?;
            if let Some(u) = &usage {
                sink.on_usage(u);
                session.total_usage.add(u);
                // Calibrate against the authoritative count (§4.7 #1) so the
                // next pre-flight estimate is closer for this model.
                self.estimator.observe(u.prompt_tokens, context_chars);
            }

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

            // §4.2 / project.rs:69: any early exit must still synthesize tool results
            // for outstanding calls so the tool-answer invariant holds. On a non-
            // `ToolCalls` finish (Stop / Length / ContentFilter / Other — e.g. a
            // stream cut mid-tool-call) the tools are NOT executed; each outstanding
            // call gets an `Interrupted` result. (The hot `ToolCalls` path moves
            // `assistant_calls` into the turn; only the uncommon early paths clone.)
            match finish_reason {
                FinishReason::ToolCalls => {
                    session.messages.push(Turn::Assistant { text, reasoning, calls: assistant_calls, usage });
                }
                FinishReason::Stop => {
                    session.messages.push(Turn::Assistant { text, reasoning, calls: assistant_calls.clone(), usage });
                    synthesize_interrupted(&mut session.messages, &assistant_calls, sink);
                    return Ok(LoopOutcome::Stop);
                }
                FinishReason::Length => {
                    tracing::warn!("finish_reason=length; stopping (auto-continue is A13)");
                    session.messages.push(Turn::Assistant { text, reasoning, calls: assistant_calls.clone(), usage });
                    synthesize_interrupted(&mut session.messages, &assistant_calls, sink);
                    return Ok(LoopOutcome::Length);
                }
                FinishReason::ContentFilter | FinishReason::Other(_) => {
                    session.messages.push(Turn::Assistant { text, reasoning, calls: assistant_calls.clone(), usage });
                    synthesize_interrupted(&mut session.messages, &assistant_calls, sink);
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
                sink.on_tool_end(&call_id, &tool, &result);
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

/// §4.2 / `project.rs:69`: any early exit from a turn must still synthesize tool
/// results for outstanding calls so the tool-answer invariant holds. Called on
/// the non-`ToolCalls` finish paths (Stop / Length / ContentFilter / Other) —
/// e.g. a stream cut mid-tool-call leaves an assistant `tool_calls` message with
/// no answers, which `verify_invariant` would flag next turn (debug_assert panic
/// / a malformed request to the provider). The tools are NOT executed; each
/// outstanding call gets an `Interrupted` result and a terminal `on_tool_end`.
fn synthesize_interrupted(
    messages: &mut Vec<Turn>,
    calls: &[ToolCall],
    sink: &dyn EventSink,
) {
    for c in calls {
        sink.on_tool_end(&c.id, &c.name, &ToolResultBody::Interrupted);
        messages.push(Turn::ToolResult {
            call_id: c.id.clone(),
            tool: c.name.clone(),
            result: ToolResultBody::Interrupted,
            duration: Duration::ZERO,
        });
    }
}

enum ExecItem {
    Call(ToolCall),
    ParseError { call_id: String, tool_name: String, error: String },
}

/// A pending parallel tool task: its batch index, call id/name, and the
/// `tokio::spawn` handle. Kept outside the task so a panicking tool can still
/// be mapped back to a well-formed error result (see `execute_batch`).
type ParallelTask = (
    usize,
    String,
    String,
    tokio::task::JoinHandle<(String, String, ToolResultBody, Duration)>,
);

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
    // M7: refresh cwd from the live shell state so tools (and the permission
    // check) see any `cd` from a prior Bash call this turn. Bash is Exclusive
    // (serialized), so there's no concurrent-cd race; the lock is uncontended.
    let ctx = {
        let mut c = ctx.clone();
        if let Ok(shell) = c.shell_state.lock() {
            c.cwd = shell.cwd.clone();
        }
        c
    };
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
                    Decision::Ask(reason) => match prompter.ask(&call.name, &input, &reason).await {
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

    // Parallel: bounded by a semaphore (§4.3). Each call runs on its own
    // `tokio::spawn`'d task so a panicking tool (a tool-implementation bug) is
    // isolated by the runtime: `handle.await` yields `Err(JoinError)` instead of
    // propagating the panic and crashing the agent loop. The call id/name are
    // kept alongside the handle (outside the task) so the panicked call still
    // gets a well-formed error result — keeping the tool-answer invariant true
    // and the `expect("a result for every batch item")` below sound. (JoinSet
    // was used before, but its `join_next` yields a bare `JoinError` with no
    // index, so a panic couldn't be mapped back to its batch slot.)
    let sem = Arc::new(Semaphore::new(PARALLEL_BOUND));
    let mut handles: Vec<ParallelTask> = Vec::new();
    for (i, tool, call) in parallel {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let ctx = ctx.clone();
        let call_id = call.id.clone();
        let tool_name = call.name.clone();
        let handle = tokio::spawn(async move {
            let r = run_one(tool, call, ctx).await;
            drop(permit);
            r
        });
        handles.push((i, call_id, tool_name, handle));
    }
    for (i, call_id, tool_name, handle) in handles {
        let r = match handle.await {
            Ok(r) => r,
            Err(join_err) => {
                tracing::error!("parallel tool `{tool_name}` panicked: {join_err}");
                (
                    call_id,
                    tool_name,
                    ToolResultBody::Error {
                        message: format!("tool panicked: {join_err}"),
                        retryable: false,
                    },
                    Duration::ZERO,
                )
            }
        };
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
