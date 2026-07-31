//! `sc doctor` — verify a gateway before trusting it with real work.
//!
//! This exists because the two things Subconscious Code promises are properties
//! of the *whole path*, not of this binary: an unlimited context is useless if
//! the endpoint refuses large bodies, and a working agent loop is useless if the
//! gateway doesn't do tool calls. Both are cheap to check and expensive to
//! discover halfway through a session.
//!
//! Probes, in order of what breaks most often:
//!
//! 1. **Config** — what actually resolved, including where the key came from.
//! 2. **Non-streaming** — the endpoint speaks `/chat/completions` at all.
//! 3. **Streaming** — SSE works, and `stream_options.include_usage` is honored
//!    (metering and the estimator's calibration depend on it).
//! 4. **Tool calling** — the model emits `tool_calls`. The agent loop is built
//!    on this; without it nothing else matters.
//! 5. **Body-size ceiling** (`--body-ladder`) — the largest request the gateway
//!    accepts. Opt-in, because it uploads real megabytes.

use anyhow::Result;
use rc_config::Settings;
use rc_proto::wire::{FunctionDefinition, ToolDefinition, ToolType};
use rc_proto::{ChatClient, CompleteOpts, ProtoError, WireMessage};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::StreamExt;

/// A single check's outcome, rendered as one line.
enum Status {
    Pass(String),
    Warn(String),
    Fail(String),
}

impl Status {
    fn render(&self, name: &str) -> String {
        match self {
            Status::Pass(d) => format!("  ok    {name:<22} {d}"),
            Status::Warn(d) => format!("  warn  {name:<22} {d}"),
            Status::Fail(d) => format!("  FAIL  {name:<22} {d}"),
        }
    }

    fn is_fail(&self) -> bool {
        matches!(self, Status::Fail(_))
    }
}

/// The body-size ladder, in bytes. Stops at the first rung the gateway rejects,
/// so the common case costs one small upload.
///
/// 32 MB is on the ladder deliberately: it's the cap Claude Code imposes, and
/// the point of this project is to be able to say concretely whether we clear it.
const LADDER: &[usize] = &[
    1 << 20,        // 1 MB
    10 << 20,       // 10 MB — AWS API Gateway's hard ceiling
    32 << 20,       // 32 MB — Claude Code's request cap
    100 << 20,      // 100 MB
    500 << 20,      // 500 MB
];

/// Run the doctor. Returns `Ok(false)` if any check failed, so the caller can
/// exit non-zero without treating a failed probe as a crash.
pub async fn run(settings: &Settings, body_ladder: bool) -> Result<bool> {
    println!("sc doctor — {}", settings.base_url);
    println!();
    println!("configuration");
    for line in config_lines(settings) {
        println!("{line}");
    }
    println!();

    let Some(api_key) = settings.api_key.clone() else {
        println!("connectivity");
        println!(
            "{}",
            Status::Fail("no API key — set $SC_API_KEY".into()).render("api key")
        );
        println!();
        println!("Set the key and re-run: SC_API_KEY=... sc doctor");
        return Ok(false);
    };

    // A generous but finite timeout for probes specifically. The agent itself
    // defaults to no total timeout (a large upload isn't a hung one), but a
    // doctor run should not hang indefinitely on a black-hole endpoint.
    let client = ChatClient::new(
        settings.base_url.clone(),
        api_key,
        settings.model.clone(),
        Some(Duration::from_secs(120)),
    )?
    .with_request_gzip(settings.request_gzip);

    println!("connectivity");
    let mut failed = false;

    let s = probe_non_streaming(&client).await;
    failed |= s.is_fail();
    println!("{}", s.render("non-streaming"));

    let s = probe_streaming(&client).await;
    failed |= s.is_fail();
    println!("{}", s.render("streaming (SSE)"));

    let s = probe_tool_calls(&client).await;
    failed |= s.is_fail();
    println!("{}", s.render("tool calling"));

    if body_ladder {
        println!();
        println!("request-size ceiling");
        println!();
        println!("  per-string  (one message — the model server's per-JSON-string limit):");
        let single = probe_body_ladder(&client).await;
        for (size, status) in &single {
            println!("{}", status.render(&human(*size)));
        }
        println!();
        println!("  per-request (payload spread across 256 KB messages — the proxy/total limit):");
        let chunked = probe_chunked_ladder(&client).await;
        for (size, status) in &chunked {
            println!("{}", status.render(&human(*size)));
        }
        println!();
        summarize_ceilings(&single, &chunked);
    } else {
        println!();
        println!("Run with --body-ladder to measure the gateway's maximum request size.");
        println!("That ceiling is what actually bounds the context, so measure it once");
        println!("per endpoint before relying on a large one.");
    }

    Ok(!failed)
}

