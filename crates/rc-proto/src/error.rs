use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProtoError {
    #[error("http transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("request failed: HTTP {status} — {body}")]
    Status { status: u16, body: String },

    #[error("response had no choices")]
    EmptyChoices,

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("no API key configured (set $SC_API_KEY or the var named by provider.api_key_env)")]
    NoApiKey,

    #[error("session id is not a valid correlation header value")]
    InvalidSessionId,

    /// Compressing the request body failed (`request_gzip`).
    #[error("gzip error: {0}")]
    Gzip(std::io::Error),

    #[error("request spool I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("DLR transport error: {0}")]
    Dlr(String),

    /// T2: the streaming body produced no chunk for the idle window (a stall).
    /// Distinct from `Http` (the total request timeout) so a caller can tell a
    /// mid-stream stall from a connection / total-timeout failure.
    #[error("stream stalled: no chunk for {0:?}")]
    Idle(std::time::Duration),
}
