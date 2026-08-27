//! RDMA / RoCEv2 transport (DESIGN §5: "RoCEv2/InfiniBand — kernel-bypass,
//! zero-copy, us latency"). Plus BlueField-2 offload and RDMA-over-TB5.
//!
//! The genuinely-bespoke privilege is kernel bypass (RDMA verbs) — the only
//! "faster than the existing stack" that is physically real, and it already
//! exists. We adopt the verbs; we do not invent them. Concrete `libibverbs` /
//! `rust-rdma` bindings are deferred; this module defines the surface and a
//! zero-copy frame abstraction.
//!
//! On RDMA paths we also use Zeckendorf framing (§6.3) instead of relying on
//! QUIC's framing integrity — the unframed path needs self-synchronization.

use dlr_core::SessionId;

use crate::{Stream, Transport, TransportError};

/// RDMA fabric configuration.
#[derive(Debug, Clone)]
pub struct RdmaConfig {
    pub device: String,
    pub port: u32,
    pub gid_index: u32,
    pub mtu: u32,
    pub max_wr: u32,
    /// BlueField-2 DPU offload: store + reconstruction on the NIC.
    pub dpu_offload: bool,
}

impl Default for RdmaConfig {
    fn default() -> Self {
        Self {
            device: "mlx5_0".into(),
            port: 1,
            gid_index: 3,
            mtu: 4096,
            max_wr: 1024,
            dpu_offload: false,
        }
    }
}

/// RDMA transport (RoCEv2). Verbs binding is second pass.
pub struct RdmaTransport {
    cfg: RdmaConfig,
}

impl RdmaTransport {
    pub fn new(cfg: RdmaConfig) -> Self {
        Self { cfg }
    }
    pub fn config(&self) -> &RdmaConfig {
        &self.cfg
    }
}

impl Transport for RdmaTransport {
    fn open(&self, _session_id: SessionId) -> Result<Stream, TransportError> {
        Err(TransportError::Other(
            "RDMA verbs binding is second pass".into(),
        ))
    }
    fn accept(&self) -> Result<Stream, TransportError> {
        Err(TransportError::Other("RDMA accept is second pass".into()))
    }
    fn kind(&self) -> &'static str {
        "rdma"
    }
}

/// RDMA-over-Thunderbolt-5 for the 8x M4 Pro Mac cluster (same coding layer,
/// different verbs).
pub struct Tb5Transport {
    #[allow(dead_code)]
    cfg: RdmaConfig,
}
impl Default for Tb5Transport {
    fn default() -> Self {
        Self::new()
    }
}

impl Tb5Transport {
    pub fn new() -> Self {
        Self {
            cfg: RdmaConfig {
                device: "thunderbolt5".into(),
                ..Default::default()
            },
        }
    }
}
impl Transport for Tb5Transport {
    fn open(&self, _s: SessionId) -> Result<Stream, TransportError> {
        Err(TransportError::Other(
            "RDMA-over-TB5 binding is second pass".into(),
        ))
    }
    fn accept(&self) -> Result<Stream, TransportError> {
        Err(TransportError::Other("TB5 accept is second pass".into()))
    }
    fn kind(&self) -> &'static str {
        "rdma-tb5"
    }
}

/// BlueField-2 offload target: chunk store, dedup, reconstruction *on the NIC*,
/// moving the cold-start reconstruction spike off the inference box.
pub struct BluefieldOffload {
    cfg: RdmaConfig,
}
impl Default for BluefieldOffload {
    fn default() -> Self {
        Self::new()
    }
}

impl BluefieldOffload {
    pub fn new() -> Self {
        Self {
            cfg: RdmaConfig {
                device: "bluefield-2".into(),
                dpu_offload: true,
                ..Default::default()
            },
        }
    }
    pub fn config(&self) -> &RdmaConfig {
        &self.cfg
    }
}