/// The resolved configuration, with the caps spelled out — a silent 16 KB
/// tool-result cap is exactly the kind of thing this should surface.
fn config_lines(s: &Settings) -> Vec<String> {
    let cap = |n: usize| if n == 0 { "unlimited".to_string() } else { human(n) };
    let lines = |n: u32| if n == 0 { "whole file".to_string() } else { format!("{n} lines") };
    let ms = |n: u64| if n == 0 { "off".to_string() } else { format!("{n} ms") };
    vec![
        format!("  base_url            {}", s.base_url),
        format!("  model               {}", s.model),
        format!(
            "  api key             {}",
            match &s.api_key {
                Some(k) => format!("present ({} chars)", k.len()),
                None => "MISSING".to_string(),
            }
        ),
        format!("  total timeout       {}", ms(s.timeout_ms)),
        format!("  idle timeout        {}", ms(s.idle_timeout_ms)),
        format!("  max retries         {}", s.max_retries),
        format!("  request gzip        {}", s.request_gzip),
        format!("  permission mode     {}", if s.permissions.default_mode.is_empty() {
            "default".to_string()
        } else {
            s.permissions.default_mode.clone()
        }),
        format!("  sandbox             {}", if s.sandbox.enabled {
            format!("on (net: {})", s.sandbox.allow_net)
        } else {
            "off".to_string()
        }),
        format!("  cap: tool result    {}", cap(s.context.tool_result_cap)),
        format!("  cap: @file inline   {}", cap(s.context.inline_file_cap)),
        format!("  cap: bash output    {}", cap(s.context.bash_output_cap)),
        format!("  cap: grep output    {}", cap(s.context.grep_output_cap)),
        format!("  cap: read default   {}", lines(s.context.read_default_limit)),
        format!("  cap: glob results   {}", cap(s.context.glob_cap)),
        format!("  max iterations      {}", s.context.max_iters),
    ]
}

async fn probe_non_streaming(client: &ChatClient) -> Status {
    let msgs = vec![WireMessage::User { content: "Reply with the single word: ok".into() }];
    let opts = CompleteOpts { max_tokens: Some(16), ..Default::default() };
    let t = Instant::now();
    match client.complete(&msgs, &opts).await {
        Ok(resp) => {
            let ms = t.elapsed().as_millis();
            let usage = if resp.usage.is_some() { "usage present" } else { "no usage field" };
            Status::Pass(format!("{ms} ms, {} choice(s), {usage}", resp.choices.len()))
        }
        Err(e) => Status::Fail(describe(&e)),
    }
}

