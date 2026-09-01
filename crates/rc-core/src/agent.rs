//! The agent loop (§4.2): streaming + tool calling with concurrency classes
//! (§4.3), the iteration budget, the tool-answer invariant, and per-call
//! permission checks (§7) — Allow→run, Deny→a denied tool result, Ask→prompter.

use crate::context::ContextAssembler;
use crate::cost::Pricing;
use crate::model::{EventSink, FinalizedToolCall, Model, ModelError, ModelRequest, ModelResponse};
use crate::project::{project, safe_tool_arguments, verify_invariant};
use crate::prompt::{AskResponse, Prompter};
use crate::registry::ToolRegistry;
use crate::tool::{Artifact, Concurrency, SandboxPolicy, Tool, ToolCtx, ToolOutcome};
use crate::turn::{
    ModelTrace, PartialStreamResponse, PartialToolCall, Session, ToolCall, ToolResultBody, Turn,
};
use rc_perm::{Decision, PermissionChecker};
use rc_proto::{CompleteOpts, FinishReason, ProtoError, ToolChoiceValue, Usage, WireMessage};
use rc_tokenize::Estimator;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
    /// Two consecutive completion-limit responses made no visible or tool
    /// progress. Continuing again would only spend another model allowance.
    NoProgress,
    /// The provider ended the response without a clean terminal marker (or a
    /// content filter stopped it). This is not a successful `Stop` outcome.
    Incomplete,
    /// T3: the turn exceeded its wall-clock budget (`turn_timeout`).
    TimeUp,
    /// The user cancelled the turn (Esc). The request was interrupted —
    /// typically surfacing as `ProtoError::Idle` from a mid-stream stall —
    /// and a `Turn::Cancelled` record was appended so the interruption is
    /// visible in the session transcript instead of being lost as a bare
    /// `LoopError::Model`. Distinct from `TimeUp` (an automatic budget) and
    /// from `Stop` (a clean model finish).
    Cancelled,
}

/// Default tool-loop iterations per turn. Not a context limit — a runaway
/// backstop. `AgentLoop::with_max_iters` raises it (the CLI defaults to 1000,
/// which is far above any legitimate task and still terminates).
const MAX_ITERS: u32 = 100;
const PARALLEL_BOUND: usize = 8;
const RESEARCH_NUDGE_AFTER: u32 = 3;
const RESEARCH_NUDGE: &str = "[agent guidance] You have spent three consecutive tool rounds on \
investigation. Before another round, consolidate what is already known and list every remaining \
unknown. If more evidence is truly required, fetch all independent paths and queries together in \
one parallel response (prefer ReadMany for files); otherwise edit or answer now.";
const FORCE_ACTION_RECOVERY: &str = "[harness recovery] Your previous response exhausted the \
completion budget without producing visible answer text or a confirmed tool call. Do not restart \
or continue hidden analysis. Take the next observable action now: call the required tool or provide \
the answer.";
const PARTIAL_ANSWER_RECOVERY: &str =
    "[harness recovery] Continue from the visible partial response \
above without repeating it. Complete the task now.";
const FINAL_REVIEW_RECOVERY: &str = "[benchmark completion gate] Before finalizing, perform one \
independent completion audit. Inspect the final diff, map every acceptance requirement to the \
implementation, and run the broadest relevant verification that is available after the last edit. \
Do not treat self-authored tests alone as sufficient evidence. If verification fails or a \
requirement is uncovered, fix it and verify again; otherwise provide the final answer.";
/// At most one extra model request may be manufactured for a truncated answer.
/// More attempts hide a serving/output-budget failure behind a loop of synthetic
/// continuations and can keep benchmark tasks alive indefinitely.
const MAX_LENGTH_RECOVERIES_PER_TURN: u32 = 1;
/// A cut stream may receive one chance to reissue an unconfirmed tool call.
/// Repeated unconfirmed mutations are terminal: executing them is unsafe and
/// retrying forever only grows the malformed-history surface.
const MAX_UNCONFIRMED_TOOL_RECOVERIES_PER_TURN: u32 = 1;
/// Keep failure diagnostics useful without duplicating an arbitrarily large
/// generated document or hidden reasoning trace in memory/session storage.
const PARTIAL_STREAM_FIELD_BYTES: usize = 16 * 1024;

#[derive(Default)]
struct RequestObservation {
    response_headers: Option<Duration>,
    ttft: Option<Duration>,
    request_bytes: usize,
    wire_bytes: usize,
    retries: u32,
    transport_events: u64,
    semantic_events: u64,
    last_transport_activity: Option<Duration>,
    last_semantic_activity: Option<Duration>,
    partial_text_chars: usize,
    partial_reasoning_chars: usize,
    partial_tool_argument_chars: usize,
    partial_text: String,
    partial_reasoning: String,
    partial_tools: Vec<PartialToolCall>,
    partial_truncated: bool,
}

/// Per-request sink wrapper that forwards live UI events while collecting the
/// metrics needed for a durable JSONL trace. The model trait stays unchanged,
/// so mock models and alternate providers automatically participate.
struct RequestTraceSink<'a> {
    inner: &'a dyn EventSink,
    started: Instant,
    started_ms: u64,
    context_chars: usize,
    context_tokens: usize,
    observation: Mutex<RequestObservation>,
}

impl<'a> RequestTraceSink<'a> {
    fn new(inner: &'a dyn EventSink, context_chars: usize, context_tokens: usize) -> Self {
        Self {
            inner,
            started: Instant::now(),
            started_ms: epoch_ms(SystemTime::now()),
            context_chars,
            context_tokens,
            observation: Mutex::new(RequestObservation::default()),
        }
    }

    fn mark_first_output(&self) {
        if let Ok(mut observation) = self.observation.lock() {
            observation
                .ttft
                .get_or_insert_with(|| self.started.elapsed());
        }
    }

    fn partial_response(&self) -> Option<PartialStreamResponse> {
        let observation = self.observation.lock().unwrap_or_else(|e| e.into_inner());
        if observation.partial_text.is_empty()
            && observation.partial_reasoning.is_empty()
            && observation.partial_tools.is_empty()
        {
            return None;
        }
        Some(PartialStreamResponse {
            text: observation.partial_text.clone(),
            reasoning: observation.partial_reasoning.clone(),
            tool_calls: observation.partial_tools.clone(),
            truncated: observation.partial_truncated,
        })
    }

