//! Non-streaming chat completions client.
//!
//! M0 sends `stream: false` and returns a single response. Streaming (SSE
//! decode, `ToolCallAccumulator`, `finish_reason` handling, retries) lands in
//! M1. The request body is serialized through [`crate::canonical`] so its
//! bytes are stable across turns (§4.6) — this matters once a tools array is
//! attached, but we do it from M0 so the discipline is in place.

use crate::canonical;
use crate::error::ProtoError;
use crate::wire::{ChatCompletionRequest, ChatCompletionResponse, WireMessage};
use std::time::Duration;

/// Per-call options. The agent loop (M1) fills these from capabilities + config.
#[derive(Debug, Clone, Default)]
pub struct CompleteOpts {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// An OpenAI-compatible `/v1/chat/completions` client.
///
/// Holds a reusable `reqwest` connection pool. `rustls-tls` (no openssl) keeps
/// the binary self-contained — a hard constraint (§0).
#[derive(Debug)]
pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}
impl ChatClient {
    /// `api_key` empty → `NoApiKey` immediately, so the caller surfaces a clear
    /// error before any network attempt.
    pub fn new(base_url: String, api_key: String, model: String) -> Result<Self, ProtoError> {
        if api_key.is_empty() {
            return Err(ProtoError::NoApiKey);
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        })
    }

    /// One non-streaming round trip. Returns the parsed response.
    pub async fn complete(
        &self,
        messages: &[WireMessage],
        opts: &CompleteOpts,
    ) -> Result<ChatCompletionResponse, ProtoError> {
        let req = ChatCompletionRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
            stream: false,
        };
        // Canonical bytes: stable across builds/sessions (§4.6).
        let body = canonical::to_string(&req)?;

        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.api_key.clone())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ProtoError::Status { status: status.as_u16(), body: text });
        }

        let parsed: ChatCompletionResponse = resp.json().await?;
        if parsed.choices.is_empty() {
            return Err(ProtoError::EmptyChoices);
        }
        Ok(parsed)
    }
}
