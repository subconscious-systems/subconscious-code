//! Transport substrate abstractions (DESIGN §5).
//!
//! "Reuse substrates; select by path." The append-log and coding layers ride on
//! top of whichever transport applies. We do **not** reinvent L4 (§0):
//! reliability, congestion, loss recovery, encryption are 40 years deep.
//!
//! This crate defines the surface the shim/receiver speak to, and ships an
//! in-process loopback transport (used by the first-pass scope and tests). QUIC
//! (WAN), RoCEv2/RDMA (intra-fabric), BlueField-2 offload, and
//! RDMA-over-Thunderbolt-5 (Mac cluster) share the same `Transport` trait; their
//! concrete bindings are deferred to the second pass and are represented here by
//! thin trait implementations that the bin wires up.

pub mod bbr;
pub mod flow;
pub mod loopback;
pub mod multipath;
pub mod pipeline;
pub mod quic;
pub mod rdma;

pub use bbr::BbrModel;
pub use flow::CreditFlow;
pub use loopback::LoopbackTransport;
pub use multipath::MultipathTransport;
pub use pipeline::BytePipeline;
pub use quic::QuicTransport;
pub use rdma::RdmaTransport;

use bytes::Bytes;
use dlr_core::SessionId;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("transport closed")]
    Closed,
    #[error("transport full / backpressure")]
    Full,
    #[error("transport error: {0}")]
    Other(String),
}

/// A reliable, ordered byte/Bytes stream between two endpoints for one session.
/// The transport guarantees delivery and order *per stream*; multiplexed
/// streams (QUIC) avoid head-of-line blocking between them.
pub trait Transport: Send + Sync {
    /// Open a new stream for `session_id`. Returns a duplex handle.
    fn open(&self, session_id: SessionId) -> Result<Stream, TransportError>;
    /// Accept an incoming stream opened by the peer.
    fn accept(&self) -> Result<Stream, TransportError>;
    fn kind(&self) -> &'static str;
}

/// A duplex stream: frames in, frames out. Frames are length-delimited `Bytes`
/// (the frame codec above adds its own 5-byte header; the transport is
/// frame-opaque).
pub struct Stream {
    pub tx: FrameSink,
    pub rx: FrameSource,
}

pub type FrameSink = Box<dyn FrameSend + Send + Sync>;
pub type FrameSource = Box<dyn FrameRecv + Send + Sync>;

pub trait FrameSend {
    fn send(&self, frame: Bytes) -> Result<(), TransportError>;
    fn close(&self) -> Result<(), TransportError>;
}

pub trait FrameRecv {
    /// Receive the next frame, blocking until one arrives.
    fn recv(&self) -> Result<Bytes, TransportError>;
    /// Non-blocking peek of availability.
    fn try_recv(&self) -> Result<Option<Bytes>, TransportError>;
}

/// Per-path selection (DESIGN §5 table). The shim picks a transport by where the
/// peer lives; the protocol layers above are path-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// Claude Code -> shim: free, downstream of all ceilings.
    Loopback,
    /// shim -> receiver over WAN.
    Quic,
    /// Multihomed WAN, aggregate links.
    MultipathQuic,
    /// Intra-fabric, kernel-bypass.
    RoceRdma,
    /// BlueField-2 offload path.
    Bluefield,
    /// Mac cluster over Thunderbolt-5.
    RdmaTb5,
}

impl Path {
    pub fn substrate(self) -> &'static str {
        match self {
            Self::Loopback => "loopback/UDS",
            Self::Quic => "QUIC (RFC 9000)",
            Self::MultipathQuic => "Multipath QUIC",
            Self::RoceRdma => "RoCEv2/InfiniBand",
            Self::Bluefield => "BlueField-2 offload",
            Self::RdmaTb5 => "RDMA-over-Thunderbolt-5",
        }
    }
}
