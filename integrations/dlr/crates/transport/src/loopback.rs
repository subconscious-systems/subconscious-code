//! In-process loopback transport (DESIGN §5: "loopback/UDS — free, downstream
//! of all ceilings").
//!
//! Used by the first-pass scope (local shim + in-process receiver test harness).
//! Two endpoints connected by a pair of bounded MPSC channels; frames are
//! `Bytes` (clone-on-write, zero-copy across the boundary).

use std::sync::Arc;

use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender};
use dlr_core::SessionId;
use parking_lot::Mutex;

use crate::{FrameRecv, FrameSend, Stream, Transport, TransportError};

/// Public alias used by the transport crate's re-exports.
pub type LoopbackTransport = LoopbackEndpoint;

/// `cap` is the per-direction frame buffer (backpressure). Returns the two
/// connected endpoints; `a.open()` talks to `b.accept()`.
pub fn loopback_pair(cap: usize) -> (LoopbackEndpoint, LoopbackEndpoint) {
    let (a_to_b_tx, a_to_b_rx) = bounded(cap);
    let (b_to_a_tx, b_to_a_rx) = bounded(cap);
    let a = LoopbackEndpoint {
        tx: a_to_b_tx,
        rx: b_to_a_rx,
        next: Arc::new(Mutex::new(0u64)),
    };
    let b = LoopbackEndpoint {
        tx: b_to_a_tx,
        rx: a_to_b_rx,
        next: Arc::new(Mutex::new(0u64)),
    };
    (a, b)
}

/// One side of a loopback pair. `open`/`accept` return streams multiplexed over
/// the single channel by a leading session-id tag (the loopback is a stand-in
/// for QUIC's real multiplexed streams).
pub struct LoopbackEndpoint {
    tx: Sender<Bytes>,
    rx: Receiver<Bytes>,
    next: Arc<Mutex<u64>>,
}

impl LoopbackEndpoint {
    pub fn pair(cap: usize) -> (Self, Self) {
        loopback_pair(cap)
    }
}

impl Transport for LoopbackEndpoint {
    fn open(&self, session_id: SessionId) -> Result<Stream, TransportError> {
        let id = {
            let mut n = self.next.lock();
            *n += 1;
            *n
        };
        let sink = Box::new(LoopbackSink {
            tx: self.tx.clone(),
            session_id,
            stream_id: id,
        });
        let src = Box::new(LoopbackSource {
            rx: self.rx.clone(),
        });
        Ok(Stream { tx: sink, rx: src })
    }

    fn accept(&self) -> Result<Stream, TransportError> {
        // Symmetric on the loopback.
        self.open(SessionId(0))
    }

    fn kind(&self) -> &'static str {
        "loopback"
    }
}

struct LoopbackSink {
    tx: Sender<Bytes>,
    session_id: SessionId,
    stream_id: u64,
}

impl FrameSend for LoopbackSink {
    fn send(&self, frame: Bytes) -> Result<(), TransportError> {
        // Tag with stream header so a future mux can demux. For the in-process
        // single-stream test harness we keep it minimal: prepend 8-byte tag.
        let mut tagged = BytesMutProxy::with_capacity(16 + frame.len());
        tagged.put_u64(self.stream_id);
        tagged.put_slice(&frame);
        let _ = self.session_id;
        self.tx
            .send(tagged.freeze())
            .map_err(|_| TransportError::Closed)
    }
    fn close(&self) -> Result<(), TransportError> {
        Ok(())
    }
}

struct LoopbackSource {
    rx: Receiver<Bytes>,
}

impl FrameRecv for LoopbackSource {
    fn recv(&self) -> Result<Bytes, TransportError> {
        let b = self.rx.recv().map_err(|_| TransportError::Closed)?;
        // strip the 8-byte stream tag written by the sink
        if b.len() >= 8 {
            Ok(b.slice(8..))
        } else {
            Ok(b)
        }
    }
    fn try_recv(&self) -> Result<Option<Bytes>, TransportError> {
        match self.rx.try_recv() {
            Ok(b) => {
                let b = if b.len() >= 8 { b.slice(8..) } else { b };
                Ok(Some(b))
            }
            Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err(TransportError::Closed),
        }
    }
}

// Tiny helper to build a tagged Bytes without pulling bytes::BufMut into the
// public surface; uses bytes::BytesMut under the hood.
struct BytesMutProxy(bytes::BytesMut);

impl BytesMutProxy {
    fn with_capacity(c: usize) -> Self {
        Self(bytes::BytesMut::with_capacity(c))
    }
    #[inline]
    fn put_u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_slice(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn freeze(self) -> Bytes {
        self.0.freeze()
    }
}