    fn finish(
        &self,
        reported: &str,
        effective: &str,
        retries: u32,
        implicit_length: bool,
    ) -> ModelTrace {
        let total = self.started.elapsed();
        let completed_ms = epoch_ms(SystemTime::now());
        let observation = self.observation.lock().unwrap_or_else(|e| e.into_inner());
        ModelTrace {
            started_ms: self.started_ms,
            completed_ms,
            total_ms: duration_ms(total),
            response_headers_ms: observation.response_headers.map(duration_ms),
            ttft_ms: observation.ttft.map(duration_ms),
            stream_ms: observation
                .response_headers
                .map(|headers| duration_ms(total.saturating_sub(headers))),
            request_bytes: observation.request_bytes,
            wire_bytes: observation.wire_bytes,
            context_chars: self.context_chars,
            context_tokens_estimate: self.context_tokens,
            retries: retries.max(observation.retries),
            reported_finish_reason: reported.to_string(),
            effective_finish_reason: effective.to_string(),
            implicit_length,
            transport_events: observation.transport_events,
            semantic_events: observation.semantic_events,
            last_transport_activity_ms: observation.last_transport_activity.map(duration_ms),
            last_semantic_activity_ms: observation.last_semantic_activity.map(duration_ms),
            partial_text_chars: observation.partial_text_chars,
            partial_reasoning_chars: observation.partial_reasoning_chars,
            partial_tool_argument_chars: observation.partial_tool_argument_chars,
        }
    }
}

fn append_partial(target: &mut String, delta: &str) -> bool {
    let remaining = PARTIAL_STREAM_FIELD_BYTES.saturating_sub(target.len());
    if remaining == 0 {
        return !delta.is_empty();
    }
    let mut take = delta.len().min(remaining);
    while take > 0 && !delta.is_char_boundary(take) {
        take -= 1;
    }
    target.push_str(&delta[..take]);
    take < delta.len()
}

impl EventSink for RequestTraceSink<'_> {
    fn on_text(&self, delta: &str) {
        if !delta.is_empty() {
            self.mark_first_output();
            if let Ok(mut observation) = self.observation.lock() {
                let elapsed = self.started.elapsed();
                observation.semantic_events = observation.semantic_events.saturating_add(1);
                observation.last_semantic_activity = Some(elapsed);
                observation.last_transport_activity = Some(elapsed);
                observation.partial_text_chars = observation
                    .partial_text_chars
                    .saturating_add(delta.chars().count());
                let truncated = append_partial(&mut observation.partial_text, delta);
                observation.partial_truncated |= truncated;
            }
        }
        self.inner.on_text(delta);
    }

    fn on_reasoning(&self, delta: &str) {
        if !delta.is_empty() {
            self.mark_first_output();
            if let Ok(mut observation) = self.observation.lock() {
                let elapsed = self.started.elapsed();
                observation.semantic_events = observation.semantic_events.saturating_add(1);
                observation.last_semantic_activity = Some(elapsed);
                observation.last_transport_activity = Some(elapsed);
                observation.partial_reasoning_chars = observation
                    .partial_reasoning_chars
                    .saturating_add(delta.chars().count());
                let truncated = append_partial(&mut observation.partial_reasoning, delta);
                observation.partial_truncated |= truncated;
            }
        }
        self.inner.on_reasoning(delta);
    }

    fn on_transport_activity(&self) {
        if let Ok(mut observation) = self.observation.lock() {
            observation.transport_events = observation.transport_events.saturating_add(1);
            observation.last_transport_activity = Some(self.started.elapsed());
        }
        self.inner.on_transport_activity();
    }

    fn on_tool_delta(&self, index: usize, id: Option<&str>, name: Option<&str>, arguments: &str) {
        self.mark_first_output();
        if let Ok(mut observation) = self.observation.lock() {
            let elapsed = self.started.elapsed();
            observation.semantic_events = observation.semantic_events.saturating_add(1);
            observation.last_semantic_activity = Some(elapsed);
            observation.last_transport_activity = Some(elapsed);
            observation.partial_tool_argument_chars = observation
                .partial_tool_argument_chars
                .saturating_add(arguments.chars().count());
            let tool = if let Some(tool) = observation
                .partial_tools
                .iter_mut()
                .find(|tool| tool.index == index)
            {
                tool
            } else {
                observation.partial_tools.push(PartialToolCall {
                    index,
                    ..PartialToolCall::default()
                });
                observation.partial_tools.last_mut().expect("just pushed")
            };
            if tool.id.is_none() {
                tool.id = id.map(str::to_string);
            }
            if tool.name.is_none() {
                tool.name = name.map(str::to_string);
            }
            let truncated = append_partial(&mut tool.arguments, arguments);
            tool.truncated |= truncated;
            observation.partial_truncated |= truncated;
        }
        self.inner.on_tool_delta(index, id, name, arguments);
    }

    fn on_tool_start(&self, call: &ToolCall) {
        self.mark_first_output();
        self.inner.on_tool_start(call);
    }

    fn on_finish(&self, reason: &FinishReason) {
        self.inner.on_finish(reason);
    }

    fn on_tool_end(&self, call_id: &str, tool: &str, result: &ToolResultBody) {
        self.inner.on_tool_end(call_id, tool, result);
    }

    fn on_artifact(&self, call_id: &str, tool: &str, artifact: &Artifact) {
        self.inner.on_artifact(call_id, tool, artifact);
    }

    fn on_iter(&self, count: u32, max: u32) {
        self.inner.on_iter(count, max);
    }

    fn on_usage(&self, usage: &Usage) {
        self.inner.on_usage(usage);
    }

    fn on_retry(&self, retries: u32) {
        if let Ok(mut observation) = self.observation.lock() {
            observation.retries = observation.retries.max(retries);
        }
        self.inner.on_retry(retries);
    }

    fn on_request_payload(&self, json_bytes: usize, wire_bytes: usize) {
        if let Ok(mut observation) = self.observation.lock() {
            observation.request_bytes = json_bytes;
            observation.wire_bytes = wire_bytes;
        }
        self.inner.on_request_payload(json_bytes, wire_bytes);
    }

    fn on_response_headers(&self, elapsed: Duration) {
        if let Ok(mut observation) = self.observation.lock() {
            observation.response_headers = Some(elapsed);
        }
        self.inner.on_response_headers(elapsed);
    }

    fn on_context(&self, chars: usize, est_tokens: usize) {
        self.inner.on_context(chars, est_tokens);
    }
}

