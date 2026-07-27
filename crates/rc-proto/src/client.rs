//! Non-streaming and streaming chat completions client (§3, §4.6).
//!
//! M0 added `complete` (non-streaming). M1 adds `stream` (§3.3): a streamed
//! request whose body is serialized canonically (§4.6) and whose response is
//! decoded incrementally — [`crate::stream::SseDecoder`] →
//! [`crate::stream::StreamFuser`] → [`EventStream`] of [`AgentStreamEvent`].
//! `rustls-tls` (no openssl) keeps the binary self-contained (§0).

use crate::canonical;
use crate::error::ProtoError;
use crate::stream::{AgentStreamEvent, SseDecoder, StreamFuser};
use crate::wire::{
    ChatCompletionRequest, ChatCompletionResponse, StreamOptions, ToolChoiceValue, ToolDefinition,
    WireMessage,
};
use bytes::Bytes;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio_stream::Stream;

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
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            stream_options: None,
        };
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

    /// One streaming round trip. Returns an [`AgentStreamEvent`] stream. The
    /// request body is canonical (§4.6); tool calls are reassembled by the
    /// [`crate::stream::ToolCallAccumulator`] and emitted (parsed, or as a
    /// parse error for the loop to feed back, §3.3). `stream_options` requests
    /// the trailing usage chunk (§3.6).
    pub async fn stream(
        &self,
        messages: &[WireMessage],
        opts: &CompleteOpts,
        tools: &[ToolDefinition],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<AgentStreamEvent, ProtoError>> + Send>>, ProtoError>
    {
        let req = ChatCompletionRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
            stream: true,
            tools: tools.to_vec(),
            tool_choice: if tools.is_empty() { None } else { Some(ToolChoiceValue::Auto) },
            parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
            stream_options: Some(StreamOptions { include_usage: true }),
        };
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
        let body: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
            Box::pin(resp.bytes_stream());
        Ok(Box::pin(EventStream::new(body)))
    }
}

/// A stream of [`AgentStreamEvent`] driven by polling the HTTP body through
/// the SSE decoder and fuser. Returned boxed from [`ChatClient::stream`].
pub struct EventStream {
    body: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    dec: SseDecoder,
    fuser: StreamFuser,
    pending: VecDeque<AgentStreamEvent>,
    done: bool,
}

impl EventStream {
    fn new(body: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>) -> Self {
        Self {
            body,
            dec: SseDecoder::new(),
            fuser: StreamFuser::new(),
            pending: VecDeque::new(),
            done: false,
        }
    }
}

impl Stream for EventStream {
    type Item = Result<AgentStreamEvent, ProtoError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Poll::Ready(Some(Ok(ev)));
            }
            if self.done {
                return Poll::Ready(None);
            }
            match self.body.as_mut().poll_next(cx) {
                Poll::Ready(None) => {
                    for chunk in self.dec.finish() {
                        match chunk {
                            Ok(c) => {
                                let evs = self.fuser.apply(c);
                                for ev in evs {
                                    self.pending.push_back(ev);
                                }
                            }
                            Err(e) => {
                                self.done = true;
                                return Poll::Ready(Some(Err(e)));
                            }
                        }
                    }
                    let evs = self.fuser.finish();
                    for ev in evs {
                        self.pending.push_back(ev);
                    }
                    self.done = true;
                    continue; // drain pending next iteration
                }
                Poll::Ready(Some(Err(e))) => {
                    self.done = true;
                    return Poll::Ready(Some(Err(ProtoError::Http(e))));
                }
                Poll::Ready(Some(Ok(bytes))) => {
                    for chunk in self.dec.feed(&bytes) {
                        match chunk {
                            Ok(c) => {
                                let evs = self.fuser.apply(c);
                                for ev in evs {
                                    self.pending.push_back(ev);
                                }
                            }
                            Err(e) => {
                                self.done = true;
                                return Poll::Ready(Some(Err(e)));
                            }
                        }
                    }
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
