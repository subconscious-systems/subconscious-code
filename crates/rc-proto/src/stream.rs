//! Streaming: SSE decode, tool-call accumulation, and event fusion (§3.3).
//!
//! The wire boundary for a streaming `/v1/chat/completions` response. Three
//! layers, each unit-testable:
//!   1. [`SseDecoder`] — incremental bytes → [`ChatCompletionChunk`] parsing.
//!   2. [`ToolCallAccumulator`] — index-keyed reassembly of fragmented tool
//!      calls (§3.3): id once, name once, args appended, empty = `{}`.
//!   3. [`StreamFuser`] — chunks → [`AgentStreamEvent`], flushing finalized
//!      calls (parsed, or — on unrecoverable JSON — reported for the loop to
//!      synthesize a `role:tool` error result, §3.3).
//!
//! The shattered-JSON property test (below) feeds a tool call's args through
//! random chunk boundaries and asserts exact reassembly — the single highest-
//! value test for this layer (§13). On valid JSON the accumulator preserves the
//! model's *exact* argument bytes (no re-serialization), which is what keeps
//! the assistant message prefix cacheable (§4.6).

use crate::error::ProtoError;
use crate::wire::Usage;
use serde::Deserialize;

// ---- chunk / delta types ----------------------------------------------------

/// One SSE chunk. `choices` is empty on the trailing usage-only chunk (§3.6) —
/// never assume `choices[0]` exists.
#[derive(Deserialize, Debug, Clone)]
pub struct ChatCompletionChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Delta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    // Reasoning: two field names in the wild (§3.4).
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// ---- finish reason ----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    /// Any unrecognized value; the loop treats unknown reasons as terminal.
    Other(String),
}

impl FinishReason {
    pub fn parse(s: &str) -> Self {
        match s {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::Length,
            "tool_calls" => FinishReason::ToolCalls,
            "content_filter" => FinishReason::ContentFilter,
            other => FinishReason::Other(other.to_string()),
        }
    }
}

// ---- fused events (what the agent loop consumes) ----------------------------

/// High-level events the agent loop reacts to. Tool calls arrive *assembled*
/// (parsed) or as a parse error the loop turns into a `role:tool` error result.
#[derive(Debug, Clone)]
pub enum AgentStreamEvent {
    /// Assistant text delta.
    Text(String),
    /// Reasoning delta (field mode; §3.4). Tag-mode reasoning is split out of
    /// `Text` by rc-core before persistence.
    Reasoning(String),
    /// A tool call whose arguments parsed. `arguments` is the model's (or
    /// repaired) argument JSON *string* — preserved verbatim so the assistant
    /// message re-sent next turn is byte-identical (§4.6).
    ToolCallReady {
        id: String,
        name: String,
        arguments: String,
    },
    /// A tool call whose arguments could not be repaired/parsed (§3.3).
    ToolCallFailed {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        raw_arguments: String,
        error: String,
    },
    /// The model finished. Emitted exactly once per stream.
    Finish {
        reason: FinishReason,
    },
    /// Token usage (the trailing chunk, §3.6).
    Usage(Usage),
}

// ---- tool-call accumulation (§3.3) -----------------------------------------

#[derive(Default)]
pub struct ToolCallAccumulator {
    slots: Vec<PartialToolCall>,
}

#[derive(Default, Clone)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

