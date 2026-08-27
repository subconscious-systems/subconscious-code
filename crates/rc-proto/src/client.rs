//! Non-streaming and streaming chat completions client (§3, §4.6).
//!
//! M0 added `complete` (non-streaming). M1 adds `stream` (§3.3): a streamed
//! request whose body is serialized canonically (§4.6) and whose response is
//! decoded incrementally — [`crate::stream::SseDecoder`] →
//! [`crate::stream::StreamFuser`] → [`EventStream`] of [`AgentStreamEvent`].
//! `rustls-tls` (no openssl) keeps the binary self-contained (§0).

use crate::dlr::{DlrMode, DlrTransport};
use crate::error::ProtoError;
use crate::stream::{AgentStreamEvent, SseDecoder, StreamFuser};
use crate::wire::{
    ChatCompletionResponse, StreamOptions, ToolChoiceValue, ToolDefinition, WireMessage,
};
use bytes::Bytes;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_stream::Stream;

/// Per-call options. The agent loop (M1) fills these from capabilities + config.
#[derive(Debug, Clone, Default)]
pub struct CompleteOpts {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// OpenAI-compatible reasoning posture (`high`, `max`, ...). `None` omits
    /// the field for providers that do not implement reasoning controls.
    pub reasoning_effort: Option<String>,
    /// Stable Subconscious Code session identity. Sent as an HTTP header rather
    /// than a body field so correlation never changes canonical prompt bytes or
    /// prefix-cache behavior.
    pub session_id: Option<String>,
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

/// Sizes for one encoded request. `json_bytes` is the canonical body before
/// optional gzip; `wire_bytes` is what reqwest uploads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestPayloadStats {
    pub json_bytes: usize,
    pub wire_bytes: usize,
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
    gzip_request: AtomicBool,
    dlr: Option<DlrTransport>,
}

/// Bytes of a request body written to the `--debug` log before eliding the rest.
///
/// The body is the entire conversation. Logging it whole was fine at chat scale
/// and is catastrophic at ours — a 200 MB context would format and write 200 MB
/// to stderr per request. Set `SC_DEBUG_FULL_BODY=1` to opt back into the full
/// dump when you genuinely need it.
const DEBUG_BODY_PREVIEW: usize = 8 * 1024;
/// Keep normal chat requests in memory, but spool genuinely large contexts so
/// retained request bytes cannot consume the editor's whole memory budget.
const REQUEST_MEMORY_BYTES: usize = 8 * 1024 * 1024;
const CLIENT_HEADER: &str = "x-subconscious-client";
const CLIENT_NAME: &str = "subconscious_code";
const SESSION_HEADER: &str = "x-subconscious-code-session-id";
const MAX_SESSION_ID_LEN: usize = 256;
static RETRY_JITTER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Serialize)]
struct BorrowedChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [WireMessage],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    stream: bool,
    #[serde(skip_serializing_if = "slice_is_empty")]
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a ToolChoiceValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<&'a StreamOptions>,
}

#[derive(serde::Serialize)]
struct BorrowedChatCompletionMetadata<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    stream: bool,
    #[serde(skip_serializing_if = "slice_is_empty")]
    tools: &'a [ToolDefinition],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a ToolChoiceValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<&'a StreamOptions>,
}

fn slice_is_empty<T>(slice: &[T]) -> bool {
    slice.is_empty()
}

struct CountingWriter<W> {
    inner: W,
    written: usize,
}

enum EncodedBody {
    Memory(Bytes),
    Spool {
        file: tempfile::NamedTempFile,
        len: usize,
        preview: Bytes,
    },
}

impl EncodedBody {
    fn len(&self) -> usize {
        match self {
            Self::Memory(bytes) => bytes.len(),
            Self::Spool { len, .. } => *len,
        }
    }

    fn preview(&self) -> &[u8] {
        match self {
            Self::Memory(bytes) => bytes,
            Self::Spool { preview, .. } => preview,
        }
    }

    fn is_spooled(&self) -> bool {
        matches!(self, Self::Spool { .. })
    }

