//! Deployable DLR HTTP sidecar.
//!
//! The sidecar terminates stateful DLR APPEND frames, reconstructs the
//! OpenAI-compatible message list beside the gateway, and forwards an ordinary
//! `/v1/chat/completions` request. The existing gateway and model runtime do
//! not need to understand DLR. Upstream responses are streamed back without
//! buffering, so SSE token latency is unchanged.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dlr_compress::Compressor;
use dlr_core::{
    decode_frame_bytes, encode_frame, from_canonical_owned, AckFrame, AppendFrame, Block, BlockId,
    BlockKind, ContentStore, Frame, FrameBlock, MerkleRoot,
};
use dlr_receiver::{Receiver, ReceiverError};
use dlr_shim::SessionShim;
use futures_util::{stream, StreamExt};
use reqwest::Body as RequestBody;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const PROTOCOL_VERSION: u16 = 1;
pub const CONTENT_TYPE: &str = "application/vnd.dlr.chat+binary; version=1";
pub const FRAME_CONTENT_TYPE: &str = "application/vnd.dlr.frame+binary; version=1";
pub const ACK_ROOT_HEADER: &str = "x-dlr-ack-root";
pub const CURRENT_ROOT_HEADER: &str = "x-dlr-current-root";
pub const SIDECAR_TOKEN_HEADER: &str = "x-dlr-sidecar-token";
const MAGIC: &[u8; 4] = b"DLR1";
const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const PROJECTION_CHUNK_BYTES: usize = 64 * 1024;
const PROJECTION_CACHE_JSON_BYTES: usize = 256 * 1024 * 1024;
const PROJECTION_CACHE_SESSIONS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("invalid DLR envelope: {0}")]
    Envelope(&'static str),
    #[error("invalid request metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("invalid DLR frame: {0}")]
    Frame(String),
    #[error("receiver: {0}")]
    Receiver(#[from] ReceiverError),
    #[error("request metadata must be a JSON object")]
    RequestNotObject,
    #[error("request metadata must not contain messages")]
    MessagesInMetadata,
    #[error("session block {0} is not a valid OpenAI message object")]
    InvalidMessage(usize),
    #[error("a prepared chat append is already awaiting an ACK")]
    RequestInFlight,
    #[error("invalid x-dlr-ack-root header")]
    InvalidAckHeader,
    #[error("sidecar ACK root did not match the prepared request")]
    UnexpectedAckRoot,
    #[error("unexpected DLR frame during synchronization")]
    UnexpectedFrame,
    #[error("sidecar returned HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },
    #[error("upstream transport: {0}")]
    Upstream(#[from] reqwest::Error),
    #[error("durability: {0}")]
    Durability(#[from] io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMetadata {
    /// OpenAI-compatible request fields excluding `messages`.
    pub request: Value,
}

#[derive(Debug, Clone)]
pub struct ChatEnvelope {
    pub metadata: ChatMetadata,
    pub frame: Bytes,
}

impl ChatEnvelope {
    pub fn encode(&self) -> Result<Bytes, SidecarError> {
        let metadata = serde_json::to_vec(&self.metadata)?;
        Self::encode_parts(&metadata, &self.frame)
    }

    fn encode_parts(metadata: &[u8], frame: &Bytes) -> Result<Bytes, SidecarError> {
        if metadata.len() > MAX_METADATA_BYTES {
            return Err(SidecarError::Envelope("metadata exceeds 2 MiB"));
        }
        let total = 4usize
            .checked_add(2)
            .and_then(|n| n.checked_add(4))
            .and_then(|n| n.checked_add(metadata.len()))
            .and_then(|n| n.checked_add(frame.len()))
            .ok_or(SidecarError::Envelope("length overflow"))?;
        if total > MAX_REQUEST_BYTES {
            return Err(SidecarError::Envelope("request exceeds 64 MiB"));
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        out.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        out.extend_from_slice(metadata);
        out.extend_from_slice(frame);
        Ok(Bytes::from(out))
    }

    pub fn decode(body: Bytes) -> Result<Self, SidecarError> {
        if body.len() < 10 || &body[..4] != MAGIC {
            return Err(SidecarError::Envelope("bad magic or truncated header"));
        }
        let version = u16::from_le_bytes([body[4], body[5]]);
        if version != PROTOCOL_VERSION {
            return Err(SidecarError::Envelope("unsupported protocol version"));
        }
        let metadata_len = u32::from_le_bytes(body[6..10].try_into().unwrap()) as usize;
        if metadata_len > MAX_METADATA_BYTES {
            return Err(SidecarError::Envelope("metadata exceeds 2 MiB"));
        }
        let frame_start = 10usize
            .checked_add(metadata_len)
            .ok_or(SidecarError::Envelope("length overflow"))?;
        if frame_start >= body.len() {
            return Err(SidecarError::Envelope("missing frame"));
        }
        let metadata = serde_json::from_slice(&body[10..frame_start])?;
        Ok(Self {
            metadata,
            frame: body.slice(frame_start..),
        })
    }
}

/// Prepared bytes remain immutable across retries. Replaying this exact object
/// cannot append session state twice. As with any chat-completions POST, an
/// ambiguous retry can still invoke the model twice; callers should only retry
/// generation according to their normal request-id/idempotency policy.
#[derive(Debug, Clone)]
pub struct PreparedChatRequest {
    pub body: Bytes,
    pub expected_root: MerkleRoot,
    /// Exact JSON bytes for the request metadata object (without messages).
    pub request_json_bytes: usize,
    /// Sum of exact serialized sizes for newly appended messages.
    pub new_messages_json_bytes: usize,
}

/// Client-side state adapter for an OpenAI-compatible conversation.
pub struct ChatSession {
    shim: SessionShim,
    next_seq: u64,
    pending_root: Option<MerkleRoot>,
}

impl ChatSession {
    pub fn new(session_key: &str) -> Self {
        let sid = session_id_from_key(session_key);
        Self {
            shim: SessionShim::new(sid, ContentStore::new(), Compressor::default()),
            next_seq: 1,
            pending_root: None,
        }
    }

    pub fn session_id(&self) -> u128 {
        self.shim.session_id
    }

    pub fn root(&self) -> MerkleRoot {
        self.shim.root()
    }

    pub fn base_root(&self) -> MerkleRoot {
        self.shim.base_root()
    }

    pub fn prepare(
        &mut self,
        new_messages: &[Value],
        request_without_messages: Value,
    ) -> Result<PreparedChatRequest, SidecarError> {
        self.prepare_ref(new_messages, &request_without_messages)
    }

    /// Borrow request metadata while preparing a frame. This avoids cloning a
    /// potentially large tool-schema tree solely to keep it available for a
    /// cold-start retry.
    pub fn prepare_ref(
        &mut self,
        new_messages: &[Value],
        request_without_messages: &Value,
    ) -> Result<PreparedChatRequest, SidecarError> {
        if self.pending_root.is_some() {
            return Err(SidecarError::RequestInFlight);
        }
        let request = request_without_messages
            .as_object()
            .ok_or(SidecarError::RequestNotObject)?;
        if request.contains_key("messages") {
            return Err(SidecarError::MessagesInMetadata);
        }
        // Serialize and validate everything before mutating the shim. A bad
        // message or oversized envelope must leave local session state intact.
        let mut serialized = Vec::with_capacity(new_messages.len());
        for (index, message) in new_messages.iter().enumerate() {
            let object = message
                .as_object()
                .ok_or(SidecarError::InvalidMessage(index))?;
            validate_message_object(object, index)?;
            serialized.push((message_kind(object), serde_json::to_vec(message)?));
        }
        validate_request_value(request_without_messages)?;
        let request_json = serde_json::to_vec(request_without_messages)?;
        // Preserve the v1 envelope representation without serializing the
        // already-materialized request Value a second time.
        let mut metadata = Vec::with_capacity(request_json.len().saturating_add(12));
        metadata.extend_from_slice(b"{\"request\":");
        metadata.extend_from_slice(&request_json);
        metadata.push(b'}');
        let metadata_len = metadata.len();
        if metadata_len > MAX_METADATA_BYTES {
            return Err(SidecarError::Envelope("metadata exceeds 2 MiB"));
        }
        // The compressor never emits more than canonical_len + one marker
        // byte. Bound the complete envelope before touching shim state so an
        // oversized request cannot partially advance the client session.
        let worst_case_size = serialized.iter().try_fold(
            10usize
                .checked_add(metadata_len)
                .and_then(|size| size.checked_add(53))
                .ok_or(SidecarError::Envelope("length overflow"))?,
            |size, (_, payload)| {
                size.checked_add(39)
                    .and_then(|size| size.checked_add(payload.len()))
                    .ok_or(SidecarError::Envelope("length overflow"))
            },
        )?;
        if worst_case_size > MAX_REQUEST_BYTES {
            return Err(SidecarError::Envelope("request exceeds 64 MiB"));
        }

        let new_messages_json_bytes = serialized.iter().map(|(_, payload)| payload.len()).sum();
        let mut blocks = Vec::with_capacity(serialized.len());
        for (kind, payload) in serialized {
            blocks.push(Block::new(kind, self.next_seq, payload));
            self.next_seq = self.next_seq.saturating_add(1);
        }
        let append = self.shim.ingest_turn(blocks);
        let expected_root = self.shim.root();
        let frame = encode_frame(&Frame::Append(append));
        let body = ChatEnvelope::encode_parts(&metadata, &frame)?;
        self.pending_root = Some(expected_root);
        Ok(PreparedChatRequest {
            body,
            expected_root,
            request_json_bytes: request_json.len(),
            new_messages_json_bytes,
        })
    }

    pub fn apply_ack(&mut self, root: MerkleRoot) -> bool {
        let applied = self.shim.apply_ack(root);
        if applied && self.pending_root == Some(root) {
            self.pending_root = None;
        }
        applied
    }

    pub fn has_pending_request(&self) -> bool {
        self.pending_root.is_some()
    }

    pub fn resync_frame(&self) -> Bytes {
        encode_frame(&Frame::Resync(self.shim.resync_frame()))
    }

    pub fn bulk_frames_for(
        &self,
        missing: &[[u8; 32]],
        repair_margin_pct: u32,
    ) -> Result<Vec<Bytes>, SidecarError> {
        self.shim
            .bulk_frames_for(missing, repair_margin_pct)
            .map(|frames| {
                frames
                    .into_iter()
                    .map(|frame| encode_frame(&Frame::Bulk(frame)))
                    .collect()
            })
            .map_err(|error| SidecarError::Frame(error.to_string()))
    }
}

/// Network driver for [`ChatSession`]. It preserves streaming by returning the
/// upstream `reqwest::Response` without buffering its body.
#[derive(Clone)]
pub struct DlrChatClient {
    base_url: Arc<str>,
    ingress_token: Option<Arc<str>>,
    http: reqwest::Client,
}

impl DlrChatClient {
    pub fn new(base_url: String, ingress_token: Option<String>) -> Result<Self, SidecarError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(600))
            .tcp_keepalive(Duration::from_secs(30))
            .http2_adaptive_window(true)
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').into(),
            ingress_token: ingress_token.map(Into::into),
            http,
        })
    }

    pub async fn send_chat(
        &self,
        session: &mut ChatSession,
        prepared: &PreparedChatRequest,
        headers: HeaderMap,
    ) -> Result<reqwest::Response, SidecarError> {
        let mut request = self
            .http
            .post(format!("{}/v1/dlr/chat/completions", self.base_url))
            .headers(headers)
            .header(header::CONTENT_TYPE, CONTENT_TYPE)
            .body(prepared.body.clone());
        if let Some(token) = self.ingress_token.as_deref() {
            request = request.header(SIDECAR_TOKEN_HEADER, token);
        }
        let response = request.send().await?;
        if let Some(value) = response.headers().get(ACK_ROOT_HEADER) {
            let root = value
                .to_str()
                .ok()
                .and_then(parse_root_hex)
                .ok_or(SidecarError::InvalidAckHeader)?;
            if root != prepared.expected_root || !session.apply_ack(root) {
                return Err(SidecarError::UnexpectedAckRoot);
            }
        }
        Ok(response)
    }

    pub async fn synchronize(
        &self,
        session: &mut ChatSession,
        repair_margin_pct: u32,
    ) -> Result<(), SidecarError> {
        match self.exchange_frame(session.resync_frame()).await? {
            Some(Frame::Ack(ack)) => self.apply_protocol_ack(session, ack),
            Some(Frame::Missing(missing)) if missing.session_id == session.session_id() => {
                let mut completion = None;
                for frame in session.bulk_frames_for(&missing.missing, repair_margin_pct)? {
                    if let Some(Frame::Ack(ack)) = self.exchange_frame(frame).await? {
                        completion = Some(ack);
                    }
                }
                completion
                    .ok_or(SidecarError::UnexpectedFrame)
                    .and_then(|ack| self.apply_protocol_ack(session, ack))
            }
            _ => Err(SidecarError::UnexpectedFrame),
        }
    }

    async fn exchange_frame(&self, frame: Bytes) -> Result<Option<Frame>, SidecarError> {
        let mut request = self
            .http
            .post(format!("{}/v1/dlr/frame", self.base_url))
            .header(header::CONTENT_TYPE, FRAME_CONTENT_TYPE)
            .body(frame);
        if let Some(token) = self.ingress_token.as_deref() {
            request = request.header(SIDECAR_TOKEN_HEADER, token);
        }
        let response = request.send().await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(SidecarError::HttpStatus { status, message });
        }
        let body = response.bytes().await?;
        decode_frame_bytes(&body)
            .map(Some)
            .map_err(|error| SidecarError::Frame(error.to_string()))
    }

    fn apply_protocol_ack(
        &self,
        session: &mut ChatSession,
        ack: AckFrame,
    ) -> Result<(), SidecarError> {
        if ack.session_id != session.session_id() || !session.apply_ack(ack.root) {
            return Err(SidecarError::UnexpectedAckRoot);
        }
        Ok(())
    }
}