/// A tool call after accumulation: parsed (with the original bytes preserved),
/// or reported as a parse error.
#[derive(Debug, Clone)]
pub enum FinalizedToolCall {
    Ok {
        id: String,
        name: String,
        arguments: String,
    },
    ParseError {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        raw_arguments: String,
        error: String,
    },
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a streamed tool-call delta. Index-keyed (deltas for calls 0 and 1
    /// interleave); `id`/`name` use `get_or_insert` so a repeat or late-arriving
    /// id/name is tolerated; `arguments` fragments are appended.
    pub fn apply(&mut self, d: &ToolCallDelta) {
        let i = d.index as usize;
        if self.slots.len() <= i {
            self.slots.resize(i + 1, Default::default());
        }
        let s = &mut self.slots[i];
        if let Some(id) = &d.id {
            s.id.get_or_insert_with(|| id.clone());
        }
        if let Some(f) = &d.function {
            if let Some(n) = &f.name {
                s.name.get_or_insert_with(|| n.clone());
            }
            if let Some(a) = &f.arguments {
                s.args.push_str(a);
            }
        }
    }

    /// Finalize every accumulated call. Empty args become `{}`. Valid JSON is
    /// preserved byte-for-byte (cache, §4.6); malformed JSON is repaired, and
    /// on unrecoverable failure reported as a `ParseError` for the loop to feed
    /// back to the model (§3.3).
    ///
    /// `confirmed` is whether the model signalled completion (`finish_reason ==
    /// "tool_calls"`). When unconfirmed (a cut stream — no finish, `length`,
    /// `content_filter`, …) repair-fabricated args are not trusted: the call is
    /// reported as a `ParseError` carrying the *raw* bytes rather than `Ok` with
    /// a repair-completed (possibly wrong) value (§3.3 / F1). Already-valid JSON
    /// is kept either way — a dropped finish_reason chunk after a complete call
    /// must not be a false negative.
    pub fn finish_confirmed(self, confirmed: bool) -> Vec<FinalizedToolCall> {
        let mut out = Vec::with_capacity(self.slots.len());
        for (i, s) in self.slots.into_iter().enumerate() {
            let id = s.id.unwrap_or_else(|| format!("call_{}", i));
            let name = s.name.unwrap_or_default();
            let raw = if s.args.is_empty() { "{}".to_string() } else { s.args };
            out.push(finalize_one(i, id, name, raw, confirmed));
        }
        out
    }

    /// Finalize assuming the model completed the calls (the historical default).
    /// Direct callers (tests) keep this; the [`StreamFuser`] knows the real
    /// `finish_reason` and calls [`finish_confirmed`](Self::finish_confirmed).
    pub fn finish(self) -> Vec<FinalizedToolCall> {
        self.finish_confirmed(true)
    }
}

fn finalize_one(index: usize, id: String, name: String, raw: String, confirmed: bool) -> FinalizedToolCall {
    // Fast path: already valid → preserve the model's exact bytes. Holds even
    // when unconfirmed: a dropped finish_reason chunk after a complete call must
    // not be a false negative.
    if serde_json::from_str::<serde_json::Value>(&raw).is_ok() {
        return FinalizedToolCall::Ok { id, name, arguments: raw };
    }
    // Repair and retry. Only trust the repaired bytes when the model signalled
    // completion (`tool_calls`); otherwise the args were likely truncated by a
    // cut stream — refuse to run on repair-fabricated values (§3.3 / F1).
    let repaired = repair(&raw);
    match serde_json::from_str::<serde_json::Value>(&repaired) {
        Ok(_) if confirmed => FinalizedToolCall::Ok { id, name, arguments: repaired },
        Ok(_) => FinalizedToolCall::ParseError {
            index,
            id: Some(id),
            name: Some(name),
            raw_arguments: raw,
            error: "arguments incomplete (stream ended before tool_calls finish_reason)".to_string(),
        },
        Err(e) => FinalizedToolCall::ParseError {
            index,
            id: Some(id),
            name: Some(name),
            raw_arguments: raw,
            error: format!("malformed tool arguments: {e}"),
        },
    }
}

// ---- JSON repair (best-effort, §3.3) ----------------------------------------

/// Parse tool-argument JSON, repairing common malformations (trailing commas,
/// unterminated strings, unbalanced braces) first. Returns the parsed value
/// or an error message for the loop to feed back to the model.
pub fn repair_json(input: &str) -> Result<serde_json::Value, String> {
    if input.is_empty() {
        return Ok(serde_json::json!({}));
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
        return Ok(v);
    }
    let repaired = repair(input);
    serde_json::from_str::<serde_json::Value>(&repaired)
        .map_err(|e| format!("malformed tool arguments: {e}"))
}

/// Best-effort repair of truncated/malformed JSON (§3.3): string-aware
/// (won't touch commas inside string values), strip trailing commas before
/// `}`/`]`/EOF, close unterminated strings, balance braces/brackets.
pub fn repair(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 4);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                stack.push(c);
                out.push(c);
            }
            '}' | ']' => {
                stack.pop();
                out.push(c);
            }
            ',' => {
                // Trailing comma? Skip if the next non-whitespace is }, ], or EOF.
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                let trailing = j >= chars.len() || chars[j] == '}' || chars[j] == ']';
                if !trailing {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
        i += 1;
    }
    if in_string {
        out.push('"');
    }
    while let Some(open) = stack.pop() {
        out.push(match open {
            '{' => '}',
            '[' => ']',
            other => other,
        });
    }
    out
}