fn epoch_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn finish_reason_name(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".into(),
        FinishReason::Length => "length".into(),
        FinishReason::ToolCalls => "tool_calls".into(),
        FinishReason::ContentFilter => "content_filter".into(),
        FinishReason::Other(reason) => format!("other:{reason}"),
    }
}

fn looks_like_implicit_length(response: &ModelResponse, configured_max: Option<u32>) -> bool {
    if response.finish_reason != FinishReason::Stop
        || !response.text.trim().is_empty()
        || !response.tool_calls.is_empty()
        || response
            .reasoning
            .as_deref()
            .is_none_or(|reasoning| reasoning.trim().is_empty())
    {
        return false;
    }
    let Some(completion_tokens) = response.usage.as_ref().map(|usage| usage.completion_tokens)
    else {
        return false;
    };
    match configured_max {
        Some(limit) => completion_tokens.saturating_add(8) >= u64::from(limit),
        // The observed GLM route defaults to 4096 output tokens. Requiring the
        // exact common ceiling avoids looping on a legitimately empty response.
        None => completion_tokens >= 4096,
    }
}

fn is_investigation_batch(items: &[ExecItem]) -> bool {
    !items.is_empty()
        && items.iter().all(|item| match item {
            ExecItem::Call(call) => !matches!(call.name.as_str(), "Write" | "Append" | "Edit"),
            ExecItem::ParseError { .. } => false,
        })
}

/// Char length of a wire message's payload, for the pre-flight context estimate.
/// Counts the text that dominates the body; the structural JSON around it is
/// noise at any interesting size.
fn message_len(m: &WireMessage) -> usize {
    match m {
        WireMessage::System { content } => content.chars().count(),
        WireMessage::User { content } => match content {
            rc_proto::wire::UserContent::Text(t) => t.chars().count(),
        },
        WireMessage::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            content.as_deref().map_or(0, |c| c.chars().count())
                + reasoning_content
                    .as_deref()
                    .map_or(0, |reasoning| reasoning.chars().count())
                + tool_calls
                    .iter()
                    .map(|c| c.function.arguments.chars().count() + c.function.name.len())
                    .sum::<usize>()
        }
        WireMessage::Tool { content, .. } => content.chars().count(),
    }
}

