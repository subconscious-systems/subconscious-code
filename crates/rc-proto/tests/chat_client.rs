//! Integration test: ChatClient against a mock `/v1/chat/completions`.
//!
//! Proves the M0 round trip — canonical request shape, auth header, response
//! parsing, and 4xx handling — without a live endpoint. The canonical-request
//! assertions tie this test to §4.6 from day one.

use rc_proto::{ChatClient, CompleteOpts, RetryOpts, WireMessage};
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sample_response(text: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "model": "mock",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3,
            "total_tokens": 8,
            "prompt_tokens_details": { "cached_tokens": 5 }
        }
    })
    .to_string()
}

#[tokio::test]
async fn completes_a_simple_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .and(header("x-subconscious-client", "subconscious_code"))
        .and(header("x-subconscious-code-session-id", "session-test-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(sample_response("hi there")),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "test-key".to_string(),
        "mock".to_string(),
        Some(Duration::from_secs(600)),
    )
    .unwrap();
    let messages = vec![WireMessage::User {
        content: "say hi".into(),
    }];
    let opts = CompleteOpts {
        session_id: Some("session-test-123".into()),
        reasoning_effort: Some("high".into()),
        ..CompleteOpts::default()
    };
    let resp = client.complete(&messages, &opts).await.unwrap();

    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].message.content.as_deref(), Some("hi there"));
    assert_eq!(resp.choices[0].finish_reason, "stop");
    let usage = resp.usage.expect("usage present");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.cached_tokens(), Some(5));

    // The request body is serialized straight to bytes (no Value round-trip),
    // so keys follow struct declaration order rather than being alphabetized.
    // What matters for prefix caching is that the bytes are *stable* and
    // compact, not that they're sorted — see `canonical::to_bytes`.
    let received = server.received_requests().await.expect("requests recorded");
    assert_eq!(received.len(), 1);
    let body = std::str::from_utf8(&received[0].body).unwrap();
    assert!(
        body.starts_with(r#"{"model":"mock","messages":"#),
        "expected declaration order (model first), got: {body}"
    );
    assert!(
        body.ends_with(r#""stream":false}"#),
        "expected compact tail, got: {body}"
    );
    assert!(
        body.contains(r#""reasoning_effort":"high""#),
        "reasoning effort must reach the wire: {body}"
    );
    assert!(
        !body.contains("\n") && !body.contains(": "),
        "expected compact JSON: {body}"
    );
}

#[tokio::test]
async fn gzip_rejection_falls_back_once_and_is_remembered() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("content-encoding", "gzip"))
        .respond_with(ResponseTemplate::new(415).set_body_string("gzip is not supported"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(sample_response("ok")),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "k".into(), "m".into(), None)
        .unwrap()
        .with_request_gzip(true);
    let messages = [WireMessage::User {
        content: "hello".into(),
    }];
    client
        .complete(&messages, &CompleteOpts::default())
        .await
        .unwrap();
    client
        .complete(&messages, &CompleteOpts::default())
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3, "gzip probe + fallback + next request");
    assert_eq!(
        requests[0]
            .headers
            .get("content-encoding")
            .and_then(|value| value.to_str().ok()),
        Some("gzip")
    );
    assert!(!requests[1].headers.contains_key("content-encoding"));
    assert!(!requests[2].headers.contains_key("content-encoding"));
    assert!(std::str::from_utf8(&requests[1].body)
        .unwrap()
        .starts_with(r#"{"model":"m""#));
}

#[tokio::test]
async fn surfaces_non_2xx_as_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "test-key".to_string(),
        "mock".to_string(),
        Some(Duration::from_secs(600)),
    )
    .unwrap();
    let messages = vec![WireMessage::User {
        content: "hi".into(),
    }];
    let err = client
        .complete(&messages, &CompleteOpts::default())
        .await
        .unwrap_err();
    let s = err.to_string();
    assert!(s.contains("401"), "error should mention status: {s}");
}

#[tokio::test]
async fn rejects_empty_api_key_upfront() {
    let err = ChatClient::new(
        "http://x".to_string(),
        String::new(),
        "m".to_string(),
        Some(Duration::from_secs(600)),
    )
    .unwrap_err();
    assert!(err.to_string().contains("API key"));
}

