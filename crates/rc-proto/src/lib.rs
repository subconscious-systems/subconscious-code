//! rc-proto: Chat Completions wire types, canonical JSON, and HTTP client.
//!
//! The OpenAI-compatible `/v1/chat/completions` boundary. Everything that
//! crosses the network is defined here so the rest of the harness can treat
//! `Turn`s (rc-core) as the source of truth and *project* to wire form per
//! request (§4.1). Wire messages are never stored as state.
//!
//! Canonical JSON (§4.6): any prefix-stable structure — the tools array
//! especially — is serialized through [`canonical`] so its byte form is
//! deterministic across builds and sessions. A reordered schema would silently
//! zero the cache hit rate against a prefix-caching router; canonicalizing
//! makes that impossible by construction. We do it from M0, before anything
//! depends on it.

pub mod canonical;
pub mod client;
mod dlr;
pub mod error;
pub mod stream;
pub mod wire;

pub use client::{ChatClient, CompleteOpts, RequestPayloadStats, RetryOpts};
pub use dlr::DlrMode;
pub use error::ProtoError;
pub use stream::{
    repair_json, AgentStreamEvent, FinalizedToolCall, FinishReason, SseDecoder, StreamFuser,
    ToolCallAccumulator,
};
pub use wire::{
    ChatCompletionResponse, FunctionCall, FunctionDefinition, StreamOptions, ToolCall,
    ToolChoiceValue, ToolDefinition, Usage, UserContent, WireMessage,
};
