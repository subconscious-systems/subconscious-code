use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use dlr_compress::Compressor;
use dlr_core::ContentStore;
use dlr_receiver::Receiver;
use dlr_sidecar::{router as sidecar_router, SidecarState};
use rc_proto::{AgentStreamEvent, ChatClient, CompleteOpts, DlrMode, WireMessage};
use serde_json::Value;
use tokio_stream::StreamExt;

#[derive(Clone, Default)]
struct UpstreamState(Arc<Mutex<Vec<Value>>>);

async fn upstream(
    State(state): State<UpstreamState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    state.0.lock().unwrap().push(body);
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        ),
    )
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

async fn new_sidecar(upstream_url: &str, token: &str) -> String {
    let receiver = Receiver::new(ContentStore::new(), Compressor::default());
    let state = SidecarState::new(
        receiver,
        upstream_url.to_string(),
        Some(token.to_string()),
        false,
    )
    .unwrap();
    serve(sidecar_router(state)).await
}

fn client(sidecar_url: String) -> ChatClient {
    ChatClient::new(
        "http://json-fallback.invalid/v1".into(),
        "provider-secret".into(),
        "test-model".into(),
        None,
    )
    .unwrap()
    .with_dlr(
        sidecar_url,
        Some("sidecar-secret".into()),
        DlrMode::Required,
        5,
    )
    .unwrap()
}

async fn send(client: &ChatClient, messages: &[WireMessage]) -> rc_proto::RequestPayloadStats {
    let opts = CompleteOpts {
        session_id: Some("integration-session".into()),
        ..Default::default()
    };
    let (mut stream, _, stats) = client.stream(messages, &opts, &[]).await.unwrap();
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let AgentStreamEvent::Text(delta) = event.unwrap() {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "ok");
    stats
}

#[tokio::test]
async fn delta_streaming_restart_resync_and_projection_replacement_work() {
    let observed = UpstreamState::default();
    let upstream_url = serve(
        Router::new()
            .route("/v1/chat/completions", post(upstream))
            .with_state(observed.clone()),
    )
    .await;
    let sidecar_url = new_sidecar(&upstream_url, "sidecar-secret").await;
    let first_client = client(sidecar_url.clone());

    let mut messages = vec![WireMessage::User {
        content: format!("large stable prefix {}", "x".repeat(256 * 1024)).into(),
    }];
    let first = send(&first_client, &messages).await;
    messages.push(WireMessage::Assistant {
        content: Some("previous answer".into()),
        reasoning_content: None,
        tool_calls: vec![],
    });
    messages.push(WireMessage::User {
        content: "next question".into(),
    });
    let second = send(&first_client, &messages).await;
    assert!(first.wire_bytes < first.json_bytes / 20);
    assert!(second.wire_bytes < second.json_bytes / 20);
    eprintln!(
        "DLR steady state: full JSON={} bytes, DLR={} bytes ({:.1}x smaller)",
        second.json_bytes,
        second.wire_bytes,
        second.json_bytes as f64 / second.wire_bytes as f64
    );

    // A new SC process has no local ACK state. The first APPEND conflicts with
    // the existing sidecar root, then RESYNC repairs it transparently.
    let restarted_client = client(sidecar_url);
    messages.push(WireMessage::Assistant {
        content: Some("after restart".into()),
        reasoning_content: None,
        tool_calls: vec![],
    });
    send(&restarted_client, &messages).await;

    // Compaction/reprojection can replace the effective transcript under the
    // same session id. That also goes through the RESYNC replacement path.
    let replacement = vec![WireMessage::User {
        content: "compacted summary".into(),
    }];
    send(&restarted_client, &replacement).await;

    let requests = observed.0.lock().unwrap();
    let counts = requests
        .iter()
        .map(|request| request["messages"].as_array().unwrap().len())
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![1, 3, 4, 1]);
    assert!(requests
        .iter()
        .all(|request| request["model"] == "test-model"));
}

#[tokio::test]
async fn auto_falls_back_before_dlr_activation() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-fallback",
            "object": "chat.completion",
            "model": "test-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]
        })))
        .mount(&upstream)
        .await;
    let client = ChatClient::new(
        upstream.uri(),
        "provider-secret".into(),
        "test-model".into(),
        None,
    )
    .unwrap()
    .with_dlr("http://127.0.0.1:1".into(), None, DlrMode::Prefer, 5)
    .unwrap();
    let response = client
        .complete(
            &[WireMessage::User {
                content: "hello".into(),
            }],
            &CompleteOpts {
                session_id: Some("fallback-session".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(response.choices.len(), 1);
}