/// Whether a request failure is worth retrying — recorded on `Turn::Error` so a
/// resumed session (or an operator reading the trace) can tell a transient
/// failure (429/5xx, transport drop, mid-stream stall) from a permanent one
/// (no API key, bad session id, malformed body). Conservative: transport and
/// idle errors are retryable; auth/config/validation errors are not.
fn is_retryable(e: &ModelError) -> bool {
    let ModelError::Proto { error, .. } = e;
    match error {
        ProtoError::Status { status, .. } => *status == 429 || (500..=599).contains(status),
        ProtoError::Http(_) | ProtoError::Idle(_) => true,
        ProtoError::EmptyChoices
        | ProtoError::Json(_)
        | ProtoError::NoApiKey
        | ProtoError::InvalidSessionId
        | ProtoError::Dlr(_)
        | ProtoError::Gzip(_)
        | ProtoError::Io(_) => false,
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
    /// Provider-native reasoning posture. `None` omits the request field.
    pub reasoning_effort: Option<String>,
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
    /// Token pricing for cost accounting (`rc_core::cost`), in integer
    /// micro-USD per million tokens. Defaults to [`Pricing::ZERO`] — cost
    /// accounting then runs but every response costs zero, a no-op until a
    /// real price sheet is supplied via [`AgentLoop::with_pricing`].
    pub pricing: Pricing,
    /// Hard per-tool-result backstop (bytes). Distinct from the user-facing
    /// model-context projection cap. A
    /// runaway `Bash`/`Read` can pour gigabytes into the context; `max_iters`
    /// and the turn timeout are the only other guards. This cap is applied to
    /// every tool result before it enters the session, head-truncated with a
    /// sentinel so the model sees it was cut. `0` disables it (truly
    /// unlimited); the default is [`HARD_TOOL_RESULT_CAP`].
    pub hard_tool_result_cap: usize,
    /// Require one bounded independent review after a benchmark coding turn
    /// first attempts to stop following tool work. Disabled for interactive
    /// sessions; the CLI enables it when benchmark artifacts are requested.
    pub completion_review: bool,
}

/// Default hard backstop on a single tool result: 1 MiB. A single runaway
/// command or recursive inventory must not bloat the session or overflow the
/// provider's next request before context projection runs. Set
/// `hard_tool_result_cap = 0` to disable it entirely.
pub const HARD_TOOL_RESULT_CAP: usize = 1024 * 1024;

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
            reasoning_effort: None,
            sandbox: None,
            estimator: Estimator::new(),
            pricing: Pricing::ZERO,
            hard_tool_result_cap: HARD_TOOL_RESULT_CAP,
            completion_review: false,
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

    /// Provider-native reasoning effort (`None` = provider default). Builder.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
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

    /// Hard per-tool-result backstop in bytes (the runaway guard, distinct
    /// from the user-facing per-tool caps). `0` disables it (truly unlimited).
    /// The default is [`HARD_TOOL_RESULT_CAP`] (1 MiB): large enough for useful
    /// output, small enough to keep a runaway command from invalidating the
    /// provider's next request. Builder.
    #[must_use]
    pub fn with_hard_tool_result_cap(mut self, cap: usize) -> Self {
        self.hard_tool_result_cap = cap;
        self
    }

    #[must_use]
    pub fn with_completion_review(mut self, enabled: bool) -> Self {
        self.completion_review = enabled;
        self
    }

    /// Token pricing for cost accounting, in integer micro-USD per million
    /// tokens (see [`crate::cost::Pricing`]). Integer per-million pricing keeps
    /// the running cost a true monoid — shard/reduce in any order, get the same
    /// number. Defaults to [`Pricing::ZERO`] (a no-op). Builder.
    #[must_use]
    pub fn with_pricing(mut self, pricing: Pricing) -> Self {
        self.pricing = pricing;
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
        push_turn(
            session,
            sink,
            Turn::User {
                content: Arc::from(user_input),
                ts: SystemTime::now(),
            },
        );
        let turn_start = SystemTime::now();
        let turn_deadline = self
            .turn_timeout
            .map(|duration| tokio::time::Instant::now() + duration);

        // M7: sync the session cwd from the live shell state (a `cd` from the
        // previous turn persists here), and stamp the change journal with a new
        // turn number so `/rewind` can attribute this turn's file changes.
        if let Ok(shell) = session.shell_state.lock() {
            session.cwd = shell.cwd.clone();
        }
        if let Ok(mut journal) = session.change_journal.lock() {
            journal.advance_turn();
        }

        // `cancel` is moved into `ToolCtx` below; keep a cheap clone (an `Arc`
        // bump) so the request-failure path can still test whether the user
        // interrupted, to distinguish `Turn::Cancelled` from `Turn::Error`.
        let turn_cancel = cancel.child_token();
        let cancel_check = turn_cancel.clone();
        let ctx = ToolCtx {
            cwd: session.cwd.clone(),
            allowed_roots: allowed_roots(session),
            cancel: turn_cancel.clone(),
            read_registry: session.read_registry.clone(),
            shell_state: session.shell_state.clone(),
            change_journal: session.change_journal.clone(),
            sandbox: self.sandbox,
        };

        let mut iters = 0;
        let mut consecutive_investigation_rounds = 0u32;
        let mut length_recoveries_used = 0u32;
        let mut unconfirmed_tool_recoveries_used = 0u32;
        let mut completion_review_used = false;
        let mut tool_work_observed = false;
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

            // A Bash tool may have changed directory in the preceding
            // iteration of this same turn. Keep context environment/memory and
            // tool execution rooted in the same live cwd.
            if let Ok(shell) = session.shell_state.lock() {
                session.cwd = shell.cwd.clone();
            }

            let mut messages = match &self.assembler {
                Some(a) => a.assemble_for(&session.messages, &session.cwd),
                None => project(&session.messages),
            };
            if consecutive_investigation_rounds >= RESEARCH_NUDGE_AFTER {
                messages.push(WireMessage::User {
                    content: RESEARCH_NUDGE.into(),
                });
            }
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
                    reasoning_effort: self.reasoning_effort.clone(),
                    session_id: Some(session.id.clone()),
                    idle_timeout: self.idle_timeout,
                },
            };
            let request_sink = RequestTraceSink::new(sink, context_chars, context_tokens);
            let model_result = match await_turn_budget(
                self.model.complete(req, &request_sink),
                turn_deadline,
                &cancel_check,
            )
            .await
            {
                TurnWait::Ready(result) => result,
                TurnWait::Cancelled => {
                    push_turn(
                        session,
                        sink,
                        Turn::Cancelled {
                            ts: SystemTime::now(),
                        },
                    );
                    return Ok(LoopOutcome::Cancelled);
                }
                TurnWait::TimeUp => {
                    turn_cancel.cancel();
                    return Ok(LoopOutcome::TimeUp);
                }
            };
            let response = match model_result {
                Ok(response) => response,
                // A request failure is the "lack of errors" blind spot: before,
                // it propagated as a bare `LoopError::Model` and left *no* mark
                // in the transcript — a resumed session had no record that a
                // request was attempted and failed. Record a `Turn::Error`
                // (or `Turn::Cancelled` if the user interrupted) so the trace is
                // honest about what happened. The error still propagates.
                Err(e) => {
                    let retries = match &e {
                        ModelError::Proto { retries, .. } => *retries,
                    };
                    let trace = request_sink.finish("error", "error", retries, false);
                    let partial = request_sink.partial_response();
                    if cancel_check.is_cancelled() {
                        push_turn(
                            session,
                            sink,
                            Turn::Cancelled {
                                ts: SystemTime::now(),
                            },
                        );
                        return Ok(LoopOutcome::Cancelled);
                    }
                    push_turn(
                        session,
                        sink,
                        Turn::Error {
                            message: Arc::<str>::from(format!("{e}")),
                            retryable: Some(is_retryable(&e)),
                            retries: (retries > 0).then_some(retries),
                            trace: Some(trace),
                            partial,
                            ts: SystemTime::now(),
                        },
                    );
                    return Err(LoopError::Model(e));
                }
            };
            let implicit_length = looks_like_implicit_length(&response, self.max_tokens);
            let reported_finish_reason = finish_reason_name(&response.finish_reason);
            let effective_finish_reason = if implicit_length {
                FinishReason::Length
            } else {
                response.finish_reason.clone()
            };
            let effective_finish_reason_name = finish_reason_name(&effective_finish_reason);
            let trace = request_sink.finish(
                &reported_finish_reason,
                &effective_finish_reason_name,
                response.retries,
                implicit_length,
            );
            if implicit_length {
                tracing::warn!(
                    completion_tokens = response
                        .usage
                        .as_ref()
                        .map_or(0, |usage| usage.completion_tokens),
                    "empty reasoning-only response reached the completion ceiling"
                );
            }
            let ModelResponse {
                text,
                reasoning,
                tool_calls,
                finish_reason: _,
                usage,
                retries: _,
            } = response;
            let finish_reason = effective_finish_reason;
            // Wrap the response text once; the assistant turn (and any re-sends of
            // it on later turns) then share this allocation via refcount bumps.
            let text = Arc::<str>::from(text);
            let reasoning = reasoning.map(Arc::<str>::from);
            // Integer micro-USD cost of this response — the accounting monoid.
            // Computed once here (Copy) and attached to the assistant turn so a
            // resumed session reconstructs `total_cost` exactly from the records.
            let turn_cost = usage.as_ref().map(|u| self.pricing.cost_of(u));
            if let Some(u) = &usage {
                sink.on_usage(u);
                session.total_usage.add(u);
                if let Some(c) = turn_cost {
                    session.total_cost.add(&c);
                }
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
                    FinalizedToolCall::ParseError {
                        id,
                        name,
                        raw,
                        error,
                    } => {
                        let call_id = id
                            .clone()
                            .unwrap_or_else(|| format!("parseerr_{}", assistant_calls.len()));
                        let tool_name = name
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| "invalid_tool_call".to_string());
                        let raw = Arc::<str>::from(raw);
                        assistant_calls.push(ToolCall {
                            id: call_id.clone(),
                            name: tool_name.clone(),
                            // Never persist incomplete raw argument bytes as a
                            // normal assistant tool call. Providers validate
                            // replayed history before considering the paired
                            // error result, so malformed JSON here would make
                            // every later request fail with HTTP 400.
                            arguments: safe_tool_arguments(&raw),
                        });
                        exec_list.push(ExecItem::ParseError {
                            call_id,
                            tool_name,
                            error,
                        });
                    }
                }
            }
            // A provider can emit a complete-looking tool call and then end the
            // stream without `finish_reason=tool_calls` (or hit its output limit
            // while emitting one). Do not execute an unconfirmed call: structurally
            // valid JSON can still be a semantically truncated mutation. Pair it
            // with a retryable error, then continue the same agent turn so the model
            // can reissue the complete call. This both preserves the tool-answer
            // invariant and avoids making the user submit another prompt.
            let unconfirmed_reason = if assistant_calls.is_empty() {
                None
            } else {
                match &finish_reason {
                    FinishReason::ToolCalls | FinishReason::ContentFilter => None,
                    FinishReason::Stop => Some("stop".to_string()),
                    FinishReason::Length => Some("length".to_string()),
                    FinishReason::Other(reason) => Some(format!("other ({reason})")),
                }
            };
            if let Some(reason) = unconfirmed_reason {
                tracing::warn!(
                    finish_reason = %reason,
                    calls = assistant_calls.len(),
                    "tool calls were not confirmed; asking the model to retry"
                );
                push_turn(
                    session,
                    sink,
                    Turn::Assistant {
                        text,
                        reasoning,
                        calls: assistant_calls.clone(),
                        usage,
                        cost: turn_cost,
                        trace: Some(trace.clone()),
                    },
                );
                synthesize_retryable_unconfirmed(session, &assistant_calls, sink, &reason);
                if unconfirmed_tool_recoveries_used >= MAX_UNCONFIRMED_TOOL_RECOVERIES_PER_TURN {
                    tracing::warn!(
                        "model repeated an unconfirmed tool call; ending the turn safely"
                    );
                    return Ok(LoopOutcome::Incomplete);
                }
                unconfirmed_tool_recoveries_used += 1;
                continue;
            }

            // §4.2 / project.rs:69: every early exit must still pair any
            // outstanding calls with results. Content-filtered calls are never
            // retried automatically; they retain the terminal Interrupted result.
            match finish_reason {
                FinishReason::ToolCalls => {
                    push_turn(
                        session,
                        sink,
                        Turn::Assistant {
                            text,
                            reasoning,
                            calls: assistant_calls.clone(),
                            usage,
                            cost: turn_cost,
                            trace: Some(trace.clone()),
                        },
                    );
                }
                FinishReason::Stop => {
                    push_turn(
                        session,
                        sink,
                        Turn::Assistant {
                            text,
                            reasoning,
                            calls: assistant_calls.clone(),
                            usage,
                            cost: turn_cost,
                            trace: Some(trace.clone()),
                        },
                    );
                    synthesize_interrupted(session, &assistant_calls, sink);
                    if self.completion_review && tool_work_observed && !completion_review_used {
                        completion_review_used = true;
                        push_turn(
                            session,
                            sink,
                            Turn::SystemNote {
                                kind: crate::turn::NoteKind::Recovery,
                                text: FINAL_REVIEW_RECOVERY.to_string(),
                            },
                        );
                        tracing::debug!(
                            "benchmark completion gate requested one final implementation audit"
                        );
                        continue;
                    }
                    return Ok(LoopOutcome::Stop);
                }
                FinishReason::Length => {
                    // A13: bounded recovery. A visible partial answer may be
                    // continued because it is replayed in the next request. An
                    // empty reasoning-only answer gets one force-action recovery;
                    // a second no-progress response terminates rather than
                    // manufacturing an unbounded chain of `User("continue")`.
                    if assistant_calls.is_empty() {
                        let no_progress = text.trim().is_empty();
                        push_turn(
                            session,
                            sink,
                            Turn::Assistant {
                                text,
                                reasoning,
                                calls: Vec::new(),
                                usage,
                                cost: turn_cost,
                                trace: Some(trace.clone()),
                            },
                        );
                        if length_recoveries_used >= MAX_LENGTH_RECOVERIES_PER_TURN {
                            tracing::warn!(
                                visible_progress = !no_progress,
                                "completion recovery budget exhausted; ending the turn"
                            );
                            return Ok(if no_progress {
                                LoopOutcome::NoProgress
                            } else {
                                LoopOutcome::Length
                            });
                        }
                        length_recoveries_used += 1;
                        if no_progress {
                            push_turn(
                                session,
                                sink,
                                Turn::SystemNote {
                                    kind: crate::turn::NoteKind::Recovery,
                                    text: FORCE_ACTION_RECOVERY.to_string(),
                                },
                            );
                            tracing::warn!(
                                "completion limit reached without observable progress; forcing action once"
                            );
                        } else {
                            push_turn(
                                session,
                                sink,
                                Turn::SystemNote {
                                    kind: crate::turn::NoteKind::Recovery,
                                    text: PARTIAL_ANSWER_RECOVERY.to_string(),
                                },
                            );
                            tracing::debug!(
                                "finish=length after visible output; requesting bounded continuation"
                            );
                        }
                        continue;
                    }
                    unreachable!("outstanding length-finished calls retry above");
                }
                FinishReason::ContentFilter | FinishReason::Other(_) => {
                    push_turn(
                        session,
                        sink,
                        Turn::Assistant {
                            text,
                            reasoning,
                            calls: assistant_calls.clone(),
                            usage,
                            cost: turn_cost,
                            trace: Some(trace.clone()),
                        },
                    );
                    synthesize_interrupted(session, &assistant_calls, sink);
                    return Ok(LoopOutcome::Incomplete);
                }
            }

            let investigation_batch = is_investigation_batch(&exec_list);
            tool_work_observed |= !exec_list.is_empty();
            let batch_checkpoint = Arc::new(Mutex::new(BatchCheckpoint::new(exec_list.len())));
            let results = match await_turn_budget(
                execute_batch(
                    &exec_list,
                    &mut session.perm_grants,
                    BatchExecutor {
                        tools: self.tools.as_ref(),
                        ctx: &ctx,
                        permission: self.permission.as_ref(),
                        prompter,
                        sink,
                        hard_result_cap: self.hard_tool_result_cap,
                        checkpoint: batch_checkpoint.clone(),
                    },
                ),
                turn_deadline,
                &cancel_check,
            )
            .await
            {
                TurnWait::Ready(results) => results,
                TurnWait::Cancelled => {
                    turn_cancel.cancel();
                    finish_partial_batch(session, &assistant_calls, sink, &batch_checkpoint);
                    push_turn(
                        session,
                        sink,
                        Turn::Cancelled {
                            ts: SystemTime::now(),
                        },
                    );
                    return Ok(LoopOutcome::Cancelled);
                }
                TurnWait::TimeUp => {
                    turn_cancel.cancel();
                    finish_partial_batch(session, &assistant_calls, sink, &batch_checkpoint);
                    return Ok(LoopOutcome::TimeUp);
                }
            };
            for (call_id, tool, result, duration, _artifacts) in results {
                push_turn(
                    session,
                    sink,
                    Turn::ToolResult {
                        call_id,
                        tool,
                        result,
                        duration,
                    },
                );
            }
            if investigation_batch {
                consecutive_investigation_rounds =
                    consecutive_investigation_rounds.saturating_add(1);
            } else {
                consecutive_investigation_rounds = 0;
            }
        }
    }
}