fn message_kind(message: &Map<String, Value>) -> BlockKind {
    match message.get("role").and_then(Value::as_str) {
        Some("system") | Some("developer") => BlockKind::System,
        Some("tool") => BlockKind::ToolResult,
        Some("assistant") if message.get("tool_calls").is_some() => BlockKind::ToolCall,
        _ => BlockKind::Message,
    }
}

pub fn session_id_from_key(key: &str) -> u128 {
    let digest = blake3::hash(key.as_bytes());
    u128::from_le_bytes(digest.as_bytes()[..16].try_into().unwrap())
}

#[derive(Clone)]
pub struct SidecarState {
    receiver: Arc<Receiver>,
    upstream_url: Arc<str>,
    http: reqwest::Client,
    ingress_token: Option<Arc<str>>,
    sync_wal: bool,
    /// Validated message payloads at an ACKed root. Besides avoiding an
    /// O(history) JSON scan, this skips the store's history-wide block-id
    /// lookup/reconstruction pass on every steady APPEND.
    chat_projections: Arc<std::sync::Mutex<ChatProjectionCache>>,
}

#[derive(Clone)]
struct CachedChatProjection {
    root: MerkleRoot,
    message_count: usize,
    /// JSON message-array contents, including inter-message commas. Small
    /// blocks are coalesced to avoid issuing thousands of tiny HTTP chunks;
    /// large blocks remain their original refcounted `Bytes` allocation.
    chunks: Arc<Vec<Bytes>>,
    json_bytes: usize,
}