// ---- SSE decoder ------------------------------------------------------------

/// Incremental Server-Sent Events decoder. Feed bytes as they arrive; complete
/// `data:` lines yield parsed chunks. `data: [DONE]` sets [`SseDecoder::is_done`].
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    done: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Feed a chunk of bytes; return any complete `data:` lines parsed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<ChatCompletionChunk, ProtoError>> {
        self.buf.extend_from_slice(bytes);
        self.drain_lines()
    }

    /// Flush any remaining buffered bytes as a final line at stream end.
    pub fn finish(&mut self) -> Vec<Result<ChatCompletionChunk, ProtoError>> {
        if self.buf.is_empty() {
            return vec![];
        }
        let line = std::mem::take(&mut self.buf);
        self.parse_line(&line)
    }

    fn drain_lines(&mut self) -> Vec<Result<ChatCompletionChunk, ProtoError>> {
        let mut out = vec![];
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            out.extend(self.parse_line(&line));
        }
        out
    }

    fn parse_line(&mut self, line: &[u8]) -> Vec<Result<ChatCompletionChunk, ProtoError>> {
        let mut s = line;
        while matches!(s.last(), Some(b'\n') | Some(b'\r')) {
            s = &s[..s.len() - 1];
        }
        if s.is_empty() {
            return vec![];
        }
        if s.first() == Some(&b':') {
            return vec![]; // comment / ping
        }
        let Some(rest) = s.strip_prefix(b"data:") else {
            return vec![]; // event:, id:, retry: — ignored for M1
        };
        let rest = rest.strip_prefix(b" ").unwrap_or(rest);
        let rest_cow = String::from_utf8_lossy(rest);
        let rest_str = rest_cow.trim();
        if rest_str.is_empty() {
            return vec![];
        }
        if rest_str == "[DONE]" {
            self.done = true;
            return vec![];
        }
        // Deep-debug only (RUST_LOG=rc_proto=trace): the raw SSE data payload.
        tracing::trace!("data: {rest_str}");
        vec![serde_json::from_str::<ChatCompletionChunk>(rest_str).map_err(ProtoError::Json)]
    }
}

// ---- stream fuser (chunks -> events) ----------------------------------------

/// Stateful reducer from [`ChatCompletionChunk`]s to [`AgentStreamEvent`]s:
/// accumulates text/reasoning and reassembles tool calls, emitting them
/// (assembled) when `finish_reason` arrives or the stream ends.
#[derive(Default)]
pub struct StreamFuser {
    acc: ToolCallAccumulator,
    finished: bool,
}

impl StreamFuser {
    pub fn new() -> Self {
        Self { acc: ToolCallAccumulator::new(), finished: false }
    }