enum TurnWait<T> {
    Ready(T),
    Cancelled,
    TimeUp,
}

async fn await_turn_budget<T>(
    future: impl Future<Output = T>,
    deadline: Option<tokio::time::Instant>,
    cancel: &CancellationToken,
) -> TurnWait<T> {
    match deadline {
        Some(deadline) => {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => TurnWait::Cancelled,
                _ = tokio::time::sleep_until(deadline) => TurnWait::TimeUp,
                output = future => TurnWait::Ready(output),
            }
        }
        None => {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => TurnWait::Cancelled,
                output = future => TurnWait::Ready(output),
            }
        }
    }
}

fn push_turn(session: &mut Session, sink: &dyn EventSink, turn: Turn) {
    session.messages.push(turn);
    if let Some(turn) = session.messages.last() {
        sink.on_turn(turn);
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
fn synthesize_interrupted(session: &mut Session, calls: &[ToolCall], sink: &dyn EventSink) {
    for c in calls {
        sink.on_tool_end(&c.id, &c.name, &ToolResultBody::Interrupted);
        push_turn(
            session,
            sink,
            Turn::ToolResult {
                call_id: c.id.clone(),
                tool: c.name.clone(),
                result: ToolResultBody::Interrupted,
                duration: Duration::ZERO,
            },
        );
    }
}

/// Pair tool calls from an unconfirmed provider response with retryable errors.
/// The next loop iteration exposes these results to the model, which can safely
/// reissue the full call. Keeping this distinct from `Interrupted` is important:
/// `Interrupted` is terminal UI state, while this condition is recoverable.
fn synthesize_retryable_unconfirmed(
    session: &mut Session,
    calls: &[ToolCall],
    sink: &dyn EventSink,
    finish_reason: &str,
) {
    for c in calls {
        let result = ToolResultBody::Error {
            message: format!(
                "the model response ended with finish_reason={finish_reason} before confirming \
                 this tool call; it was not executed. Retry the tool call with complete arguments"
            ),
            retryable: true,
        };
        sink.on_tool_end(&c.id, &c.name, &result);
        push_turn(
            session,
            sink,
            Turn::ToolResult {
                call_id: c.id.clone(),
                tool: c.name.clone(),
                result,
                duration: Duration::ZERO,
            },
        );
    }
}

enum ExecItem {
    Call(ToolCall),
    ParseError {
        call_id: String,
        tool_name: String,
        error: String,
    },
}

/// A pending parallel tool task: its batch index, call id/name, and the
/// `tokio::spawn` handle. Kept outside the task so a panicking tool can still
/// be mapped back to a well-formed error result (see `execute_batch`).
type ParallelTask = (usize, String, String, AbortOnDropHandle<ToolExecResult>);

/// Tokio detaches a `JoinHandle` when it is dropped. A turn deadline must not
/// leave tool calls running after the agent has returned, so batch-owned tasks
/// abort automatically if the batch future is cancelled or times out.
struct AbortOnDropHandle<T>(tokio::task::JoinHandle<T>);

impl<T> AbortOnDropHandle<T> {
    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        (&mut self.0).await
    }
}

impl<T> Drop for AbortOnDropHandle<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct ScheduledCall {
    tool: Option<Arc<dyn Tool>>,
    call: ToolCall,
    concurrency: Concurrency,
}

type ToolExecResult = (
    String,
    String,
    ToolResultBody,
    Duration,
    Vec<crate::tool::Artifact>,
);

#[derive(Clone)]
struct BatchCheckpoint {
    results: Vec<Option<ToolExecResult>>,
    emitted: HashSet<usize>,
}

impl BatchCheckpoint {
    fn new(len: usize) -> Self {
        Self {
            results: std::iter::repeat_with(|| None).take(len).collect(),
            emitted: HashSet::new(),
        }
    }
}

type SharedBatchCheckpoint = Arc<Mutex<BatchCheckpoint>>;

struct BatchExecutor<'a> {
    tools: &'a ToolRegistry,
    ctx: &'a ToolCtx,
    permission: &'a dyn PermissionChecker,
    prompter: &'a dyn Prompter,
    sink: &'a dyn EventSink,
    hard_result_cap: usize,
    checkpoint: SharedBatchCheckpoint,
}