#[derive(Default)]
struct ChatProjectionCache {
    entries: HashMap<u128, CachedChatProjection>,
    lru: VecDeque<u128>,
    json_bytes: usize,
}

impl ChatProjectionCache {
    fn get(&mut self, session_id: u128, root: MerkleRoot) -> Option<CachedChatProjection> {
        let projection = self
            .entries
            .get(&session_id)
            .filter(|projection| projection.root == root)
            .cloned()?;
        self.touch(session_id);
        Some(projection)
    }

    fn insert(&mut self, session_id: u128, projection: CachedChatProjection) {
        self.remove(session_id);
        while !self.entries.is_empty()
            && (self.entries.len() >= PROJECTION_CACHE_SESSIONS
                || self.json_bytes.saturating_add(projection.json_bytes)
                    > PROJECTION_CACHE_JSON_BYTES)
        {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.json_bytes = self.json_bytes.saturating_sub(evicted.json_bytes);
            }
        }
        self.json_bytes = self.json_bytes.saturating_add(projection.json_bytes);
        self.entries.insert(session_id, projection);
        self.lru.push_back(session_id);
    }

    fn remove(&mut self, session_id: u128) {
        if let Some(removed) = self.entries.remove(&session_id) {
            self.json_bytes = self.json_bytes.saturating_sub(removed.json_bytes);
        }
        self.lru.retain(|id| *id != session_id);
    }

    fn touch(&mut self, session_id: u128) {
        self.lru.retain(|id| *id != session_id);
        self.lru.push_back(session_id);
    }
}

impl SidecarState {
    pub fn new(
        receiver: Receiver,
        upstream_url: String,
        ingress_token: Option<String>,
        sync_wal: bool,
    ) -> Result<Self, SidecarError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(600))
            .tcp_keepalive(Duration::from_secs(30))
            .http2_adaptive_window(true)
            .build()?;
        Ok(Self {
            receiver: Arc::new(receiver),
            upstream_url: upstream_url.trim_end_matches('/').into(),
            http,
            ingress_token: ingress_token.map(Into::into),
            sync_wal,
            chat_projections: Arc::new(std::sync::Mutex::new(ChatProjectionCache::default())),
        })
    }

    pub fn receiver(&self) -> &Receiver {
        &self.receiver
    }
}