#[tokio::test]
async fn rejects_an_invalid_session_id_before_sending() {
    let server = MockServer::start().await;
    let client = ChatClient::new(
        server.uri(),
        "test-key".into(),
        "mock".into(),
        Some(Duration::from_secs(600)),
    )
    .unwrap();
    let opts = CompleteOpts {
        session_id: Some("session\r\ninjected".into()),
        ..CompleteOpts::default()
    };

    let err = client
        .complete(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &opts,
        )
        .await
        .unwrap_err();

    assert!(err.to_string().contains("session id"));
    assert!(server
        .received_requests()
        .await
        .expect("requests recorded")
        .is_empty());
}

#[tokio::test]
async fn honors_a_configured_request_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;

    // 50 ms timeout; the mock responds after 2 s, so the client must give up fast
    // (proving the configured timeout is honored — it was hardcoded 600 s).
    let client = ChatClient::new(
        server.uri(),
        "k".into(),
        "m".into(),
        Some(Duration::from_millis(50)),
    )
    .unwrap();
    let messages = vec![WireMessage::User {
        content: "hi".into(),
    }];
    let start = std::time::Instant::now();
    let err = client
        .complete(&messages, &CompleteOpts::default())
        .await
        .unwrap_err();
    let elapsed = start.elapsed();
    let s = err.to_string().to_lowercase();
    assert!(
        s.contains("transport") || s.contains("timeout"),
        "expected a timeout error: {err}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "should give up well under the 2 s delay, took {elapsed:?}"
    );
}

fn retry_opts(max: u32) -> RetryOpts {
    RetryOpts {
        max_retries: max,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    }
}

#[tokio::test]
async fn retries_on_429_then_succeeds() {
    let server = MockServer::start().await;
    // The 429 (mounted first → higher priority, up to 2) handles the first two
    // attempts; once exhausted, the 200 fallback wins.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(sample_response("ok")),
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
    .with_retry(retry_opts(3));
    let resp = client
        .complete(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &CompleteOpts::default(),
        )
        .await
        .unwrap();
    assert_eq!(resp.choices[0].message.content.as_deref(), Some("ok"));
    let received = server.received_requests().await.expect("requests recorded");
    assert_eq!(received.len(), 3, "1 initial + 2 retries");
}

#[tokio::test]
async fn gives_up_after_exhausting_retries_on_503() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "k".into(),
        "m".into(),
        Some(Duration::from_secs(600)),
    )
    .unwrap()
    .with_retry(retry_opts(2));
    let err = client
        .complete(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &CompleteOpts::default(),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("503"),
        "expected 503 in error: {err}"
    );
    let received = server.received_requests().await.expect("requests recorded");
    assert_eq!(received.len(), 3, "1 initial + 2 retries, then give up");
}

#[tokio::test]
async fn does_not_retry_non_transient_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "k".into(),
        "m".into(),
        Some(Duration::from_secs(600)),
    )
    .unwrap()
    .with_retry(retry_opts(3));
    let err = client
        .complete(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &CompleteOpts::default(),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("404"),
        "expected 404 in error: {err}"
    );
    let received = server.received_requests().await.expect("requests recorded");
    assert_eq!(received.len(), 1, "4xx is not transient — no retry");
}

#[tokio::test]
async fn respects_retry_after_header_on_429() {
    let server = MockServer::start().await;
    // 429 with Retry-After: 1 (mounted first → higher priority, up to 1), then 200.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_string("rate limited"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(sample_response("ok")),
        )
        .mount(&server)
        .await;

    // base_delay 5 s would dominate if backoff were used; Retry-After: 1 (under
    // the 10 s cap) should win → ~1 s, not ~5 s.
    let client = ChatClient::new(
        server.uri(),
        "k".into(),
        "m".into(),
        Some(Duration::from_secs(600)),
    )
    .unwrap()
    .with_retry(RetryOpts {
        max_retries: 3,
        base_delay: Duration::from_secs(5),
        max_delay: Duration::from_secs(10),
    });
    let start = std::time::Instant::now();
    let resp = client
        .complete(
            &[WireMessage::User {
                content: "hi".into(),
            }],
            &CompleteOpts::default(),
        )
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(resp.choices[0].message.content.as_deref(), Some("ok"));
    assert!(
        elapsed < Duration::from_secs(3),
        "Retry-After: 1 should win over 5 s backoff, took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(800),
        "should have waited ~1 s, took {elapsed:?}"
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("requests recorded")
            .len(),
        2
    );
}