/// Execute a batch of tool calls per the concurrency policy (§4.3), after a
/// per-call permission check (§7). Denied/Ask-denied calls become denied tool
/// results without running; Ask-approved calls run (a Session grant is added to
/// `grants`). Results are returned in the original tool-call order.
async fn execute_batch(
    items: &[ExecItem],
    grants: &mut Vec<String>,
    executor: BatchExecutor<'_>,
) -> Vec<ToolExecResult> {
    let BatchExecutor {
        tools,
        ctx,
        permission,
        prompter,
        sink,
        hard_result_cap,
        checkpoint,
    } = executor;
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
    let mut results: HashMap<usize, ToolExecResult> = HashMap::new();
    let mut emitted: HashSet<usize> = HashSet::new();
    let mut order: Vec<usize> = Vec::new();
    let mut scheduled: Vec<Option<ScheduledCall>> =
        std::iter::repeat_with(|| None).take(items.len()).collect();

    for (i, item) in items.iter().enumerate() {
        order.push(i);
        match item {
            ExecItem::Call(call) => match tools.get(&call.name) {
                Some(tool) => {
                    scheduled[i] = Some(ScheduledCall {
                        tool: Some(tool.clone()),
                        call: call.clone(),
                        concurrency: tool.concurrency(),
                    });
                }
                None => {
                    scheduled[i] = Some(ScheduledCall {
                        tool: None,
                        call: call.clone(),
                        concurrency: Concurrency::Exclusive,
                    });
                }
            },
            ExecItem::ParseError {
                call_id,
                tool_name,
                error,
            } => {
                record_batch_result(
                    &mut results,
                    &checkpoint,
                    i,
                    (
                        call_id.clone(),
                        tool_name.clone(),
                        ToolResultBody::Error {
                            message: format!("invalid tool arguments: {error}"),
                            retryable: true,
                        },
                        Duration::ZERO,
                        Vec::new(),
                    ),
                );
            }
        }
    }

    // Preserve the model's effect order. Contiguous pure reads form a bounded
    // parallel phase; every write or exclusive call is an ordered barrier.
    // Previously all reads in the response ran before all writes, so a model
    // batch of `Write(path) -> Read(path)` observed the old file.
    let mut cursor = 0usize;
    while cursor < scheduled.len() {
        let Some(next) = scheduled[cursor].as_ref() else {
            cursor += 1;
            continue;
        };
        if next.concurrency == Concurrency::Parallel {
            let phase_start = cursor;
            let mut phase = Vec::new();
            while cursor < scheduled.len() {
                match scheduled[cursor].as_ref() {
                    Some(call) if call.concurrency == Concurrency::Parallel => {
                        let call = scheduled[cursor].take().expect("parallel call present");
                        phase.push((
                            cursor,
                            call.tool.expect("parallel calls have a registered tool"),
                            call.call,
                        ));
                        cursor += 1;
                    }
                    None => cursor += 1,
                    Some(_) => break,
                }
            }
            debug_assert!(cursor > phase_start);
            let live_ctx = refresh_tool_ctx(&ctx);
            let mut allowed_phase = Vec::with_capacity(phase.len());
            for (index, tool, call) in phase {
                if let Some(result) =
                    authorize_call(&call, &live_ctx, permission, prompter, grants).await
                {
                    emit_tool_completion(sink, &result);
                    emitted.insert(index);
                    mark_batch_emitted(&checkpoint, index);
                    record_batch_result(&mut results, &checkpoint, index, result);
                } else {
                    allowed_phase.push((index, tool, call));
                }
            }
            run_parallel_phase(
                allowed_phase,
                &live_ctx,
                sink,
                hard_result_cap,
                &mut results,
                &mut emitted,
                &checkpoint,
            )
            .await;
        } else {
            let call = scheduled[cursor].take().expect("barrier call present");
            let live_ctx = refresh_tool_ctx(&ctx);
            if let Some(result) =
                authorize_call(&call.call, &live_ctx, permission, prompter, grants).await
            {
                emit_tool_completion(sink, &result);
                emitted.insert(cursor);
                mark_batch_emitted(&checkpoint, cursor);
                record_batch_result(&mut results, &checkpoint, cursor, result);
                cursor += 1;
                continue;
            }
            let Some(tool) = call.tool else {
                let result = (
                    call.call.id.clone(),
                    call.call.name.clone(),
                    ToolResultBody::Error {
                        message: format!("unknown tool: {}", call.call.name),
                        retryable: false,
                    },
                    Duration::ZERO,
                    Vec::new(),
                );
                emit_tool_completion(sink, &result);
                emitted.insert(cursor);
                mark_batch_emitted(&checkpoint, cursor);
                record_batch_result(&mut results, &checkpoint, cursor, result);
                cursor += 1;
                continue;
            };
            let r = cap_tool_result(run_one(tool, call.call, live_ctx).await, hard_result_cap);
            emit_tool_completion(sink, &r);
            emitted.insert(cursor);
            mark_batch_emitted(&checkpoint, cursor);
            record_batch_result(&mut results, &checkpoint, cursor, r);
            cursor += 1;
        }
    }

    // Permission denials, parse failures, and unknown tools are terminal before
    // execution starts. Publish them too; executed entries were already sent
    // above.
    for (i, r) in &results {
        if !emitted.contains(i) {
            emit_tool_completion(sink, r);
            mark_batch_emitted(&checkpoint, *i);
        }
    }

    order
        .into_iter()
        .map(|i| results.remove(&i).expect("a result for every batch item"))
        .collect()
}

