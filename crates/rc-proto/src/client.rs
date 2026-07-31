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
    /// T2: abort the stream if no chunk arrives within this duration (a stall),
    /// failing fast instead of waiting for the total request timeout. `None`
    /// (default) disables the idle bound. Consumed by `rc_core::ChatModel`; not a
    /// wire field.
    pub idle_timeout: Option<std::time::Duration>,
}

/// Retry policy for transient HTTP errors (429 / 5xx). `max_retries = 0` (the
/// default) disables retry — a request fails on the first transient error.
#[derive(Debug, Clone, Copy)]
pub struct RetryOpts {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryOpts {
    fn default() -> Self {
        Self {
            max_retries: 0,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
        }
    }
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
    retry: RetryOpts,
}

impl ChatClient {
    /// `api_key` empty → `NoApiKey` immediately, so the caller surfaces a clear
    /// error before any network attempt. `timeout` bounds the *total* request
    /// (connect → end of body); on the streaming path it caps the whole stream,
    /// so a small value can cut a stream mid-tool-call (the loop synthesizes
    /// `Interrupted` results for any outstanding call — see `rc_core::agent`).
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        timeout: Duration,
    ) -> Result<Self, ProtoError> {
        if api_key.is_empty() {
            return Err(ProtoError::NoApiKey);
        }
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            retry: RetryOpts::default(),
        })
    }

    /// Set the retry policy for transient HTTP errors (429/5xx). Default is no
    /// retry. The body is canonical and stable, so retrying is safe; a streaming
    /// request is retried only before the body starts flowing — a mid-stream
    /// error is not retried (no resume). Builder.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryOpts) -> Self {
        self.retry = retry;
        self
    }

    /// POST `body` to `url`, retrying on a transient error up to
    /// `retry.max_retries` times with exponential backoff. A transient error is
    /// a status of 429/5xx, or a *connection* error (DNS/TCP-refused/TLS) that
    /// isn't a timeout. Timeouts are NOT retried — a retry would multiply
    /// worst-case latency (`max_retries × total timeout`) for a merely-slow
    /// server. Returns the 2xx `Response`; a non-transient error (or an
    /// exhausted retry budget) returns the final error.
    async fn send_with_retry(&self, url: &str, body: &str) -> Result<reqwest::Response, ProtoError> {
        let mut attempt = 0u32;
        loop {
            let resp = match self
                .http
                .post(url)
                .bearer_auth(self.api_key.clone())
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if Self::is_transient_transport(&e) && attempt < self.retry.max_retries {
                        let d = self.backoff(attempt);
                        tracing::warn!(error = %e, attempt, "transient transport error; retrying in {d:?}");
                        tokio::time::sleep(d).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(ProtoError::Http(e));
                }
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            let transient = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504);
            if transient && attempt < self.retry.max_retries {
                // Honor `Retry-After` (seconds) if the server sent one, capped at
                // max_delay; else exponential backoff.
                let d = self.retry_after(&resp).unwrap_or_else(|| self.backoff(attempt));
                tracing::warn!(status = status.as_u16(), attempt, "transient HTTP error; retrying in {d:?}");
                tokio::time::sleep(d).await;
                attempt += 1;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            tracing::debug!("← {status}\n{text}");
            return Err(ProtoError::Status { status: status.as_u16(), body: text });
        }
    }

    /// Exponential backoff: `base * 2^attempt`, capped at `max_delay`. No jitter
    /// (a refinement); deterministic backoff is enough to spread load.
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u64 << attempt.min(20);
        self.retry.base_delay.saturating_mul(factor as u32).min(self.retry.max_delay)
    }

    /// A transport error worth retrying: a *connection* failure (DNS, TCP
    /// refused, TLS handshake) that isn't a timeout. Timeouts are excluded so a
    /// slow server can't multiply latency across retries.
    fn is_transient_transport(e: &reqwest::Error) -> bool {
        e.is_connect() && !e.is_timeout()
    }

    /// Parse the `Retry-After` header (integer seconds) if present, capped at
    /// `max_delay` so a misconfigured/hostile value can't stall the turn. The
    /// HTTP-date form and unparseable values return `None` (fall back to backoff).
    fn retry_after(&self, resp: &reqwest::Response) -> Option<Duration> {
        let secs: u64 = resp.headers().get("retry-after")?.to_str().ok()?.parse().ok()?;
        Some(Duration::from_secs(secs).min(self.retry.max_delay))
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
        // The body is canonical JSON (no key — the key is in the Authorization
        // header, which we never log). --debug output contains full conversation
        // content; share with care.
        tracing::debug!("→ POST {url}\n{body}");
        let resp = self.send_with_retry(&url, &body).await?;
        let status = resp.status();
        let parsed: ChatCompletionResponse = resp.json().await?;
        tracing::debug!("← {status} choices={} usage={:?}", parsed.choices.len(), parsed.usage);
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
        tracing::debug!("→ POST {url} (stream)\n{body}");
        let resp = self.send_with_retry(&url, &body).await?;
        let status = resp.status();
        tracing::debug!("← {status} streaming");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A closed port (bind + drop the listener) → connection refused → a
    /// retryable transport error (is_connect, not a timeout).
    async fn closed_port_err() -> reqwest::Error {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        reqwest::Client::new()
            .post(format!("http://{addr}"))
            .send()
            .await
            .expect_err("port is closed")
    }

    #[tokio::test]
    async fn connection_refused_is_transient_transport() {
        let err = closed_port_err().await;
        assert!(err.is_connect(), "connection refused is a connect error: {err}");
        assert!(!err.is_timeout(), "connection refused is not a timeout: {err}");
        assert!(ChatClient::is_transient_transport(&err), "should retry: {err}");
    }

    #[tokio::test]
    async fn timeout_is_not_transient_transport() {
        // A server that never responds within the 80 ms client timeout → a
        // timeout error → NOT retried (would multiply latency for a slow server).
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder().timeout(Duration::from_millis(80)).build().unwrap();
        let err = client
            .post(format!("{}/chat/completions", server.uri()))
            .send()
            .await
            .expect_err("should time out");
        assert!(err.is_timeout(), "should be a timeout: {err}");
        assert!(!ChatClient::is_transient_transport(&err), "timeout must not retry: {err}");
    }

    #[tokio::test]
    async fn retries_on_connection_refused_then_gives_up() {
        // A closed port with max_retries=2 → 3 attempts (all connection refused),
        // backing off between them. Connection refused is instant, so the backoff
        // is the only time spent — elapsed ≈ sum of backoffs, proving the
        // transport path actually retried (no-retry would be ~0 ms).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let client = ChatClient::new(format!("http://{addr}"), "k".into(), "m".into(), Duration::from_secs(60))
            .unwrap()
            .with_retry(RetryOpts {
                max_retries: 2,
                base_delay: Duration::from_millis(40),
                max_delay: Duration::from_millis(200),
            });
        let start = std::time::Instant::now();
        let err = client
            .complete(&[WireMessage::User { content: "hi".into() }], &CompleteOpts::default())
            .await
            .expect_err("closed port should fail");
        let elapsed = start.elapsed();
        assert!(err.to_string().contains("transport"), "expected an Http error: {err}");
        // 2 backoffs: 40 ms + 80 ms = 120 ms. No-retry would be ~0 ms.
        assert!(elapsed >= Duration::from_millis(80), "should have backed off (retried), took {elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "should give up fast, took {elapsed:?}");
    }
}