async fn probe_streaming(client: &ChatClient) -> Status {
    let msgs = vec![WireMessage::User { content: "Count: 1 2 3".into() }];
    let opts = CompleteOpts { max_tokens: Some(32), ..Default::default() };
    let t = Instant::now();
    let mut stream = match client.stream(&msgs, &opts, &[]).await {
        Ok(s) => s,
        Err(e) => return Status::Fail(describe(&e)),
    };
    let mut first_chunk_ms = None;
    let mut chunks = 0usize;
    let mut saw_usage = false;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ev) => {
                chunks += 1;
                first_chunk_ms.get_or_insert(t.elapsed().as_millis());
                if format!("{ev:?}").contains("Usage") {
                    saw_usage = true;
                }
            }
            Err(e) => return Status::Fail(format!("mid-stream: {}", describe(&e))),
        }
    }
    if chunks == 0 {
        return Status::Fail("stream produced no events".into());
    }
    let ttfb = first_chunk_ms.unwrap_or(0);
    if saw_usage {
        Status::Pass(format!("{chunks} events, first at {ttfb} ms, usage reported"))
    } else {
        // Not fatal: only metering and estimator calibration degrade.
        Status::Warn(format!(
            "{chunks} events, first at {ttfb} ms — no usage chunk (stream_options.include_usage ignored; metering will be blank)"
        ))
    }
}

async fn probe_tool_calls(client: &ChatClient) -> Status {
    let tool = ToolDefinition {
        ty: ToolType::Function,
        function: FunctionDefinition {
            name: "get_time".to_string(),
            description: "Get the current time in a given timezone.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "timezone": { "type": "string", "description": "An IANA timezone name." }
                },
                "required": ["timezone"],
                "additionalProperties": false
            }),
        },
    };
    let msgs = vec![WireMessage::User {
        content: "What time is it in Tokyo? Use the get_time tool.".into(),
    }];
    let opts = CompleteOpts { max_tokens: Some(128), ..Default::default() };
    let mut stream = match client.stream(&msgs, &opts, std::slice::from_ref(&tool)).await {
        Ok(s) => s,
        Err(e) => return Status::Fail(describe(&e)),
    };
    let mut saw_tool_call = false;
    let mut names = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ev) => {
                let d = format!("{ev:?}");
                if d.contains("ToolCall") {
                    saw_tool_call = true;
                    if d.contains("get_time") {
                        names.push("get_time");
                    }
                }
            }
            Err(e) => return Status::Fail(format!("mid-stream: {}", describe(&e))),
        }
    }
    if saw_tool_call {
        Status::Pass(format!("model emitted a tool call{}", if names.is_empty() {
            ""
        } else {
            " (get_time)"
        }))
    } else {
        // The agent loop cannot function without this, so it's a hard failure
        // even though the endpoint is technically responding.
        Status::Fail(
            "model returned text instead of a tool call — the agent loop needs tool support".into(),
        )
    }
}

/// Walk the ladder until the gateway refuses, reporting each rung. Padding rides
/// in a user message, which is where a real large context lives too.
async fn probe_body_ladder(client: &ChatClient) -> Vec<(usize, Status)> {
    let mut out = Vec::new();
    for &size in LADDER {
        // Padding that won't tempt the model into quoting it back.
        let pad = "x".repeat(size);
        let msgs = vec![WireMessage::User {
            content: format!("Ignore this padding and reply with 'ok'.\n{pad}").into(),
        }];
        let opts = CompleteOpts { max_tokens: Some(16), ..Default::default() };
        let t = Instant::now();
        let status = match client.complete(&msgs, &opts).await {
            Ok(_) => {
                let secs = t.elapsed().as_secs_f64();
                let mbps = (size as f64 / (1 << 20) as f64) / secs.max(0.001);
                Status::Pass(format!("accepted in {secs:.1}s ({mbps:.1} MB/s)"))
            }
            Err(e) => Status::Fail(describe(&e)),
        };
        let stop = status.is_fail();
        out.push((size, status));
        if stop {
            break;
        }
    }
    out
}