    /// Apply a chunk; return any events it produced.
    pub fn apply(&mut self, chunk: ChatCompletionChunk) -> Vec<AgentStreamEvent> {
        let mut out = vec![];
        if let Some(u) = chunk.usage {
            out.push(AgentStreamEvent::Usage(u));
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
                out.push(AgentStreamEvent::Text(text));
            }
            if let Some(r) = choice
                .delta
                .reasoning_content
                .or(choice.delta.reasoning)
                .filter(|r| !r.is_empty())
            {
                out.push(AgentStreamEvent::Reasoning(r));
            }
            for tc in &choice.delta.tool_calls {
                self.acc.apply(tc);
            }
            if let Some(fr) = choice.finish_reason {
                self.flush(&mut out, Some(&fr));
            }
        }
        out
    }

    /// Flush at stream end if no `finish_reason` was seen. The loop treats the
    /// resulting `Other("stream-ended")` as terminal.
    pub fn finish(&mut self) -> Vec<AgentStreamEvent> {
        let mut out = vec![];
        self.flush(&mut out, None);
        out
    }

    fn flush(&mut self, out: &mut Vec<AgentStreamEvent>, reason: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Move the accumulator out without re-borrowing it across the iterator.
        let acc = std::mem::take(&mut self.acc);
        // Only `tool_calls` means the model deliberately completed the calls;
        // anything else (None / length / content_filter / unknown) may be a cut
        // stream — don't trust repair-fabricated args (§3.3 / F1).
        let confirmed = reason == Some("tool_calls");
        for fc in acc.finish_confirmed(confirmed) {
            match fc {
                FinalizedToolCall::Ok { id, name, arguments } => {
                    out.push(AgentStreamEvent::ToolCallReady { id, name, arguments })
                }
                FinalizedToolCall::ParseError { index, id, name, raw_arguments, error } => {
                    out.push(AgentStreamEvent::ToolCallFailed {
                        index,
                        id,
                        name,
                        raw_arguments,
                        error,
                    })
                }
            }
        }
        out.push(AgentStreamEvent::Finish {
            reason: match reason {
                Some(r) => FinishReason::parse(r),
                None => FinishReason::Other("stream-ended".to_string()),
            },
        });
    }
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct XorShift(u64);
    impl XorShift {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn range(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }
        fn bool(&mut self) -> bool {
            self.next() & 1 == 1
        }
    }

    fn random_json(rng: &mut XorShift) -> String {
        let n = (rng.range(3) + 1) as usize; // 1..=3 fields
        let keys = ["a", "b", "c", "d", "e", "f"];
        let mut s = String::from("{");
        for i in 0..n {
            if i > 0 {
                s.push(',');
            }
            let k = keys[rng.range(keys.len() as u64) as usize];
            let v = match rng.range(3) {
                0 => format!("\"v{}\"", rng.range(50)),
                1 => format!("{}", rng.range(1000)),
                _ => if rng.bool() { "true".into() } else { "false".into() },
            };
            s.push_str(&format!("\"{k}\":{v}"));
        }
        s.push('}');
        s
    }

    /// Shard a string into 1..=len+4 fragments at random positions (some may
    /// be empty). Concatenation always equals the input.
    fn shard(s: &str, rng: &mut XorShift) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len();
        let n = (rng.range(len as u64 + 4) + 1) as usize; // at least 1
        let mut cuts: Vec<usize> = (0..n.saturating_sub(1))
            .map(|_| rng.range(len as u64 + 1) as usize)
            .collect();
        cuts.sort_unstable();
        let mut out = Vec::new();
        let mut prev = 0;
        for c in cuts {
            out.push(chars[prev..c].iter().collect::<String>());
            prev = c;
        }
        out.push(chars[prev..].iter().collect::<String>());
        out
    }

    /// §13: the single highest-value test. Shatter a tool call's JSON across
    /// random chunk boundaries (with id sometimes late/duplicated) and assert
    /// the accumulator reassembles it exactly — preserving the model's bytes.
    #[test]
    fn accumulator_reassembles_shattered_json() {
        let mut rng = XorShift::new(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2000 {
            let id = format!("call_{}", rng.range(100));
            let name = ["Read", "Edit", "Bash"][rng.range(3) as usize];
            let args = random_json(&mut rng);

            let fragments = shard(&args, &mut rng);
            let send_dup = rng.range(5) == 0;
            let id_at_start = send_dup || rng.bool();
            let id_at_last = send_dup || !id_at_start;

            let mut acc = ToolCallAccumulator::new();
            // chunk 0 carries the name (and maybe the id).
            acc.apply(&ToolCallDelta {
                index: 0,
                id: if id_at_start { Some(id.clone()) } else { None },
                function: Some(FunctionDelta { name: Some(name.to_string()), arguments: None }),
            });
            let last = fragments.len().saturating_sub(1);
            for (i, frag) in fragments.iter().enumerate() {
                let carry_id = i == last && id_at_last;
                acc.apply(&ToolCallDelta {
                    index: 0,
                    id: if carry_id { Some(id.clone()) } else { None },
                    function: Some(FunctionDelta { name: None, arguments: Some(frag.clone()) }),
                });
            }

            let finalized = acc.finish();
            assert_eq!(finalized.len(), 1, "one call in, one out");
            match &finalized[0] {
                FinalizedToolCall::Ok { id: got_id, name: got_name, arguments } => {
                    assert_eq!(got_id, &id, "id reassembled (late/dup tolerated)");
                    assert_eq!(got_name, name, "name reassembled");
                    assert_eq!(arguments, &args, "argument bytes preserved exactly");
                }
                FinalizedToolCall::ParseError { .. } => {
                    panic!("valid args failed to parse: {args}");
                }
            }
        }
    }

    #[test]
    fn empty_arguments_become_empty_object() {
        let mut acc = ToolCallAccumulator::new();
        acc.apply(&ToolCallDelta {
            index: 0,
            id: Some("c".into()),
            function: Some(FunctionDelta { name: Some("Read".into()), arguments: None }),
        });
        let f = acc.finish();
        match &f[0] {
            FinalizedToolCall::Ok { arguments, .. } => assert_eq!(arguments, "{}"),
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn repair_fixes_trailing_commas_and_unbalanced_braces() {
        assert_eq!(repair(r#"{"a":1,}"#), r#"{"a":1}"#);
        assert_eq!(repair(r#"{"a":{"b":2,"#), r#"{"a":{"b":2}}"#);
        // commas/braces inside strings are left alone; the in-string `}` does
        // not close the object, so repair adds one real closing brace.
        assert_eq!(repair(r#"{"a":"x,}"#), "{\"a\":\"x,}\"}");
        assert!(repair_json(r#"{"a":1, "b":2,}"#).is_ok());
        assert!(repair_json(r#"{"a":"unterminated"#).is_ok());
    }

    #[test]
    fn sse_decoder_handles_split_and_done() {
        let mut dec = SseDecoder::new();
        // Feed a complete event split across two feed calls.
        let a = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n";
        let (first, second) = a.split_at(a.len() / 2);
        let out1 = dec.feed(first);
        assert!(out1.is_empty(), "partial line yields nothing");
        let out2 = dec.feed(second);
        assert_eq!(out2.len(), 1);
        let chunk = out2[0].as_ref().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));

        // [DONE] and a usage-only chunk.
        let out3 = dec.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n");
        assert_eq!(out3.len(), 1);
        assert!(!dec.is_done(), "usage chunk is not [DONE]");
        let out4 = dec.feed(b"data: [DONE]\n\n");
        assert!(out4.is_empty());
        assert!(dec.is_done(), "[DONE] sets done");
    }

    #[test]
    fn fuser_assembles_tool_calls_and_finishes() {
        let mut f = StreamFuser::new();
        let c1 = serde_json::from_str::<ChatCompletionChunk>(
            r#"{"choices":[{"index":0,"delta":{"content":"he"}}]}"#,
        )
        .unwrap();
        let c2 = serde_json::from_str::<ChatCompletionChunk>(
            r#"{"choices":[{"index":0,"delta":{"content":"llo"}}]}"#,
        )
        .unwrap();
        let c3 = serde_json::from_str::<ChatCompletionChunk>(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        let mut text = String::new();
        for chunk in [c1, c2, c3] {
            for ev in f.apply(chunk) {
                match ev {
                    AgentStreamEvent::Text(t) => text.push_str(&t),
                    AgentStreamEvent::Finish { reason } => assert_eq!(reason, FinishReason::Stop),
                    _ => {}
                }
            }
        }
        assert_eq!(text, "hello");
    }

    // ---- completion-gated finalize (§3.3 / F1) ---------------------------------

    /// Helper: build a one-call chunk with the given (possibly malformed) args.
    /// Constructed directly (not via JSON interpolation) so `args` may contain
    /// raw quotes / braces without breaking the envelope.
    fn call_chunk(id: &str, name: &str, args: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: String::new(),
            model: String::new(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta {
                    tool_calls: vec![ToolCallDelta {
                        index: 0,
                        id: Some(id.to_string()),
                        function: Some(FunctionDelta {
                            name: Some(name.to_string()),
                            arguments: Some(args.to_string()),
                        }),
                    }],
                    ..Delta::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }
    fn finish_chunk(reason: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: String::new(),
            model: String::new(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some(reason.to_string()),
            }],
            usage: None,
        }
    }

    #[test]
    fn truncated_args_without_finish_reason_become_parse_error() {
        // A cut stream: args truncated (unterminated string), NO finish_reason.
        // repair() would fabricate `{"file":"/tmp/fo"}` (valid, wrong) — refuse it.
        let mut f = StreamFuser::new();
        for _ in f.apply(call_chunk("c1", "Read", r#"{"file":"/tmp/fo"#)) {}
        let evs = f.finish(); // no finish_reason → Other("stream-ended")
        assert!(
            evs.iter().any(|e| matches!(e, AgentStreamEvent::ToolCallFailed { id, .. } if id.as_deref() == Some("c1"))),
            "truncated + no finish → ToolCallFailed, got {evs:?}"
        );
        assert!(
            !evs.iter().any(|e| matches!(e, AgentStreamEvent::ToolCallReady { .. })),
            "must not surface repair-fabricated args as ready, got {evs:?}"
        );
    }

    #[test]
    fn valid_args_survive_stream_end_without_finish_reason() {
        // Complete, valid args + no finish_reason → fast path → ToolCallReady.
        // A dropped finish_reason chunk after a complete call is not a false neg.
        let mut f = StreamFuser::new();
        for _ in f.apply(call_chunk("c1", "Read", r#"{"file":"/tmp/foo"}"#)) {}
        let evs = f.finish();
        assert!(
            evs.iter().any(|e| matches!(e, AgentStreamEvent::ToolCallReady { id, .. } if id == "c1")),
            "valid args + no finish → ToolCallReady, got {evs:?}"
        );
    }

    #[test]
    fn length_finish_with_invalid_args_becomes_parse_error() {
        let mut f = StreamFuser::new();
        let mut evs = Vec::new();
        evs.extend(f.apply(call_chunk("c1", "Read", r#"{"file":"/tmp/fo"#)));
        evs.extend(f.apply(finish_chunk("length"))); // flush fires here
        evs.extend(f.finish()); // no-op (already flushed)
        assert!(
            evs.iter().any(|e| matches!(e, AgentStreamEvent::ToolCallFailed { id, .. } if id.as_deref() == Some("c1"))),
            "length + truncated → ToolCallFailed, got {evs:?}"
        );
    }

    #[test]
    fn tool_calls_finish_with_malformed_args_still_repaired() {
        // `tool_calls` means the model completed the call; malformed-but-complete
        // args (trailing comma) are repaired. Current behavior preserved.
        let mut f = StreamFuser::new();
        let mut evs = Vec::new();
        evs.extend(f.apply(call_chunk("c1", "Read", r#"{"file":"x",}"#)));
        evs.extend(f.apply(finish_chunk("tool_calls")));
        evs.extend(f.finish());
        assert!(
            evs.iter().any(|e| matches!(e, AgentStreamEvent::ToolCallReady { id, .. } if id == "c1")),
            "tool_calls + malformed-but-complete → ToolCallReady (repaired), got {evs:?}"
        );
    }

    #[test]
    fn structurally_valid_truncation_on_tool_calls_is_not_caught() {
        // KNOWN RESIDUAL (pinned, not fixed): a cut landing at a JSON boundary
        // yields parseable JSON — a truncated form of the intended args. The
        // fast path passes regardless of `confirmed`, so on the `tool_calls` path
        // (where tools execute) this is NOT caught. Distrusting all fast-path
        // output would break the §3.3 repair use-case, so it is left as a known
        // limit for a future length/max_tokens-aware fix.
        let mut f = StreamFuser::new();
        let mut evs = Vec::new();
        evs.extend(f.apply(call_chunk("c1", "Bash", r#"{"command":"rm -rf /tmp/old"}"#)));
        evs.extend(f.apply(finish_chunk("tool_calls")));
        evs.extend(f.finish());
        assert!(
            evs.iter().any(|e| matches!(e, AgentStreamEvent::ToolCallReady { id, .. } if id == "c1")),
            "structurally-valid truncation is NOT caught (residual): {evs:?}"
        );
    }
}
