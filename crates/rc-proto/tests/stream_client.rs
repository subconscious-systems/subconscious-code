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

/// The review's mid-body cut: the server sends a valid SSE body declaring a
/// `Content-Length` larger than what it then delivers, and the connection
/// dies. The wire client previously surfaced the transport error immediately —
/// only the clean-EOF path flushed the decoder/fuser — so any text already
/// streamed was lost from the drain and a half-assembled tool call never got
/// the `confirmed=false` finish. Now the flush runs before the error, and the
/// error surfaces *after* the flushed events.
#[tokio::test]
async fn cut_mid_body_flushes_partial_text_and_unconfirmed_finish_before_the_error() {
    use std::io::Write as _;

    // A raw HTTP server on a plain thread (tokio has no net feature here):
    // answer the POST with one complete SSE frame, claim more bytes than will
    // ever arrive, then close the socket mid-body.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial answer\"}}]}\n\n";
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len() * 10
    );
    let server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().unwrap();
        // Drain the request before answering: a server that closes without
        // reading the request makes hyper cancel the whole exchange with
        // "unexpected message" instead of yielding the (broken) response the
        // test needs to observe.
        use std::io::Read as _;
        let mut seen = Vec::new();
        let expected = loop {
            let mut b = [0u8; 4096];
            let n = sock.read(&mut b).expect("reading the request");
            if n == 0 {
                panic!("peer closed before sending a full request");
            }
            seen.extend_from_slice(&b[..n]);
            if let Some(head_end) = find_subsequence(&seen, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&seen[..head_end]).to_uppercase();
                let len = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("CONTENT-LENGTH:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break seen.len() >= head_end + 4 + len;
            }
        };
        assert!(expected, "the request must be fully drained first");
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(body).unwrap();
        let _ = sock.flush();
        // Drop without satisfying Content-Length -> mid-body transport error.
    });

    let client = ChatClient::new(
        format!("http://{addr}"),
        "k".into(),
        "m".into(),
        Some(Duration::from_secs(60)),
    )
    .unwrap();
    let (mut stream, _retries, _payload) = client
        .stream(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &CompleteOpts::default(),
            &[],
        )
        .await
        .unwrap();

    let mut text = String::new();
    let mut finish_index = None;
    let mut error_index = None;
    let mut index = 0usize;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(AgentStreamEvent::Text(t)) => text.push_str(&t),
            Ok(AgentStreamEvent::Finish { .. }) => finish_index = Some(index),
            Ok(_) => {}
            Err(_) => error_index = Some(index),
        }
        index += 1;
    }
    server.join().unwrap();

    assert_eq!(text, "partial answer", "streamed text must not be lost by the cut");
    let finish = finish_index.expect("the cut-stream finish (stream-ended) must be flushed");
    let error = error_index.expect("the transport error must surface after the flush");
    assert!(
        finish < error,
        "fuser finish at event {finish} must precede the error at event {error}"
    );

    // After the error the stream terminated cleanly (the loop above drained
    // to None without hanging); the cut-stream text survived.
}

/// The first index at which `needle` occurs inside `haystack`, if any.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