    async fn request_body(&self) -> Result<reqwest::Body, ProtoError> {
        match self {
            Self::Memory(bytes) => Ok(reqwest::Body::from(bytes.clone())),
            Self::Spool { file, .. } => {
                let file = tokio::fs::File::open(file.path()).await?;
                Ok(reqwest::Body::wrap_stream(
                    tokio_util::io::ReaderStream::new(file),
                ))
            }
        }
    }
}

/// A serde target that promotes from one bounded Vec to a temporary file. The
/// first bytes are retained separately for debug diagnostics; retry attempts
/// reopen the immutable spool and stream it without rebuilding the request.
struct SpoolWriter {
    memory: Vec<u8>,
    file: Option<tempfile::NamedTempFile>,
    preview: Vec<u8>,
    threshold: usize,
    total: usize,
}

impl SpoolWriter {
    fn new(threshold: usize) -> Self {
        Self {
            memory: Vec::with_capacity(threshold.min(256 * 1024)),
            file: None,
            preview: Vec::with_capacity(DEBUG_BODY_PREVIEW),
            threshold,
            total: 0,
        }
    }

    fn finish(self) -> std::io::Result<EncodedBody> {
        if let Some(mut file) = self.file {
            std::io::Write::flush(&mut file)?;
            return Ok(EncodedBody::Spool {
                file,
                len: self.total,
                preview: Bytes::from(self.preview),
            });
        }
        Ok(EncodedBody::Memory(Bytes::from(self.memory)))
    }

    fn promote(&mut self) -> std::io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let mut file = tempfile::Builder::new().prefix("sc-request-").tempfile()?;
        std::io::Write::write_all(&mut file, &self.memory)?;
        self.memory.clear();
        self.memory.shrink_to_fit();
        self.file = Some(file);
        Ok(())
    }
}

impl std::io::Write for SpoolWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let preview_left = DEBUG_BODY_PREVIEW.saturating_sub(self.preview.len());
        self.preview
            .extend_from_slice(&bytes[..bytes.len().min(preview_left)]);
        if self.file.is_none() && self.total.saturating_add(bytes.len()) > self.threshold {
            self.promote()?;
        }
        if let Some(file) = &mut self.file {
            std::io::Write::write_all(file, bytes)?;
        } else {
            self.memory.extend_from_slice(bytes);
        }
        self.total = self.total.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().map_or(Ok(()), std::io::Write::flush)
    }
}