/// Walk the ladder with the payload spread across many small, alternating
/// user/assistant messages — each well under the per-string ceiling — to
/// measure the **total** request ceiling. This is the limit a proxy/gateway
/// enforces, and the one the unlimited-context thesis actually needs: a small
/// per-string limit is survivable iff the total can still grow by chunking.
async fn probe_chunked_ladder(client: &ChatClient) -> Vec<(usize, Status)> {
    /// Each message stays well under the ~1 MB per-JSON-string limit observed
    /// on GLM-class servers, so a failure here is about the *total*, not the
    /// string.
    const CHUNK: usize = 256 * 1024;
    let mut out = Vec::new();
    for &size in LADDER {
        let n = size.div_ceil(CHUNK);
        let pad = "x".repeat(CHUNK);
        let mut msgs = Vec::with_capacity(n);
        for i in 0..n {
            if i % 2 == 0 {
                msgs.push(WireMessage::User { content: pad.clone().into() });
            } else {
                msgs.push(WireMessage::Assistant {
                    content: Some(Arc::from(pad.as_str())),
                    tool_calls: vec![],
                });
            }
        }
        let opts = CompleteOpts { max_tokens: Some(16), ..Default::default() };
        let t = Instant::now();
        let status = match client.complete(&msgs, &opts).await {
            Ok(_) => {
                let secs = t.elapsed().as_secs_f64();
                let mbps = (size as f64 / (1 << 20) as f64) / secs.max(0.001);
                Status::Pass(format!("accepted in {secs:.1}s ({mbps:.1} MB/s)"))
            }
            Err(e) => Status::Fail(describe(&e)),
        };
        let stop = status.is_fail();
        out.push((size, status));
        if stop {
            break;
        }
    }
    out
}

/// Largest rung the gateway accepted, or `None` if even the first was refused.
fn largest_accepted(results: &[(usize, Status)]) -> Option<usize> {
    results.iter().rev().find(|(_, s)| !s.is_fail()).map(|(n, _)| *n)
}

/// The first rung the gateway rejected, with its byte size and error message —
/// the message is what distinguishes a token/context-length limit (a model
/// property) from a byte/payload limit (a proxy property).
fn first_failure(results: &[(usize, Status)]) -> Option<(usize, &str)> {
    results.iter().find_map(|(n, s)| match s {
        Status::Fail(m) => Some((*n, m.as_str())),
        _ => None,
    })
}

/// Does a failure message describe a token / context-length limit rather than a
/// byte/payload one? GLM-class servers reject with "Request requires an
/// estimated N tokens, exceeding the selected route's configured context
/// length" — that's the model's context window, not a proxy cap.
fn is_token_limit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("token") && (m.contains("context") || m.contains("length"))
}

/// The rejected token estimate from a token-limit message, if any. Pulls the
/// integer preceding "tokens" ("...an estimated 2621465 tokens..."), tolerating
/// the space between the number and the word.
fn rejected_token_count(msg: &str) -> Option<u64> {
    let lower = msg.to_ascii_lowercase();
    let idx = lower.find("tokens")?;
    let before = &msg[..idx];
    let bytes = before.as_bytes();
    // Skip whitespace between the number and "tokens" ("2621465 tokens").
    let mut end = before.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == end {
        return None;
    }
    before[start..end].parse().ok()
}

