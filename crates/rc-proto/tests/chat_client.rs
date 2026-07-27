//! Integration test: ChatClient against a mock `/v1/chat/completions`.
//!
//! Proves the M0 round trip — canonical request shape, auth header, response
//! parsing, and 4xx handling — without a live endpoint. The canonical-request
//! assertions tie this test to §4.6 from day one.

use rc_proto::{ChatClient, CompleteOpts, WireMessage};
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
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(sample_response("hi there")),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-key".to_string(), "mock".to_string()).unwrap();
    let messages = vec![WireMessage::User { content: "say hi".into() }];
    let resp = client.complete(&messages, &CompleteOpts::default()).await.unwrap();

    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].message.content.as_deref(), Some("hi there"));
    assert_eq!(resp.choices[0].finish_reason, "stop");
    let usage = resp.usage.expect("usage present");
    assert_eq!(usage.prompt_tokens, 5);
    assert_eq!(usage.cached_tokens(), Some(5));

    // The request body must be canonical: top-level keys sorted (messages,
    // model, stream — the None optionals are skipped), compact.
    let received = server.received_requests().await.expect("requests recorded");
    assert_eq!(received.len(), 1);
    let body = std::str::from_utf8(&received[0].body).unwrap();
    assert!(
        body.starts_with("{\"messages\":"),
        "expected canonical key order (messages first), got: {body}"
    );
    assert!(
        body.contains(r#","model":"mock","stream":false}"#),
        "expected compact, sorted tail, got: {body}"
    );
}

#[tokio::test]
async fn surfaces_non_2xx_as_status_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
        .mount(&server)
        .await;

    let client = ChatClient::new(server.uri(), "test-key".to_string(), "mock".to_string()).unwrap();
    let messages = vec![WireMessage::User { content: "hi".into() }];
    let err = client.complete(&messages, &CompleteOpts::default()).await.unwrap_err();
    let s = err.to_string();
    assert!(s.contains("401"), "error should mention status: {s}");
}

#[tokio::test]
async fn rejects_empty_api_key_upfront() {
    let err = ChatClient::new("http://x".to_string(), String::new(), "m".to_string()).unwrap_err();
    assert!(err.to_string().contains("API key"));
}