impl<W: std::io::Write> std::io::Write for CountingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl ChatClient {
    /// `api_key` empty → `NoApiKey` immediately, so the caller surfaces a clear
    /// error before any network attempt.
    ///
    /// `timeout` bounds the *total* request (connect → end of body).
    /// **`None` disables it**, which is the Subconscious Code default: a total
    /// budget also covers the upload, so on a very large body it can expire
    /// mid-upload and trigger a retry that re-uploads from scratch. Liveness is
    /// better served by the idle bound in [`CompleteOpts::idle_timeout`], which
    /// fails fast on a *stalled* stream without penalizing a merely large one.
    ///
    /// When set, a small value can cut a stream mid-tool-call (the loop
    /// synthesizes `Interrupted` results for any outstanding call — see
    /// `rc_core::agent`).
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        timeout: Option<Duration>,
    ) -> Result<Self, ProtoError> {
        if api_key.is_empty() {
            return Err(ProtoError::NoApiKey);
        }
        let mut builder = reqwest::Client::builder();
        if let Some(t) = timeout {
            builder = builder.timeout(t);
        }
        let http = builder.build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            retry: RetryOpts::default(),
            gzip_request: AtomicBool::new(false),
            dlr: None,
        })
    }

    /// Set the retry policy for transient HTTP errors (429/5xx). Default is no
    /// retry. The body is immutable, so retrying is safe and cheap: small bodies
    /// clone `Bytes`, while large bodies reopen their exact spool. A streaming
    /// request is retried only before the body starts flowing; a mid-stream
    /// error is not retried (no resume). Builder.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryOpts) -> Self {
        self.retry = retry;
        self
    }

    /// Compress the request body with gzip and set `Content-Encoding: gzip`.
    ///
    /// Off by default because the gateway must support it — a server that
    /// ignores `Content-Encoding` will read the compressed bytes as JSON and
    /// reject the request. At our body sizes wire time dominates and JSON-wrapped
    /// source compresses roughly 5-10×, so it is worth confirming support and
    /// turning on. Builder.
    #[must_use]
    pub fn with_request_gzip(self, on: bool) -> Self {
        self.gzip_request.store(on, Ordering::Relaxed);
        self
    }

    /// Route request bodies through a stateful DLR sidecar. The sidecar
    /// reconstructs ordinary OpenAI JSON next to the gateway; the gateway and
    /// model runtime require no changes.
    pub fn with_dlr(
        mut self,
        sidecar_url: String,
        ingress_token: Option<String>,
        mode: DlrMode,
        repair_margin_pct: u32,
    ) -> Result<Self, ProtoError> {
        self.dlr = Some(DlrTransport::new(
            sidecar_url,
            ingress_token,
            mode,
            repair_margin_pct,
        )?);
        // DLR frames already compress message blocks; HTTP gzip applies only
        // to the fallback JSON path.
        Ok(self)
    }

    async fn send_model_request<T: serde::Serialize, M: serde::Serialize>(
        &self,
        url: &str,
        request: &T,
        metadata: &M,
        messages: &[WireMessage],
        session_id: Option<&str>,
        streaming: bool,
    ) -> Result<(reqwest::Response, u32, RequestPayloadStats), (ProtoError, u32, RequestPayloadStats)>
    {
        if let Some(dlr) = &self.dlr {
            let metadata = serde_json::to_value(metadata)
                .map_err(|error| (ProtoError::Json(error), 0, RequestPayloadStats::default()))?;
            if let Some((response, payload)) = dlr
                .send(session_id, messages, metadata, &self.api_key)
                .await
                .map_err(|error| (error, 0, RequestPayloadStats::default()))?
            {
                let status = response.status();
                if status.is_success() {
                    return Ok((response, 0, payload));
                }
                let body = response.text().await.unwrap_or_default();
                return Err((
                    ProtoError::Status {
                        status: status.as_u16(),
                        body,
                    },
                    0,
                    payload,
                ));
            }
        }
        self.encode_and_send(url, request, session_id, streaming)
            .await
    }

    /// Encode and send once, falling back to the canonical uncompressed body
    /// when a gateway explicitly rejects or clearly fails to decode gzip. The
    /// capability result is remembered for this client, so an incompatible
    /// custom endpoint pays the probe only on its first request.
    async fn encode_and_send<T: serde::Serialize>(
        &self,
        url: &str,
        request: &T,
        session_id: Option<&str>,
        streaming: bool,
    ) -> Result<(reqwest::Response, u32, RequestPayloadStats), (ProtoError, u32, RequestPayloadStats)>
    {
        let gzip = self.gzip_request.load(Ordering::Relaxed);
        let (body, payload) = self
            .encode_body_with(request, gzip)
            .map_err(|error| (error, 0, RequestPayloadStats::default()))?;
        self.log_request(url, &body, streaming);
        match self.send_with_retry(url, &body, session_id, gzip).await {
            Ok((response, retries)) => Ok((response, retries, payload)),
            Err((error, retries)) if gzip && gzip_is_unsupported(&error) => {
                tracing::warn!(error = %error, "gateway rejected gzip; retrying once uncompressed and disabling gzip for this client");
                self.gzip_request.store(false, Ordering::Relaxed);
                let (raw_body, raw_payload) = self
                    .encode_body_with(request, false)
                    .map_err(|error| (error, retries, RequestPayloadStats::default()))?;
                self.log_request(url, &raw_body, streaming);
                match self
                    .send_with_retry(url, &raw_body, session_id, false)
                    .await
                {
                    Ok((response, raw_retries)) => Ok((
                        response,
                        retries.saturating_add(1).saturating_add(raw_retries),
                        raw_payload,
                    )),
                    Err((error, raw_retries)) => Err((
                        error,
                        retries.saturating_add(1).saturating_add(raw_retries),
                        raw_payload,
                    )),
                }
            }
            Err((error, retries)) => Err((error, retries, payload)),
        }
    }

    /// POST `body` to `url`, retrying on a transient error up to
    /// `retry.max_retries` times with exponential backoff. A transient error is
    /// a status of 429/5xx, or a *connection* error (DNS/TCP-refused/TLS) that
    /// isn't a timeout. Timeouts are NOT retried — a retry would multiply
    /// worst-case latency (`max_retries × total timeout`) for a merely-slow
    /// server. Returns `(2xx Response, retries_used)` on success — `retries_used`
    /// is the number of retries before the successful response (0 for a clean
    /// first attempt), surfaced to the host so a `Turn::Error` can record how
    /// hard the harness tried. On failure returns `(error, retries_used)` so
    /// the final error still carries the retry count.
    /// `body` is either refcounted bytes or a disk spool, so an attempt never
    /// retains another full payload in memory.
    async fn send_with_retry(
        &self,
        url: &str,
        body: &EncodedBody,
        session_id: Option<&str>,
        gzip: bool,
    ) -> Result<(reqwest::Response, u32), (ProtoError, u32)> {
        if session_id.is_some_and(|id| {
            id.is_empty()
                || id.len() > MAX_SESSION_ID_LEN
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        }) {
            return Err((ProtoError::InvalidSessionId, 0));
        }
        let mut attempt = 0u32;
        loop {
            let mut req = self
                .http
                .post(url)
                .bearer_auth(self.api_key.clone())
                .header("content-type", "application/json")
                .header("content-length", body.len())
                .header(CLIENT_HEADER, CLIENT_NAME);
            if let Some(session_id) = session_id {
                req = req.header(SESSION_HEADER, session_id);
            }
            if gzip {
                req = req.header("content-encoding", "gzip");
            }
            let request_body = match body.request_body().await {
                Ok(body) => body,
                Err(error) => return Err((error, attempt)),
            };
            let resp = match req.body(request_body).send().await {
                Ok(r) => r,
                Err(e) => {
                    if Self::is_transient_transport(&e) && attempt < self.retry.max_retries {
                        let d = self.backoff(attempt);
                        tracing::warn!(error = %e, attempt, "transient transport error; retrying in {d:?}");
                        tokio::time::sleep(d).await;
                        attempt += 1;
                        continue;
                    }
                    return Err((ProtoError::Http(e), attempt));
                }
            };
            let status = resp.status();
            if status.is_success() {
                return Ok((resp, attempt));
            }
            let transient = matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504);
            if transient && attempt < self.retry.max_retries {
                // Honor `Retry-After` (seconds) if the server sent one, capped at
                // max_delay; else exponential backoff.
                let d = self
                    .retry_after(&resp)
                    .unwrap_or_else(|| self.backoff(attempt));
                tracing::warn!(
                    status = status.as_u16(),
                    attempt,
                    "transient HTTP error; retrying in {d:?}"
                );
                tokio::time::sleep(d).await;
                attempt += 1;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            tracing::debug!("← {status}\n{text}");
            return Err((
                ProtoError::Status {
                    status: status.as_u16(),
                    body: text,
                },
                attempt,
            ));
        }
    }

    /// Exponential backoff capped at `max_delay`, with equal-ish jitter in the
    /// upper quarter of the window. Keeping a nonzero floor avoids retry storms
    /// while the per-process sequence and wall clock prevent hundreds of
    /// benchmark workers from waking on the same millisecond.
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u64 << attempt.min(20);
        let ceiling = self
            .retry
            .base_delay
            .saturating_mul(factor as u32)
            .min(self.retry.max_delay);
        let nanos = ceiling.as_nanos().min(u128::from(u64::MAX)) as u64;
        if nanos < 4 {
            return ceiling;
        }
        let spread = nanos / 4;
        let floor = nanos - spread;
        let sequence = RETRY_JITTER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut mixed = epoch ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        mixed ^= mixed >> 30;
        mixed = mixed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed ^= mixed >> 27;
        mixed = mixed.wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^= mixed >> 31;
        Duration::from_nanos(floor + mixed % (spread + 1))
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
        let secs: u64 = resp
            .headers()
            .get("retry-after")?
            .to_str()
            .ok()?
            .parse()
            .ok()?;
        Some(Duration::from_secs(secs).min(self.retry.max_delay))
    }

    /// Serialize a request to the exact bytes that go on the wire — once.
    ///
    /// Without gzip, one deterministic serialization is handed to `Bytes` or a
    /// spool without a second copy. With gzip, serde writes directly into the
    /// compressor, avoiding simultaneous raw-JSON and compressed buffers. The
    /// old path built a `Value` tree, a canonicalized copy, and then a `String`.
    ///
    /// This removes serialization as a memory multiplier; it does not make the
    /// whole request path allocation-free. The ~6× peak RSS measured for a 12 MB
    /// body was taken before the `Arc<str>` change to context assembly (which
    /// made turn→wire projection a refcount bump); the new figure has not been
    /// re-measured. Either way, what's left is upstream of this function, not
    /// in it.
    fn encode_body_with<T: serde::Serialize>(
        &self,
        req: &T,
        gzip: bool,
    ) -> Result<(EncodedBody, RequestPayloadStats), ProtoError> {
        if !gzip {
            let mut writer = SpoolWriter::new(REQUEST_MEMORY_BYTES);
            serde_json::to_writer(&mut writer, req)?;
            let json_bytes = writer.total;
            let body = writer.finish()?;
            return Ok((
                body,
                RequestPayloadStats {
                    json_bytes,
                    wire_bytes: json_bytes,
                },
            ));
        }
        // Stream serde directly through gzip. The previous path first retained
        // the entire raw JSON Vec and then allocated a second compressed Vec;
        // large-context requests therefore paid for both bodies concurrently.
        let encoder = flate2::write::GzEncoder::new(
            SpoolWriter::new(REQUEST_MEMORY_BYTES),
            flate2::Compression::fast(),
        );
        let mut writer = CountingWriter {
            inner: encoder,
            written: 0,
        };
        serde_json::to_writer(&mut writer, req)?;
        let json_bytes = writer.written;
        let compressed = writer.inner.finish().map_err(ProtoError::Gzip)?;
        let body = compressed.finish()?;
        tracing::debug!(
            raw = json_bytes,
            compressed = body.len(),
            spooled = body.is_spooled(),
            "gzipped request body"
        );
        let wire_bytes = body.len();
        Ok((
            body,
            RequestPayloadStats {
                json_bytes,
                wire_bytes,
            },
        ))
    }

    /// Log a request under `--debug` without dumping the entire conversation.
    ///
    /// Emits the byte count always and a bounded head of the body, since the
    /// interesting part (model, tools, the leading messages) is at the front.
    /// `SC_DEBUG_FULL_BODY=1` restores the unbounded dump. Never logs the API
    /// key — that lives in the `Authorization` header, which is not touched here.
    fn log_request(&self, url: &str, body: &EncodedBody, streaming: bool) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let tag = if streaming { " (stream)" } else { "" };
        let full = std::env::var("SC_DEBUG_FULL_BODY").as_deref() == Ok("1");
        let preview = body.preview();
        if (full && !body.is_spooled()) || body.len() <= DEBUG_BODY_PREVIEW {
            tracing::debug!(
                "→ POST {url}{tag} [{} bytes]\n{}",
                body.len(),
                String::from_utf8_lossy(preview)
            );
            return;
        }
        let cut = floor_char_boundary(preview, DEBUG_BODY_PREVIEW);
        tracing::debug!(
            "→ POST {url}{tag} [{} bytes, showing first {cut}; large spooled bodies stay preview-only]\n{}",
            body.len(),
            String::from_utf8_lossy(&preview[..cut])
        );
    }

    /// One non-streaming round trip. Returns the parsed response.
    pub async fn complete(
        &self,
        messages: &[WireMessage],
        opts: &CompleteOpts,
    ) -> Result<ChatCompletionResponse, ProtoError> {
        let req = BorrowedChatCompletionRequest {
            model: &self.model,
            messages,
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
            reasoning_effort: opts.reasoning_effort.as_deref(),
            stream: false,
            tools: &[],
            tool_choice: None,
            parallel_tool_calls: None,
            stream_options: None,
        };
        let metadata = BorrowedChatCompletionMetadata {
            model: &self.model,
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
            reasoning_effort: opts.reasoning_effort.as_deref(),
            stream: false,
            tools: &[],
            tool_choice: None,
            parallel_tool_calls: None,
            stream_options: None,
        };
        let url = format!("{}/chat/completions", self.base_url);
        // `complete` is the non-streaming path (doctor / tests); it discards the
        // wire retry count. The agent loop uses `stream` (via `ChatModel`), which
        // surfaces retries to the host.
        let (resp, _retries, _payload) = self
            .send_model_request(
                &url,
                &req,
                &metadata,
                messages,
                opts.session_id.as_deref(),
                false,
            )
            .await
            .map_err(|(error, _, _)| error)?;
        let status = resp.status();
        let parsed: ChatCompletionResponse = resp.json().await?;
        tracing::debug!(
            "← {status} choices={} usage={:?}",
            parsed.choices.len(),
            parsed.usage
        );
        if parsed.choices.is_empty() {
            return Err(ProtoError::EmptyChoices);
        }
        Ok(parsed)
    }

    /// One streaming round trip. Returns an [`AgentStreamEvent`] stream and the
    /// number of wire-layer retries (429/5xx) the request survived (0 for a clean
    /// first attempt), surfaced to the host via `EventSink::on_retry` and
    /// persisted on a `Turn::Error` if the request ultimately fails. The request
    /// body is canonical (§4.6); tool calls are reassembled by the
    /// [`crate::stream::ToolCallAccumulator`] and emitted (parsed, or as a
    /// parse error for the loop to feed back, §3.3). `stream_options` requests
    /// the trailing usage chunk (§3.6).
    pub async fn stream(
        &self,
        messages: &[WireMessage],
        opts: &CompleteOpts,
        tools: &[ToolDefinition],
    ) -> Result<
        (
            Pin<Box<dyn Stream<Item = Result<AgentStreamEvent, ProtoError>> + Send>>,
            u32,
            RequestPayloadStats,
        ),
        (ProtoError, u32, RequestPayloadStats),
    > {
        let tool_choice = ToolChoiceValue::Auto;
        let stream_options = StreamOptions {
            include_usage: true,
        };
        let req = BorrowedChatCompletionRequest {
            model: &self.model,
            messages,
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
            reasoning_effort: opts.reasoning_effort.as_deref(),
            stream: true,
            tools,
            tool_choice: if tools.is_empty() {
                None
            } else {
                Some(&tool_choice)
            },
            parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
            stream_options: Some(&stream_options),
        };
        let metadata = BorrowedChatCompletionMetadata {
            model: &self.model,
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
            reasoning_effort: opts.reasoning_effort.as_deref(),
            stream: true,
            tools,
            tool_choice: if tools.is_empty() {
                None
            } else {
                Some(&tool_choice)
            },
            parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
            stream_options: Some(&stream_options),
        };
        let url = format!("{}/chat/completions", self.base_url);
        let (resp, retries, payload) = self
            .send_model_request(
                &url,
                &req,
                &metadata,
                messages,
                opts.session_id.as_deref(),
                true,
            )
            .await?;
        let status = resp.status();
        tracing::debug!("← {status} streaming (retries={retries})");
        let body: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
            Box::pin(resp.bytes_stream());
        Ok((Box::pin(EventStream::new(body)), retries, payload))
    }
}

