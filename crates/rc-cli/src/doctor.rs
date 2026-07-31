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
        let ceiling = probe_body_ladder(&client).await;
        for (size, status) in &ceiling {
            println!("{}", status.render(&human(*size)));
        }
        println!();
        summarize_ceiling(&ceiling);
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

/// Interpret the ladder result, naming the usual culprit for the size found.
fn summarize_ceiling(results: &[(usize, Status)]) {
    let largest_ok = results.iter().rev().find(|(_, s)| !s.is_fail()).map(|(n, _)| *n);
    match largest_ok {
        None => {
            println!("The gateway rejected even a 1 MB body. Something upstream is capping");
            println!("requests hard — check the proxy in front of the model server.");
        }
        Some(n) if n >= 32 << 20 => {
            println!("Largest accepted body: {}. That clears Claude Code's 32 MB cap,", human(n));
            println!("so the client is the only thing that would bound context — and it doesn't.");
        }
        // Exactly 10 MB is the AWS API Gateway signature, and it's the one
        // result that means the goal is unreachable on this route.
        Some(n) if n == 10 << 20 => {
            println!("Largest accepted body: 10 MB — exactly AWS API Gateway's payload limit.");
            println!();
            println!("That limit cannot be raised. If API Gateway is in the path, a larger");
            println!("context needs a route that bypasses it (an ALB, or direct-to-origin);");
            println!("no client-side change can lift it. Note this is *below* Claude Code's");
            println!("32 MB cap, so on this route we'd be more limited, not less.");
        }
        Some(n) if n > 10 << 20 => {
            println!("Largest accepted body: {}. Past API Gateway's 10 MB ceiling but", human(n));
            println!("below Claude Code's 32 MB cap — worth finding what imposes this limit.");
        }
        Some(n) => {
            println!("Largest accepted body: {}.", human(n));
            println!();
            println!("A ceiling at 1 MB is usually nginx's default `client_max_body_size`,");
            println!("which can be raised. Whatever sits in front of the model server is");
            println!("what needs changing — this client imposes no cap of its own.");
        }
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
}
