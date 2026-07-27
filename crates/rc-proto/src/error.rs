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

    #[error("no API key configured (set $RC_API_KEY or the var named by provider.api_key_env)")]
    NoApiKey,
}
