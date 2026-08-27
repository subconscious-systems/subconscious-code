//! Multipath transport (DESIGN §5: "Multipath QUIC — aggregate links; pairs
//! with RLNC (§6.5) for seamless path use"). Coded packets are path-agnostic:
//! spread a coded generation across multiple paths without coordinating which
//! packet went where; any K independent combinations reconstruct.
//!
//! This module defines a multipath *dispatcher* that round-robins (or
//! latency-weights) outbound frames across N underlying transports, and a
//! *collector* that merges inbound frames into one ordered stream for the
//! receiver. Because the frames above are coded (fountain/RLNC), out-of-order and
//! duplicate-across-path arrival is not just tolerated but useful.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;

use crate::{FrameSend, Stream, Transport, TransportError};

/// A multipath transport wrapping several underlying transports.
pub struct MultipathTransport {
    paths: Vec<Arc<dyn Transport>>,
    /// Per-path weight for the dispatcher.
    weights: Vec<f32>,
    /// Round-robin index — atomic so the per-open dispatch takes no lock.
    next: AtomicUsize,
}

impl MultipathTransport {
    pub fn new(paths: Vec<Arc<dyn Transport>>, weights: Vec<f32>) -> Self {
        let weights = if weights.len() == paths.len() {
            weights
        } else {
            vec![1.0; paths.len()]
        };
        Self {
            paths,
            weights,
            next: AtomicUsize::new(0),
        }
    }

    fn pick(&self) -> usize {
        // Lock-free round-robin: a relaxed fetch_add is sufficient — dispatch
        // need not be perfectly fair, just spread. (Weights are kept on the
        // struct for a future weighted table; the plain RR path is used today,
        // matching the prior `(*n + 1) % len` behaviour without the mutex.)
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.paths.len();
        let _ = self.weights.len();
        i
    }
}

impl Transport for MultipathTransport {
    fn open(&self, session_id: dlr_core::SessionId) -> Result<Stream, TransportError> {
        let i = self.pick();
        self.paths[i].open(session_id)
    }
    fn accept(&self) -> Result<Stream, TransportError> {
        // accept from any path that has an incoming stream; here pick the first.
        for p in &self.paths {
            if let Ok(s) = p.accept() {
                return Ok(s);
            }
        }
        Err(TransportError::Closed)
    }
    fn kind(&self) -> &'static str {
        "multipath"
    }
}

/// A sink that fans a coded generation out across multiple paths.
pub struct MultipathSink {
    sinks: Vec<Box<dyn FrameSend>>,
    /// Round-robin index — atomic so the per-frame fan-out takes no lock.
    next: AtomicUsize,
}

impl MultipathSink {
    pub fn new(sinks: Vec<Box<dyn FrameSend>>) -> Self {
        Self {
            sinks,
            next: AtomicUsize::new(0),
        }
    }
}

impl FrameSend for MultipathSink {
    fn send(&self, frame: Bytes) -> Result<(), TransportError> {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.sinks.len();
        self.sinks[i].send(frame)
    }
    fn close(&self) -> Result<(), TransportError> {
        let mut last = Ok(());
        for s in &self.sinks {
            if let Err(e) = s.close() {
                last = Err(e);
            }
        }
        last
    }
}