pub fn router(state: SidecarState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/dlr/capabilities", get(capabilities))
        .route("/v1/dlr/frame", post(handle_frame))
        .route("/v1/dlr/chat/completions", post(chat_completions))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn readyz(State(state): State<SidecarState>) -> Response {
    match state.receiver.store().flush_wal(false) {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "ready"}))).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "reason": "wal_unavailable"})),
        )
            .into_response(),
    }
}

async fn capabilities() -> impl IntoResponse {
    Json(json!({
        "protocol": "dlr",
        "version": PROTOCOL_VERSION,
        "transport": "http",
        "chat_completions": true,
        "resync": true,
        "gateway_changes_required": false,
        "model_runtime_changes_required": false,
        "max_request_bytes": MAX_REQUEST_BYTES,
    }))
}

async fn handle_frame(
    State(state): State<SidecarState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid sidecar token");
    }
    let frame = match decode_frame_bytes(&body) {
        Ok(frame) => frame,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    // The generic protocol endpoint may seed or mutate blocks without chat
    // schema validation. Its next chat request must validate the full root.
    state
        .chat_projections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(frame_session_id(&frame));
    match state.receiver.handle_frame(frame) {
        Ok(response) => {
            if let Err(error) = state.receiver.store().flush_wal(state.sync_wal) {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
            }
            match response {
                Some(response) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, FRAME_CONTENT_TYPE)],
                    encode_frame(&response),
                )
                    .into_response(),
                None => StatusCode::NO_CONTENT.into_response(),
            }
        }
        Err(error) => receiver_error_response(error),
    }
}

async fn chat_completions(
    State(state): State<SidecarState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&state, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid sidecar token");
    }
    let envelope = match ChatEnvelope::decode(body) {
        Ok(envelope) => envelope,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    if let Err(error) = validate_request_metadata(&envelope.metadata) {
        return error_response(StatusCode::BAD_REQUEST, &error.to_string());
    }
    let frame = match decode_frame_bytes(&envelope.frame) {
        Ok(Frame::Append(frame)) => frame,
        Ok(_) => return error_response(StatusCode::BAD_REQUEST, "chat endpoint requires APPEND"),
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    if let Err(error) = validate_chat_append(&state.receiver, &frame) {
        return error_response(StatusCode::BAD_REQUEST, &error.to_string());
    }
    let session_id = frame.session_id;
    let base_root = frame.base_root;
    let cached_projection = state
        .chat_projections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_id, base_root);
    let ack = match state.receiver.handle_append(frame) {
        Ok(ack) => ack,
        Err(error) => return receiver_error_response(error),
    };
    // The ACK is not exposed until the WAL has been flushed. A client that
    // receives x-dlr-ack-root may therefore safely discard its retry body.
    if let Err(error) = state.receiver.store().flush_wal(state.sync_wal) {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }
    let (request_body, request_len, projection) = match reconstruct_request_body(
        &state.receiver,
        session_id,
        envelope.metadata,
        ack.root,
        cached_projection,
    ) {
        Ok(request) => request,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    state
        .chat_projections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id, projection);
    let upstream = format!("{}/v1/chat/completions", state.upstream_url);
    let mut outbound = state
        .http
        .post(upstream)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, request_len)
        .body(request_body);
    for name in forwarded_request_headers() {
        if let Some(value) = headers.get(name) {
            outbound = outbound.header(name, value);
        }
    }
    let response = match outbound.send().await {
        Ok(response) => response,
        Err(error) => {
            let mut response = error_response(StatusCode::BAD_GATEWAY, &error.to_string());
            insert_root_header(response.headers_mut(), ACK_ROOT_HEADER, &ack.root);
            return response;
        }
    };
    proxy_response(response, ack)
}

/// Build an upstream JSON body as a stream of existing refcounted block
/// payloads. This avoids parsing a large history into a `Value` tree and then
/// serializing/copying the entire tree into another contiguous request buffer.
fn reconstruct_request_body(
    receiver: &Receiver,
    session_id: u128,
    metadata: ChatMetadata,
    root: MerkleRoot,
    cached: Option<CachedChatProjection>,
) -> Result<(RequestBody, usize, CachedChatProjection), SidecarError> {
    let request = metadata
        .request
        .as_object()
        .ok_or(SidecarError::RequestNotObject)?;
    if request.contains_key("messages") {
        return Err(SidecarError::MessagesInMetadata);
    }
    let mut prefix = serde_json::to_vec(&metadata.request)?;
    if prefix.pop() != Some(b'}') {
        return Err(SidecarError::RequestNotObject);
    }
    prefix.extend_from_slice(b",\"messages\":[");

    let projection = extend_or_reconstruct_projection(receiver, session_id, root, cached)?;
    let total = prefix
        .len()
        .saturating_add(projection.json_bytes)
        .saturating_add(2); // final `]}`
    let cached_chunks = Arc::clone(&projection.chunks);
    let message_stream = stream::unfold((cached_chunks, 0usize), |(chunks, index)| async move {
        if index >= chunks.len() {
            None
        } else {
            let chunk = chunks[index].clone();
            Some((Ok::<Bytes, io::Error>(chunk), (chunks, index + 1)))
        }
    });
    let chunks = stream::iter([Ok::<Bytes, io::Error>(Bytes::from(prefix))])
        .chain(message_stream)
        .chain(stream::iter([Ok(Bytes::from_static(b"]}"))]));
    let body = RequestBody::wrap_stream(chunks);
    Ok((body, total, projection))
}