fn gzip_is_unsupported(error: &ProtoError) -> bool {
    let ProtoError::Status { status, body } = error else {
        return false;
    };
    if *status == 415 {
        return true;
    }
    if *status != 400 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("content-encoding")
        || body.contains("gzip")
        || body.contains("compressed")
        || body.contains("invalid utf-8")
        || body.contains("expected value at line 1 column 1")
}

/// Largest index `≤ cap` in `bytes` that isn't inside a UTF-8 sequence, so a
/// preview slice is lossless. A continuation byte matches `0b10xxxxxx`; walking
/// back past at most three of them reaches a lead byte.
fn floor_char_boundary(bytes: &[u8], cap: usize) -> usize {
    if cap >= bytes.len() {
        return bytes.len();
    }
    let mut cut = cap;
    while cut > 0 && bytes[cut] & 0xC0 == 0x80 {
        cut -= 1;
    }
    cut
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
                    let had_pending = !self.pending.is_empty();
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
                    // Parsed model events reset the core idle timer themselves.
                    // When a non-empty network frame produced no semantic event
                    // (an SSE comment or a partial `data:` line), emit an
                    // internal-only liveness event instead. Without this,
                    // Orange Line can send a heartbeat every 15 seconds while
                    // SC still reports an exact 120-second stream stall.
                    if !bytes.is_empty() && !had_pending && self.pending.is_empty() {
                        return Poll::Ready(Some(Ok(AgentStreamEvent::TransportActivity)));
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
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn sse_comment_becomes_transport_activity() {
        let body = tokio_stream::iter(vec![Ok::<Bytes, reqwest::Error>(Bytes::from_static(
            b": heartbeat\n\n",
        ))]);
        let mut stream = EventStream::new(Box::pin(body));

        assert!(matches!(
            stream.next().await,
            Some(Ok(AgentStreamEvent::TransportActivity))
        ));
        assert!(matches!(
            stream.next().await,
            Some(Ok(AgentStreamEvent::Finish {
                reason: crate::stream::FinishReason::Other(reason)
            })) if reason == "stream-ended"
        ));
    }

    #[test]
    fn request_writer_spools_over_threshold_and_preserves_exact_bytes() {
        use std::io::Write;
        let mut writer = SpoolWriter::new(32);
        writer.write_all(b"0123456789abcdef").unwrap();
        writer
            .write_all(b"--this crosses the in-memory boundary--")
            .unwrap();
        let body = writer.finish().unwrap();
        assert!(body.is_spooled());
        let EncodedBody::Spool { file, len, .. } = body else {
            unreachable!()
        };
        let bytes = std::fs::read(file.path()).unwrap();
        assert_eq!(
            bytes,
            b"0123456789abcdef--this crosses the in-memory boundary--"
        );
        assert_eq!(len, bytes.len());
    }

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
        assert!(
            err.is_connect(),
            "connection refused is a connect error: {err}"
        );
        assert!(
            !err.is_timeout(),
            "connection refused is not a timeout: {err}"
        );
        assert!(
            ChatClient::is_transient_transport(&err),
            "should retry: {err}"
        );
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(80))
            .build()
            .unwrap();
        let err = client
            .post(format!("{}/chat/completions", server.uri()))
            .send()
            .await
            .expect_err("should time out");
        assert!(err.is_timeout(), "should be a timeout: {err}");
        assert!(
            !ChatClient::is_transient_transport(&err),
            "timeout must not retry: {err}"
        );
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
        let client = ChatClient::new(
            format!("http://{addr}"),
            "k".into(),
            "m".into(),
            Some(Duration::from_secs(60)),
        )
        .unwrap()
        .with_retry(RetryOpts {
            max_retries: 2,
            base_delay: Duration::from_millis(40),
            max_delay: Duration::from_millis(200),
        });
        let start = std::time::Instant::now();
        let err = client
            .complete(
                &[WireMessage::User {
                    content: "hi".into(),
                }],
                &CompleteOpts::default(),
            )
            .await
            .expect_err("closed port should fail");
        let elapsed = start.elapsed();
        assert!(
            err.to_string().contains("transport"),
            "expected an Http error: {err}"
        );
        // 2 backoffs: 40 ms + 80 ms = 120 ms. No-retry would be ~0 ms.
        assert!(
            elapsed >= Duration::from_millis(80),
            "should have backed off (retried), took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "should give up fast, took {elapsed:?}"
        );
    }

    #[test]
    fn retry_jitter_stays_inside_the_upper_quarter_of_each_window() {
        let client = ChatClient::new("http://127.0.0.1".into(), "k".into(), "m".into(), None)
            .unwrap()
            .with_retry(RetryOpts {
                max_retries: 3,
                base_delay: Duration::from_millis(100),
                max_delay: Duration::from_millis(250),
            });

        for _ in 0..32 {
            assert!((Duration::from_millis(75)..=Duration::from_millis(100))
                .contains(&client.backoff(0)));
            assert!((Duration::from_millis(150)..=Duration::from_millis(200))
                .contains(&client.backoff(1)));
            assert!(
                (Duration::from_micros(187_500)..=Duration::from_millis(250))
                    .contains(&client.backoff(9))
            );
        }
    }
}