/// Authorize immediately before execution, using the cwd produced by every
/// earlier ordered barrier. This prevents a same-batch `Bash(cd ...)` followed
/// by a relative edit from being approved against the stale pre-`cd` path.
/// `None` means allowed; `Some` is the terminal denied result.
async fn authorize_call(
    call: &ToolCall,
    ctx: &ToolCtx,
    permission: &dyn PermissionChecker,
    prompter: &dyn Prompter,
    grants: &mut Vec<String>,
) -> Option<ToolExecResult> {
    let input: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let denied = |reason| {
        Some((
            call.id.clone(),
            call.name.clone(),
            ToolResultBody::Denied { reason },
            Duration::ZERO,
            Vec::new(),
        ))
    };
    match permission.check(&call.name, &input, &ctx.cwd, &ctx.allowed_roots, grants) {
        Decision::Allow => None,
        Decision::Deny(reason) => denied(reason),
        Decision::Ask(reason) => match prompter.ask(&call.name, &input, &reason).await {
            AskResponse::Once => None,
            AskResponse::Session(rule) | AskResponse::Always(rule) => {
                grants.push(rule);
                None
            }
            AskResponse::Deny(reason) => denied(reason),
        },
    }
}

async fn run_parallel_phase(
    phase: Vec<(usize, Arc<dyn Tool>, ToolCall)>,
    ctx: &ToolCtx,
    sink: &dyn EventSink,
    hard_result_cap: usize,
    results: &mut HashMap<usize, ToolExecResult>,
    emitted: &mut HashSet<usize>,
    checkpoint: &SharedBatchCheckpoint,
) {
    // Each call runs in an isolated task so a tool-implementation panic becomes
    // a well-formed result. Abort-on-drop prevents detached work after a turn
    // timeout cancels this phase.
    let sem = Arc::new(Semaphore::new(PARALLEL_BOUND));
    let live_ctx = refresh_tool_ctx(ctx);
    let mut handles: Vec<ParallelTask> = Vec::with_capacity(phase.len());
    for (i, tool, call) in phase {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let ctx = live_ctx.clone();
        let call_id = call.id.clone();
        let tool_name = call.name.clone();
        let handle = tokio::spawn(async move {
            let r = run_one(tool, call, ctx).await;
            drop(permit);
            r
        });
        handles.push((i, call_id, tool_name, AbortOnDropHandle(handle)));
    }
    for (i, call_id, tool_name, handle) in handles {
        let r = match handle.join().await {
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
                    Vec::new(),
                )
            }
        };
        let r = cap_tool_result(r, hard_result_cap);
        emit_tool_completion(sink, &r);
        emitted.insert(i);
        mark_batch_emitted(checkpoint, i);
        record_batch_result(results, checkpoint, i, r);
    }
}

