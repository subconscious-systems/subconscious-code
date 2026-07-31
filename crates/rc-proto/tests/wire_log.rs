//! Redaction check (§7.1): request logging must contain the canonical request
//! body (so a run is debuggable) but must NEVER contain the API key. The key
//! lives only in the `Authorization: Bearer` header, which `ChatClient` does not
//! log; the request body has no key field (`wire.rs`).

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use rc_proto::{ChatClient, CompleteOpts, WireMessage};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A `tracing` writer that appends every event into a shared buffer.
#[derive(Clone)]
struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct CapturingMaker(Arc<Mutex<Vec<u8>>>);

impl tracing_subscriber::fmt::MakeWriter for CapturingMaker {
    type Writer = CapturingWriter;
    fn make_writer(&self) -> Self::Writer {
        CapturingWriter(self.0.clone())
    }
}

fn sample_response() -> String {
    serde_json::json!({
        "id": "x", "model": "mock",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}

#[tokio::test]
async fn debug_request_log_has_body_but_not_the_api_key() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    // Capture rc_proto's debug logs into `buf`. Filter to rc_proto only so the
    // mock server's own logs don't pollute the buffer.
    let _ = tracing_subscriber::fmt::Subscriber::builder()
        .with_writer(CapturingMaker(buf.clone()))
        .with_env_filter(tracing_subscriber::EnvFilter::new("rc_proto=debug"))
        .with_ansi(false)
        .try_init();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(sample_response()),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new(
        server.uri(),
        "secret-key-xyz".into(),
        "mock".into(),
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    let messages = vec![WireMessage::User { content: "fingerprint-me".into() }];
    let _ = client.complete(&messages, &CompleteOpts::default()).await.unwrap();

    let captured = String::from_utf8_lossy(&buf.lock().unwrap()).to_string();
    assert!(captured.contains("→ POST"), "request log should be present: {captured}");
    assert!(captured.contains("fingerprint-me"), "body should be logged: {captured}");
    assert!(
        !captured.contains("secret-key-xyz"),
        "API key must never appear in logs: {captured}"
    );
}