/// Interpret both ladders together. The thesis turns on whether the *total*
/// can exceed the per-string limit by spreading content across messages.
fn summarize_ceilings(single: &[(usize, Status)], chunked: &[(usize, Status)]) {
    let per_string = largest_accepted(single);
    let per_request = largest_accepted(chunked);
    let fmt = |n: Option<usize>| match n {
        Some(x) => human(x),
        None => "rejected at 1 MB".to_string(),
    };
    println!("per-string  ceiling: {}  (one message or tool result)", fmt(per_string));
    // A chunked failure that mentions tokens/context-length is the route's
    // context window (a token budget), not a byte/payload cap — label it as such
    // so nobody chases a proxy setting that isn't the constraint.
    let token_limited = first_failure(chunked)
        .map(|(_, m)| is_token_limit(m))
        .unwrap_or(false);
    if token_limited {
        println!("per-request ceiling: TOKEN/context-length limit  (the route's context window)");
    } else {
        println!("per-request ceiling: {}  (total body, chunked across messages)", fmt(per_request));
    }
    println!();
    match per_request {
        Some(r) if r >= 32 << 20 => {
            println!("Total requests clear 32 MB — Claude Code's cap. The per-string limit");
            println!("({}) only means a single message or tool result must be split; the", fmt(per_string));
            println!("whole context is bounded by {} at the proxy, not by this client.", human(r));
        }
        Some(r) if r > per_string.unwrap_or(0) && token_limited => {
            // The total can grow past the per-string limit by chunking, but the
            // route's token context window is the real ceiling — and it's a
            // model/route property, not a proxy byte limit. Say so plainly.
            let (rej_bytes, msg) = first_failure(chunked).expect("token_limited implies a failure");
            let single = human(per_string.unwrap_or(1 << 20));
            println!("Chunking helps: a chunked total of {} passes where a single {}", human(r), single);
            println!("{} message is rejected. But the *total* is bounded by the route's", single);
            println!("configured context length — a TOKEN limit, not a byte/payload limit.");
            println!("{} was accepted; {} was rejected as \"exceeding the selected", human(r), human(rej_bytes));
            match rejected_token_count(msg) {
                Some(t) => {
                    let passed = (t as f64 * (r as f64 / rej_bytes as f64)) as u64;
                    println!("route's configured context length\" (~{t} tokens vs ~{passed} for the size");
                    println!("that passed). The route's context window is the real ceiling:");
                    println!("between ~{passed} and ~{t} tokens.");
                }
                None => println!("route's configured context length\"."),
            }
            println!("That's a model/route property — NOT a proxy byte limit, and not");
            println!("something nginx `client_max_body_size` can raise. To use more context,");
            println!("the route's context length must be configured higher on the model side.");
        }
        Some(r) if r > per_string.unwrap_or(0) => {
            println!("Chunking helps: total {} exceeds the single-message {}.", human(r), fmt(per_string));
            println!("Large contexts survive by spreading across messages, but the total is");
            print_proxy_culprit(r);
        }
        Some(r) => {
            println!("Chunking does NOT help — the total is capped at {} just like a", human(r));
            println!("single string. The model server itself bounds the context; no");
            println!("client-side change can lift it.");
        }
        None => {
            println!("Even a 1 MB chunked request was rejected — the path caps total bodies");
            println!("hard. Check the proxy in front of the model server.");
        }
    }
}

/// Name the usual culprit for a total-body ceiling, when chunking helps.
fn print_proxy_culprit(n: usize) {
    // Exactly 10 MB is the AWS API Gateway signature, and the one result that
    // means the goal is unreachable on this route.
    if n == 10 << 20 {
        println!("capped at 10 MB — exactly AWS API Gateway's payload limit, which");
        println!("cannot be raised. Bypass API Gateway (an ALB, or direct-to-origin) to");
        println!("go larger; no client-side change lifts it. Note this is *below*");
        println!("Claude Code's 32 MB cap, so on this route we'd be more limited.");
    } else if n >= 32 << 20 {
        println!("capped at {} — past API Gateway and at/above Claude Code's 32 MB.", human(n));
    } else if n <= 1 << 20 {
        println!("capped at {} — usually nginx's default `client_max_body_size`,", human(n));
        println!("which can be raised. Whatever sits in front of the model server is");
        println!("what needs changing — this client imposes no cap of its own.");
    } else {
        println!("capped at {} — between 1 and 10 MB. Find what imposes this limit;", human(n));
        println!("it's in the proxy path, not this client.");
    }
}

/// A one-line error description, with the HTTP status kept intact — the status
/// code is the whole diagnostic when a body is rejected (413 vs 502 vs a
/// truncated stream point at different components).
fn describe(e: &ProtoError) -> String {
    match e {
        ProtoError::Status { status, body } => {
            let snippet: String = body.chars().take(160).collect();
            format!("HTTP {status} — {snippet}")
        }
        // reqwest's top-level Display ("error decoding response body") hides the
        // serde detail that actually says what's wrong; walk the source chain so
        // a parse failure points at the field instead of the transport.
        ProtoError::Http(http) => {
            use std::error::Error;
            let mut full = http.to_string();
            let mut src: Option<&dyn Error> = http.source();
            while let Some(s) = src {
                let msg = s.to_string();
                if !msg.is_empty() && !full.contains(&msg) {
                    full.push_str(": ");
                    full.push_str(&msg);
                }
                src = s.source();
            }
            full
        }
        other => other.to_string(),
    }
}