fn extend_or_reconstruct_projection(
    receiver: &Receiver,
    session_id: u128,
    root: MerkleRoot,
    cached: Option<CachedChatProjection>,
) -> Result<CachedChatProjection, SidecarError> {
    let session_len = receiver.store().session_len(session_id);
    if let Some(mut projection) = cached.filter(|cached| cached.message_count <= session_len) {
        let start = projection.message_count;
        let additions = receiver.reconstruct_range(session_id, start, session_len);
        if additions.len() != session_len.saturating_sub(start) {
            return Err(SidecarError::Envelope("session projection is incomplete"));
        }
        if !additions.is_empty() {
            append_projection_payloads(
                Arc::make_mut(&mut projection.chunks),
                &mut projection.message_count,
                &mut projection.json_bytes,
                additions.into_iter().map(|block| block.payload),
            );
        }
        projection.root = root;
        return Ok(projection);
    }

    let blocks = receiver.reconstruct(session_id);
    let mut payloads = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.into_iter().enumerate() {
        validate_serialized_message(&block.payload, index)?;
        payloads.push(block.payload);
    }
    let mut chunks = Vec::new();
    let mut message_count = 0usize;
    let mut json_bytes = 0usize;
    append_projection_payloads(&mut chunks, &mut message_count, &mut json_bytes, payloads);
    Ok(CachedChatProjection {
        root,
        message_count,
        chunks: Arc::new(chunks),
        json_bytes,
    })
}

fn append_projection_payloads(
    chunks: &mut Vec<Bytes>,
    message_count: &mut usize,
    json_bytes: &mut usize,
    payloads: impl IntoIterator<Item = Bytes>,
) {
    // Grow to the target naturally. A steady turn often appends only two tiny
    // messages; preallocating 64 KiB for every such cached tail would waste
    // substantial memory over a long conversation.
    let mut pending = Vec::new();
    for payload in payloads {
        let comma_bytes = usize::from(*message_count > 0);
        *json_bytes = json_bytes
            .saturating_add(comma_bytes)
            .saturating_add(payload.len());
        let combined_len = comma_bytes.saturating_add(payload.len());
        if combined_len <= PROJECTION_CHUNK_BYTES {
            if pending.len().saturating_add(combined_len) > PROJECTION_CHUNK_BYTES {
                chunks.push(Bytes::from(std::mem::take(&mut pending)));
            }
            if comma_bytes != 0 {
                pending.push(b',');
            }
            pending.extend_from_slice(&payload);
        } else {
            if !pending.is_empty() {
                chunks.push(Bytes::from(std::mem::take(&mut pending)));
            }
            if comma_bytes != 0 {
                chunks.push(Bytes::from_static(b","));
            }
            chunks.push(payload);
        }
        *message_count = message_count.saturating_add(1);
    }
    if !pending.is_empty() {
        chunks.push(Bytes::from(pending));
    }
}

fn frame_session_id(frame: &Frame) -> u128 {
    match frame {
        Frame::Append(frame) => frame.session_id,
        Frame::Resync(frame) => frame.session_id,
        Frame::Bulk(frame) => frame.session_id,
        Frame::Ack(frame) => frame.session_id,
        Frame::Missing(frame) => frame.session_id,
    }
}

#[derive(Deserialize)]
struct MessageRole<'a> {
    #[serde(borrow)]
    #[allow(dead_code)]
    role: std::borrow::Cow<'a, str>,
}

fn validate_serialized_message(payload: &[u8], index: usize) -> Result<(), SidecarError> {
    serde_json::from_slice::<MessageRole<'_>>(payload)
        .map(|_| ())
        .map_err(|_| SidecarError::InvalidMessage(index))
}

fn validate_chat_append(receiver: &Receiver, frame: &AppendFrame) -> Result<(), SidecarError> {
    let mut inline: HashMap<BlockId, Block> = HashMap::new();
    for (index, frame_block) in frame.blocks.iter().enumerate() {
        let block = match frame_block {
            FrameBlock::Inline(wire_block) => {
                let canonical = receiver
                    .compressor()
                    .decompress(&wire_block.payload)
                    .map_err(|error| SidecarError::Frame(error.to_string()))?;
                let (block, id) = from_canonical_owned(canonical)
                    .map_err(|error| SidecarError::Frame(error.to_string()))?;
                inline.insert(id, block.clone());
                block
            }
            FrameBlock::Ref(id) => receiver
                .store()
                .get(id)
                .or_else(|| inline.get(id).cloned())
                .ok_or(SidecarError::Receiver(ReceiverError::MissingRef))?,
        };
        let value: Value = serde_json::from_slice(&block.payload)?;
        let object = value
            .as_object()
            .ok_or(SidecarError::InvalidMessage(index))?;
        validate_message_object(object, index)?;
    }
    Ok(())
}

fn validate_message_object(message: &Map<String, Value>, index: usize) -> Result<(), SidecarError> {
    if message.get("role").and_then(Value::as_str).is_none() {
        return Err(SidecarError::InvalidMessage(index));
    }
    Ok(())
}

fn validate_request_metadata(metadata: &ChatMetadata) -> Result<(), SidecarError> {
    validate_request_value(&metadata.request)
}

fn validate_request_value(request: &Value) -> Result<(), SidecarError> {
    let request = request.as_object().ok_or(SidecarError::RequestNotObject)?;
    if request.contains_key("messages") {
        return Err(SidecarError::MessagesInMetadata);
    }
    Ok(())
}

