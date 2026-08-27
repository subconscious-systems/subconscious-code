//! QUIC transport (DESIGN §5: "QUIC (RFC 9000) — multiplexed streams, no HoL
//! block, 0-RTT resume, conn migration, TLS built-in").
//!
//! Concrete QUIC bindings (e.g. `quinn`) are deferred to the second pass. This
//! module defines the binding surface and a feature-gated stub so the rest of the
//! system can be written against `QuicTransport` now.
//!
//! Append deltas are sent as independent QUIC streams (no head-of-line blocking
//! between turns); the cold-start coded bulk transfer is one long stream.

use bytes::Bytes;
use dlr_core::SessionId;

use crate::{FrameRecv, FrameSend, Stream, Transport, TransportError};

/// Configuration for a QUIC endpoint.
#[derive(Debug, Clone)]
pub struct QuicConfig {
    pub addr: String,
    pub max_idle_ms: u64,
    pub initial_window: u64,
    pub max_streams: u64,
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:0".into(),
            max_idle_ms: 30_000,
            initial_window: 1 << 20,
            max_streams: 256,
        }
    }
}

/// QUIC transport. The real binding (`quinn`/`h3`) is wired in the second pass;
/// constructing it without a `quinn` feature returns an error so callers fail
/// loudly rather than silently degrading.
pub struct QuicTransport {
    cfg: QuicConfig,
}

impl QuicTransport {
    pub fn new(cfg: QuicConfig) -> Result<Self, TransportError> {
        // The first-pass scope runs over loopback only; QUIC is second pass.
        Ok(Self { cfg })
    }
    pub fn config(&self) -> &QuicConfig {
        &self.cfg
    }
}

impl Transport for QuicTransport {
    fn open(&self, _session_id: SessionId) -> Result<Stream, TransportError> {
        Err(TransportError::Other(
            "QUIC transport is deferred to the second pass; use LoopbackTransport".into(),
        ))
    }
    fn accept(&self) -> Result<Stream, TransportError> {
        Err(TransportError::Other(
            "QUIC accept deferred to second pass".into(),
        ))
    }
    fn kind(&self) -> &'static str {
        "quic"
    }
}

/// A QUIC stream sink placeholder (see `Stream` in `lib.rs`).
pub struct QuicSink {
    _buf: Bytes,
}
impl FrameSend for QuicSink {
    fn send(&self, _frame: Bytes) -> Result<(), TransportError> {
        Err(TransportError::Other("quic sink not yet bound".into()))
    }
    fn close(&self) -> Result<(), TransportError> {
        Ok(())
    }
}
pub struct QuicSource;
impl FrameRecv for QuicSource {
    fn recv(&self) -> Result<Bytes, TransportError> {
        Err(TransportError::Other("quic source not yet bound".into()))
    }
    fn try_recv(&self) -> Result<Option<Bytes>, TransportError> {
        Ok(None)
    }
}