fn record_batch_result(
    results: &mut HashMap<usize, ToolExecResult>,
    checkpoint: &SharedBatchCheckpoint,
    index: usize,
    result: ToolExecResult,
) {
    if let Ok(mut checkpoint) = checkpoint.lock() {
        checkpoint.results[index] = Some(result.clone());
    }
    results.insert(index, result);
}

fn mark_batch_emitted(checkpoint: &SharedBatchCheckpoint, index: usize) {
    if let Ok(mut checkpoint) = checkpoint.lock() {
        checkpoint.emitted.insert(index);
    }
}

fn finish_partial_batch(
    session: &mut Session,
    calls: &[ToolCall],
    sink: &dyn EventSink,
    checkpoint: &SharedBatchCheckpoint,
) {
    let checkpoint = checkpoint
        .lock()
        .map(|checkpoint| checkpoint.clone())
        .unwrap_or_else(|_| BatchCheckpoint::new(calls.len()));
    for (index, call) in calls.iter().enumerate() {
        if let Some((call_id, tool, result, duration, _)) =
            checkpoint.results.get(index).and_then(Clone::clone)
        {
            if !checkpoint.emitted.contains(&index) {
                sink.on_tool_end(&call_id, &tool, &result);
            }
            push_turn(
                session,
                sink,
                Turn::ToolResult {
                    call_id,
                    tool,
                    result,
                    duration,
                },
            );
        } else {
            sink.on_tool_end(&call.id, &call.name, &ToolResultBody::Interrupted);
            push_turn(
                session,
                sink,
                Turn::ToolResult {
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    result: ToolResultBody::Interrupted,
                    duration: Duration::ZERO,
                },
            );
        }
    }
}

fn refresh_tool_ctx(ctx: &ToolCtx) -> ToolCtx {
    let mut live = ctx.clone();
    if let Ok(shell) = live.shell_state.lock() {
        live.cwd = shell.cwd.clone();
    }
    live
}

/// Run one tool call: invoke, map the outcome, time it. (Permission was already
/// checked by the caller; this only runs Allow'd calls.)
async fn run_one(tool: Arc<dyn Tool>, call: ToolCall, ctx: ToolCtx) -> ToolExecResult {
    let start = SystemTime::now();
    let input: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let outcome = match tool.call(input, &ctx).await {
        Ok(o) => o,
        Err(e) => ToolOutcome::Error {
            message: e.to_string(),
            retryable: false,
        },
    };
    let dur = start.elapsed().unwrap_or(Duration::ZERO);
    let (result, artifacts) = match outcome {
        ToolOutcome::Ok {
            content,
            truncated,
            artifacts,
        } => (
            ToolResultBody::Ok {
                content: Arc::from(content),
                truncated,
            },
            artifacts,
        ),
        ToolOutcome::Denied { reason } => (ToolResultBody::Denied { reason }, Vec::new()),
        ToolOutcome::Error { message, retryable } => {
            (ToolResultBody::Error { message, retryable }, Vec::new())
        }
        ToolOutcome::Interrupted => (ToolResultBody::Interrupted, Vec::new()),
    };
    (call.id, call.name, result, dur, artifacts)
}

fn emit_tool_completion(sink: &dyn EventSink, completed: &ToolExecResult) {
    let (call_id, tool, result, _, artifacts) = completed;
    for artifact in artifacts {
        sink.on_artifact(call_id, tool, artifact);
    }
    sink.on_tool_end(call_id, tool, result);
}

fn cap_tool_result(mut completed: ToolExecResult, cap: usize) -> ToolExecResult {
    // Hard runaway backstop: cap the model-facing and host-visible result in
    // one place. File-change artifacts remain exact and never enter context.
    if cap > 0 {
        completed.2 = completed.2.truncate_body(cap);
    }
    completed
}