/// Bytes as B/KB/MB, for both the ladder and the caps display.
fn human(n: usize) -> String {
    match n {
        n if n >= 1 << 20 => format!("{} MB", n / (1 << 20)),
        n if n >= 1 << 10 => format!("{} KB", n / (1 << 10)),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_renders_ladder_sizes() {
        assert_eq!(human(1 << 20), "1 MB");
        assert_eq!(human(32 << 20), "32 MB");
        assert_eq!(human(16 * 1024), "16 KB");
        assert_eq!(human(512), "512 B");
    }

    #[test]
    fn status_render_is_aligned_and_tagged() {
        assert!(Status::Pass("d".into()).render("x").starts_with("  ok  "));
        assert!(Status::Fail("d".into()).render("x").contains("FAIL"));
        assert!(Status::Warn("d".into()).render("x").contains("warn"));
    }

    /// The caps line must say "unlimited" for 0, since that's the shipped
    /// default and the whole point of the product.
    #[test]
    fn config_lines_report_unlimited_caps() {
        let s = Settings::load(std::path::Path::new("/nonexistent"));
        let joined = config_lines(&s).join("\n");
        assert!(joined.contains("cap: tool result    unlimited"), "{joined}");
        assert!(joined.contains("cap: read default   whole file"), "{joined}");
        assert!(joined.contains("total timeout       off"), "{joined}");
    }

    /// The ladder includes both limits that matter for the product claim.
    #[test]
    fn ladder_covers_api_gateway_and_claude_code_limits() {
        assert!(LADDER.contains(&(10 << 20)), "API Gateway's 10 MB ceiling");
        assert!(LADDER.contains(&(32 << 20)), "Claude Code's 32 MB cap");
    }

    /// A token/context-length rejection (the route's context window) must be
    /// distinguished from a byte/payload rejection (a proxy cap) — the two call
    /// for opposite conclusions, and only the error body tells them apart.
    #[test]
    fn token_limit_is_detected_from_error_body() {
        // Real shape from the GLM route: "Request requires an estimated 2621465
        // tokens, exceeding the selected route's configured context length".
        let msg = "HTTP 400 — {\"error\":{\"code\":\"invalid_request\",\"message\":\
                   \"Request requires an estimated 2621465 tokens, exceeding the \
                   selected route's configured context length\"}}";
        assert!(is_token_limit(msg));
        assert_eq!(rejected_token_count(msg), Some(2_621_465));

        // A proxy/payload rejection is NOT a token limit.
        let proxy = "HTTP 413 — <html>Request Entity Too Large</html>";
        assert!(!is_token_limit(proxy));
        assert_eq!(rejected_token_count(proxy), None);

        // The per-JSON-string limit is a byte limit, not a token one.
        let per_string = "HTTP 400 — {\"message\":\"JSON string must not exceed 1048576 bytes\"}";
        assert!(!is_token_limit(per_string));
    }

    /// `first_failure` returns the smallest rejected rung, not any later one.
    #[test]
    fn first_failure_picks_the_smallest_rejected_rung() {
        let ladder = vec![
            (1 << 20, Status::Pass("ok".into())),
            (10 << 20, Status::Fail("HTTP 400 — too big".into())),
            (32 << 20, Status::Fail("HTTP 400 — also too big".into())),
        ];
        assert_eq!(first_failure(&ladder), Some((10 << 20, "HTTP 400 — too big")));
        // Largest accepted skips the failures and finds the rung before them.
        assert_eq!(largest_accepted(&ladder), Some(1 << 20));
    }
}
