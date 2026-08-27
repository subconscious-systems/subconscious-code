//! Integration test: ChatClient::stream against a mock `/v1/chat/completions`
//! emitting SSE. Proves the streaming path end-to-end — text deltas, tool-call
//! argument reassembly across fragments, finish, and the trailing usage chunk.

use rc_proto::stream::AgentStreamEvent;
use rc_proto::{ChatClient, CompleteOpts, RetryOpts, WireMessage};
use std::time::Duration;
use tokio_stream::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body(lines: &[&str]) -> String {
    lines.iter().map(|l| format!("data: {l}\n\n")).collect()
}

#[tokio::test]
async fn streams_text_finish_and_usage() {
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"content":"hel"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
        "[DONE]",
    ]);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("x-subconscious-client", "subconscious_code"))
        .and(header(
            "x-subconscious-code-session-id",
            "session-stream-123",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "k".into(),
        "m".into(),
        Some(Duration::from_secs(600)),
    )
    .unwrap();
    let opts = CompleteOpts {
        session_id: Some("session-stream-123".into()),
        ..CompleteOpts::default()
    };
    let (mut stream, _retries, payload) = client
        .stream(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &opts,
            &[],
        )
        .await
        .unwrap();

    let mut text = String::new();
    let mut finish = String::new();
    let mut usage = None;
    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            AgentStreamEvent::Text(t) => text.push_str(&t),
            AgentStreamEvent::Finish { reason } => finish = format!("{reason:?}"),
            AgentStreamEvent::Usage(u) => usage = Some(u),
            _ => {}
        }
    }
    assert_eq!(text, "hello");
    assert_eq!(payload.json_bytes, payload.wire_bytes);
    assert!(payload.wire_bytes > 0);
    assert!(finish.contains("Stop"));
    assert_eq!(usage.unwrap().completion_tokens, 1);
}

#[tokio::test]
async fn stream_assembles_tool_call_args_across_fragments() {
    // The model streams one tool call whose `arguments` JSON is split across
    // two chunks: `{"file` then `":"x"}` -> `{"file":"x"}`.
    let c1 = serde_json::json!({
        "choices":[{"index":0,"delta":{"role":"assistant",
            "tool_calls":[{"index":0,"id":"call_1","function":{"name":"Read","arguments":"{\"file"}}]
        }}]
    });
    let c2 = serde_json::json!({
        "choices":[{"index":0,"delta":{
            "tool_calls":[{"index":0,"function":{"arguments":"\":\"x\"}"}}]
        }}]
    });
    let c3 = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
    let body = sse_body(&[&c1.to_string(), &c2.to_string(), &c3.to_string(), "[DONE]"]);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "k".into(),
        "m".into(),
        Some(Duration::from_secs(600)),
    )
    .unwrap();
    let (mut stream, _retries, _payload) = client
        .stream(
            &[WireMessage::User {
                content: "read it".into(),
            }],
            &CompleteOpts::default(),
            &[],
        )
        .await
        .unwrap();

    let mut ready = None;
    let mut finish = String::new();
    while let Some(ev) = stream.next().await {
        match ev.unwrap() {
            AgentStreamEvent::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                ready = Some((id, name, arguments));
            }
            AgentStreamEvent::Finish { reason } => finish = format!("{reason:?}"),
            _ => {}
        }
    }
    let (id, name, arguments) = ready.expect("a tool call was assembled");
    assert_eq!(id, "call_1");
    assert_eq!(name, "Read");
    let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
    assert_eq!(parsed, serde_json::json!({"file":"x"}));
    assert!(finish.contains("ToolCalls"));
}

#[tokio::test]
async fn stream_retries_on_429_then_streams() {
    // A streaming request is retried only before the body starts flowing: the
    // first attempt gets 429, the retry gets the SSE body.
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header(
            "x-subconscious-code-session-id",
            "session-retry-123",
        ))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header(
            "x-subconscious-code-session-id",
            "session-retry-123",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "k".into(),
        "m".into(),
        Some(Duration::from_secs(600)),
    )
    .unwrap()
    .with_retry(RetryOpts {
        max_retries: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    });
    let opts = CompleteOpts {
        session_id: Some("session-retry-123".into()),
        ..CompleteOpts::default()
    };
    let (mut stream, retries, _payload) = client
        .stream(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &opts,
            &[],
        )
        .await
        .unwrap();
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        if let AgentStreamEvent::Text(t) = ev.unwrap() {
            text.push_str(&t);
        }
    }
    assert_eq!(text, "hi");
    assert_eq!(retries, 1, "1 wire retry after the initial 429");
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("requests recorded")
            .len(),
        2,
        "1 initial 429 + 1 retry 200"
    );
}