fn authorized(state: &SidecarState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.ingress_token.as_deref() else {
        return true;
    };
    let Some(actual) = headers
        .get(SIDECAR_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    constant_time_eq(actual.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn forwarded_request_headers() -> &'static [HeaderName] {
    static HEADERS: std::sync::OnceLock<Vec<HeaderName>> = std::sync::OnceLock::new();
    HEADERS.get_or_init(|| {
        [
            "authorization",
            "openai-organization",
            "openai-project",
            "x-subconscious-code-session-id",
            "x-subconscious-client",
            "x-request-id",
            "x-trace-id",
            "traceparent",
        ]
        .into_iter()
        .map(HeaderName::from_static)
        .collect()
    })
}

fn proxy_response(upstream: reqwest::Response, ack: AckFrame) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let upstream_headers = upstream.headers().clone();
    let stream = upstream
        .bytes_stream()
        .map(|item| item.map_err(io::Error::other));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    for (name, value) in &upstream_headers {
        if !is_hop_by_hop_response_header(name) {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    insert_root_header(response.headers_mut(), ACK_ROOT_HEADER, &ack.root);
    response
}

fn is_hop_by_hop_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | ACK_ROOT_HEADER
            | CURRENT_ROOT_HEADER
    )
}

fn receiver_error_response(error: ReceiverError) -> Response {
    match error {
        ReceiverError::BaseRootMismatch { current, .. } => {
            let mut response = error_response(StatusCode::CONFLICT, &error.to_string());
            insert_root_header(response.headers_mut(), CURRENT_ROOT_HEADER, &current);
            response
        }
        ReceiverError::ColdStartInProgress(_) => {
            error_response(StatusCode::CONFLICT, &error.to_string())
        }
        ReceiverError::MissingRef => error_response(StatusCode::CONFLICT, &error.to_string()),
        _ => error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": {"message": message}}))).into_response()
}

fn insert_root_header(headers: &mut HeaderMap, name: &'static str, root: &MerkleRoot) {
    if let Ok(value) = HeaderValue::from_str(&root_hex(root)) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

pub fn root_hex(root: &MerkleRoot) -> String {
    let mut out = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in root {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn parse_root_hex(value: &str) -> Option<MerkleRoot> {
    if value.len() != 64 {
        return None;
    }
    let mut root = [0u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        root[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(root)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::post;
    use http_body_util::BodyExt;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[test]
    fn envelope_roundtrip_and_bounds() {
        let envelope = ChatEnvelope {
            metadata: ChatMetadata {
                request: json!({"model": "test", "stream": true}),
            },
            frame: Bytes::from_static(b"frame"),
        };
        let decoded = ChatEnvelope::decode(envelope.encode().unwrap()).unwrap();
        assert_eq!(decoded.metadata.request["model"], "test");
        assert_eq!(decoded.frame, Bytes::from_static(b"frame"));
        assert!(ChatEnvelope::decode(Bytes::from_static(b"bad")).is_err());
    }

    #[test]
    fn prepared_request_reports_exact_json_sizes() {
        let request = json!({"model": "test", "stream": true});
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "world"}),
        ];
        let mut session = ChatSession::new("reported-sizes");
        let prepared = session.prepare_ref(&messages, &request).unwrap();

        assert_eq!(
            prepared.request_json_bytes,
            serde_json::to_vec(&request).unwrap().len()
        );
        assert_eq!(
            prepared.new_messages_json_bytes,
            messages
                .iter()
                .map(|message| serde_json::to_vec(message).unwrap().len())
                .sum::<usize>()
        );
        let decoded = ChatEnvelope::decode(prepared.body).unwrap();
        assert_eq!(decoded.metadata.request, request);
    }

    #[test]
    fn cached_projection_extends_only_with_new_messages() {
        let receiver = Receiver::new(ContentStore::new(), Compressor::default());
        let session_id = session_id_from_key("projection-extension");
        let mut shim = SessionShim::new(session_id, ContentStore::new(), Compressor::default());
        let first_payload = serde_json::to_vec(&json!({"role": "user", "content": "one"})).unwrap();
        let first = shim.ingest_turn(vec![Block::new(
            BlockKind::Message,
            1,
            first_payload.clone(),
        )]);
        let first_ack = receiver.handle_append(first).unwrap();
        let projection =
            extend_or_reconstruct_projection(&receiver, session_id, first_ack.root, None).unwrap();
        assert_eq!(projection.message_count, 1);
        assert_eq!(projection.chunks.concat(), first_payload);

        assert!(shim.apply_ack(first_ack.root));
        let second_payload =
            serde_json::to_vec(&json!({"role": "assistant", "content": "two"})).unwrap();
        let second = shim.ingest_turn(vec![Block::new(
            BlockKind::Message,
            2,
            second_payload.clone(),
        )]);
        let second_ack = receiver.handle_append(second).unwrap();
        let projection = extend_or_reconstruct_projection(
            &receiver,
            session_id,
            second_ack.root,
            Some(projection),
        )
        .unwrap();

        assert_eq!(projection.root, second_ack.root);
        assert_eq!(projection.message_count, 2);
        let mut expected = first_payload;
        expected.push(b',');
        expected.extend_from_slice(&second_payload);
        assert_eq!(projection.chunks.concat(), expected);
        assert_eq!(projection.json_bytes, expected.len());
    }

    #[test]
    fn projection_cache_is_bounded_and_lru() {
        fn projection(root_byte: u8, json_bytes: usize) -> CachedChatProjection {
            CachedChatProjection {
                root: [root_byte; 32],
                message_count: 0,
                chunks: Arc::new(Vec::new()),
                json_bytes,
            }
        }

        let mut cache = ChatProjectionCache::default();
        for session_id in 0..PROJECTION_CACHE_SESSIONS as u128 {
            cache.insert(session_id, projection(session_id as u8, 1));
        }
        assert!(cache.get(0, [0; 32]).is_some()); // make session 0 newest
        cache.insert(99, projection(99, 1));
        assert!(cache.entries.contains_key(&0));
        assert!(!cache.entries.contains_key(&1));

        let large = PROJECTION_CACHE_JSON_BYTES / 2 + 1;
        cache = ChatProjectionCache::default();
        cache.insert(1, projection(1, large));
        cache.insert(2, projection(2, large));
        assert!(!cache.entries.contains_key(&1));
        assert!(cache.entries.contains_key(&2));
        assert!(cache.json_bytes <= PROJECTION_CACHE_JSON_BYTES);
    }

    /// Manual release-mode comparison of the former history-wide store walk
    /// and chunk-vector rebuild against the ACK-root projection cache.
    #[test]
    #[ignore]
    fn bench_cached_projection_many_messages() {
        const MESSAGE_COUNT: usize = 10_000;
        const OLD_ITERS: usize = 200;
        const CACHED_ITERS: usize = 100_000;
        let receiver = Receiver::new(ContentStore::new(), Compressor::default());
        let session_id = session_id_from_key("projection-benchmark");
        let mut shim = SessionShim::new(session_id, ContentStore::new(), Compressor::default());
        let blocks = (0..MESSAGE_COUNT)
            .map(|index| {
                Block::new(
                    BlockKind::Message,
                    index as u64 + 1,
                    serde_json::to_vec(&json!({
                        "role": "user",
                        "content": format!("message-{index}-{}", "x".repeat(480)),
                    }))
                    .unwrap(),
                )
            })
            .collect();
        let ack = receiver.handle_append(shim.ingest_turn(blocks)).unwrap();
        let projection =
            extend_or_reconstruct_projection(&receiver, session_id, ack.root, None).unwrap();

        let old_started = std::time::Instant::now();
        for _ in 0..OLD_ITERS {
            let blocks = receiver.reconstruct(session_id);
            let mut chunks = Vec::with_capacity(blocks.len().saturating_mul(2));
            for (index, block) in blocks.into_iter().enumerate() {
                if index > 0 {
                    chunks.push(Bytes::from_static(b","));
                }
                chunks.push(block.payload);
            }
            std::hint::black_box(chunks);
        }
        let old_elapsed = old_started.elapsed();

        let cached_started = std::time::Instant::now();
        for _ in 0..CACHED_ITERS {
            let cached = extend_or_reconstruct_projection(
                &receiver,
                session_id,
                ack.root,
                Some(projection.clone()),
            )
            .unwrap();
            std::hint::black_box(cached);
        }
        let cached_elapsed = cached_started.elapsed();
        eprintln!(
            "projection {MESSAGE_COUNT} messages: old={:.3} ms/request cached={:.3} µs/request speedup={:.1}x chunks={}→{}",
            old_elapsed.as_secs_f64() * 1_000.0 / OLD_ITERS as f64,
            cached_elapsed.as_secs_f64() * 1_000_000.0 / CACHED_ITERS as f64,
            (old_elapsed.as_secs_f64() / OLD_ITERS as f64)
                / (cached_elapsed.as_secs_f64() / CACHED_ITERS as f64),
            MESSAGE_COUNT * 2 - 1,
            projection.chunks.len(),
        );
    }

    #[test]
    fn root_hex_roundtrip() {
        let root = [0xabu8; 32];
        assert_eq!(parse_root_hex(&root_hex(&root)), Some(root));
        assert_eq!(parse_root_hex("xyz"), None);
    }

    #[test]
    fn prepare_validation_is_atomic() {
        let mut session = ChatSession::new("atomic-prepare");
        let root = session.root();
        assert!(matches!(
            session.prepare(
                &[Value::String("not a message".into())],
                json!({"model": "test"})
            ),
            Err(SidecarError::InvalidMessage(0))
        ));
        assert_eq!(session.root(), root);

        assert!(matches!(
            session.prepare(
                &[json!({"role": "user", "content": "hello"})],
                json!({"model": "test", "messages": []})
            ),
            Err(SidecarError::MessagesInMetadata)
        ));
        assert_eq!(session.root(), root);

        assert!(matches!(
            session.prepare(
                &[json!({"content": "missing role"})],
                json!({"model": "test"})
            ),
            Err(SidecarError::InvalidMessage(0))
        ));
        assert_eq!(session.root(), root);
    }

    #[test]
    fn chat_session_allows_only_one_unacknowledged_append() {
        let mut session = ChatSession::new("single-flight");
        let prepared = session
            .prepare(
                &[json!({"role": "user", "content": "first"})],
                json!({"model": "test"}),
            )
            .unwrap();
        assert!(session.has_pending_request());
        assert!(matches!(
            session.prepare(
                &[json!({"role": "user", "content": "second"})],
                json!({"model": "test"})
            ),
            Err(SidecarError::RequestInFlight)
        ));
        assert!(session.apply_ack(prepared.expected_root));
        assert!(!session.has_pending_request());
        session
            .prepare(
                &[json!({"role": "user", "content": "second"})],
                json!({"model": "test"}),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn invalid_wire_message_cannot_poison_session() {
        let receiver = Receiver::new(ContentStore::new(), Compressor::default());
        let state = SidecarState::new(receiver, "http://127.0.0.1:9".into(), None, false).unwrap();
        let inspect = state.clone();
        let session_id = session_id_from_key("invalid-wire");
        let mut shim = SessionShim::new(session_id, ContentStore::new(), Compressor::default());
        let append = shim.ingest_turn(vec![Block::new(
            BlockKind::Message,
            1,
            serde_json::to_vec(&json!({"content": "missing role"})).unwrap(),
        )]);
        let body = ChatEnvelope {
            metadata: ChatMetadata {
                request: json!({"model": "test"}),
            },
            frame: encode_frame(&Frame::Append(append)),
        }
        .encode()
        .unwrap();

        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/dlr/chat/completions")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(inspect.receiver().store().session_len(session_id), 0);
    }

    #[tokio::test]
    async fn readiness_checks_the_wal() {
        let state = SidecarState::new(
            Receiver::new(ContentStore::new(), Compressor::default()),
            "http://127.0.0.1:9".into(),
            None,
            false,
        )
        .unwrap();
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn generic_protocol_mutation_invalidates_chat_validation_cache() {
        let state = SidecarState::new(
            Receiver::new(ContentStore::new(), Compressor::default()),
            "http://127.0.0.1:9".into(),
            None,
            false,
        )
        .unwrap();
        let session_id = session_id_from_key("cache-invalidation");
        state.chat_projections.lock().unwrap().insert(
            session_id,
            CachedChatProjection {
                root: [7; 32],
                message_count: 0,
                chunks: Arc::new(Vec::new()),
                json_bytes: 0,
            },
        );
        let frame = Frame::Resync(dlr_core::ResyncFrame {
            session_id,
            client_root: dlr_core::ROOT_ZERO,
            manifest: vec![],
        });
        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/dlr/frame")
                    .body(Body::from(encode_frame(&frame)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!state
            .chat_projections
            .lock()
            .unwrap()
            .entries
            .contains_key(&session_id));
    }

    #[tokio::test]
    async fn network_client_streams_chat_and_applies_ack() {
        async fn upstream(Json(body): Json<Value>) -> Json<Value> {
            Json(json!({"message_count": body["messages"].as_array().unwrap().len()}))
        }
        let upstream_app = Router::new().route("/v1/chat/completions", post(upstream));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let state = SidecarState::new(
            Receiver::new(ContentStore::new(), Compressor::default()),
            format!("http://{upstream_addr}"),
            Some("secret".into()),
            false,
        )
        .unwrap();
        let sidecar_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sidecar_addr = sidecar_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(sidecar_listener, router(state)).await.unwrap();
        });

        let client =
            DlrChatClient::new(format!("http://{sidecar_addr}"), Some("secret".into())).unwrap();
        let mut session = ChatSession::new("network-client");
        let prepared = session
            .prepare(
                &[json!({"role": "user", "content": "hello"})],
                json!({"model": "test"}),
            )
            .unwrap();
        let response = client
            .send_chat(&mut session, &prepared, HeaderMap::new())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(session.base_root(), prepared.expected_root);
        assert!(!session.has_pending_request());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["message_count"], 1);
    }

    #[tokio::test]
    async fn network_client_completes_cold_synchronization() {
        let state = SidecarState::new(
            Receiver::new(ContentStore::new(), Compressor::default()),
            "http://127.0.0.1:9".into(),
            None,
            false,
        )
        .unwrap();
        let inspect = state.clone();
        let sidecar_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sidecar_addr = sidecar_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(sidecar_listener, router(state)).await.unwrap();
        });

        let client = DlrChatClient::new(format!("http://{sidecar_addr}"), None).unwrap();
        let mut session = ChatSession::new("cold-network-client");
        let prepared = session
            .prepare(
                &[json!({"role": "user", "content": "cold history"})],
                json!({"model": "test"}),
            )
            .unwrap();
        client.synchronize(&mut session, 5).await.unwrap();
        assert_eq!(session.base_root(), prepared.expected_root);
        assert!(!session.has_pending_request());
        assert_eq!(
            inspect.receiver().session_root(session.session_id()),
            Some(prepared.expected_root)
        );
    }

    #[tokio::test]
    async fn sse_bytes_and_streaming_headers_pass_through() {
        async fn upstream() -> Response {
            (
                [
                    (header::CONTENT_TYPE, "text/event-stream"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
            )
                .into_response()
        }

        let upstream_app = Router::new().route("/v1/chat/completions", post(upstream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });

        let state = SidecarState::new(
            Receiver::new(ContentStore::new(), Compressor::default()),
            format!("http://{addr}"),
            None,
            false,
        )
        .unwrap();
        let mut session = ChatSession::new("sse-session");
        let prepared = session
            .prepare(
                &[json!({"role": "user", "content": "hello"})],
                json!({"model": "test", "stream": true}),
            )
            .unwrap();
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/dlr/chat/completions")
                    .body(Body::from(prepared.body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert!(response.headers().contains_key(ACK_ROOT_HEADER));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "data: {\"choices\":[]}\n\ndata: [DONE]\n\n");
    }

    #[tokio::test]
    async fn append_reconstruct_proxy_and_retry_are_end_to_end() {
        #[derive(Clone)]
        struct Seen(Arc<Mutex<Vec<Value>>>);
        async fn upstream(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
            seen.0.lock().unwrap().push(body.clone());
            Json(json!({
                "id": "chatcmpl-test",
                "choices": [{"message": {"role": "assistant", "content": "ok"}}],
                "seen_messages": body["messages"].as_array().unwrap().len()
            }))
        }

        let seen = Seen(Arc::new(Mutex::new(Vec::new())));
        let upstream_app = Router::new()
            .route("/v1/chat/completions", post(upstream))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });

        let receiver = Receiver::new(ContentStore::new(), Compressor::default());
        let state = SidecarState::new(
            receiver,
            format!("http://{addr}"),
            Some("secret".into()),
            false,
        )
        .unwrap();
        let app = router(state);
        let mut session = ChatSession::new("session-test");
        let prepared = session
            .prepare(
                &[
                    json!({"role": "system", "content": "be concise"}),
                    json!({"role": "user", "content": "hello"}),
                ],
                json!({"model": "test", "stream": false}),
            )
            .unwrap();

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/dlr/chat/completions")
            .header(SIDECAR_TOKEN_HEADER, "secret")
            .header(header::AUTHORIZATION, "Bearer gateway-key")
            .header(header::CONTENT_TYPE, CONTENT_TYPE)
            .body(Body::from(prepared.body.clone()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ack = response
            .headers()
            .get(ACK_ROOT_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_root_hex)
            .unwrap();
        assert_eq!(ack, prepared.expected_root);
        assert!(session.apply_ack(ack));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["seen_messages"], 2);

        // Retry the exact accepted request. The receiver recognizes the
        // target root and re-ACKs without duplicating either message.
        let retry = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/dlr/chat/completions")
            .header(SIDECAR_TOKEN_HEADER, "secret")
            .body(Body::from(prepared.body))
            .unwrap();
        let response = app.clone().oneshot(retry).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["seen_messages"], 2);

        let second = session
            .prepare(
                &[json!({"role": "assistant", "content": "ok"})],
                json!({"model": "test", "stream": false}),
            )
            .unwrap();
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/dlr/chat/completions")
            .header(SIDECAR_TOKEN_HEADER, "secret")
            .body(Body::from(second.body))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["seen_messages"], 3);
        assert_eq!(seen.0.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn sidecar_token_is_enforced() {
        let receiver = Receiver::new(ContentStore::new(), Compressor::default());
        let state = SidecarState::new(
            receiver,
            "http://127.0.0.1:9".into(),
            Some("secret".into()),
            false,
        )
        .unwrap();
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/dlr/frame")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
