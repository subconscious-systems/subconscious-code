//! Hierarchical two-layer coding for N-agent fan-out (extra strategy).
//!
//! Nightshift fans one context out to N agents. A flat RLNC multicast codes the
//! context once and lets any K combinations reconstruct per agent — already
//! capacity-optimal (§6.5). But when N is large and the agents sit behind
//! *different* lossy hops, a single flat generation forces every agent to
//! collect K independent combinations from a shared pool: a single slow hop
//! stalls everyone.
//!
//! Hierarchical coding splits the work in two layers:
//!   - **Outer** (across agents): a systematic Reed-Solomon (§`rs`) code of the
//!     context produces `n = k + m` outer symbols. Assign disjoint outer-symbol
//!     subsets to agent-groups; any `k` outer symbols reconstruct the context.
//!   - **Inner** (within a group): each group RLNC-codes (§6.5) its assigned
//!     outer symbols so a lossy intra-group hop doesn't stall the group.
//!
//! Net: a lossy hop only costs its own group's redundancy, not the whole fan's;
//! the outer RS bounds the total redundancy needed across all groups. For N
//! agents with per-agent loss p, hierarchical uses ~m_outer + m_inner·(groups)
//! parity instead of one fragile flat generation, and decouples slow hops.
//!
//! This module composes `rs` and `rlnc`; it does not re-implement field math.

use std::sync::Arc;

use crate::rlnc::{RlncDecoder, RlncEncoder, RlncError};
use crate::rs::{self, RsError};

#[derive(Debug, thiserror::Error)]
pub enum HierError {
    #[error("rs: {0}")]
    Rs(String),
    #[error("rlnc: {0}")]
    Rlnc(String),
    #[error("empty context")]
    Empty,
}

/// One outer symbol is itself a vector of `symbol_size` bytes (a chunk of the
/// flattened, compressed context, as in the cold-start path).
pub struct HierEncoder {
    outer: Vec<Arc<[u8]>>, // the n outer symbols (k data + m parity), Arc-shared
    k: usize,
    n: usize,
    symbol_size: usize,
}

impl HierEncoder {
    /// Build a hierarchical encoder. `data` is k outer data symbols; the outer
    /// RS adds `m_outer` parity. Each group will RLNC its assigned outer
    /// symbols.
    pub fn new(
        data: Vec<Vec<u8>>,
        k: usize,
        m_outer: usize,
        symbol_size: usize,
    ) -> Result<Self, HierError> {
        if data.is_empty() {
            return Err(HierError::Empty);
        }
        let enc = rs::RsEncoder::new(k, m_outer, symbol_size)
            .map_err(|e| HierError::Rs(e.to_string()))?;
        let outer = enc
            .encode(&data)
            .map_err(|e| HierError::Rs(e.to_string()))?;
        // Arc the outer symbols once so each group shares them by refcount bump
        // instead of cloning the (k+m)·symbol_size bytes per group.
        let outer: Vec<Arc<[u8]>> = outer.into_iter().map(Arc::from).collect();
        Ok(Self {
            outer,
            k,
            n: k + m_outer,
            symbol_size,
        })
    }

    pub fn n(&self) -> usize {
        self.n
    }
    pub fn k(&self) -> usize {
        self.k
    }

    /// Partition the `n` outer symbols into `g` disjoint groups and return a
    /// per-group RLNC encoder plus the outer-symbol indices that group owns.
    pub fn into_groups(self, g: usize, seed: u64) -> Vec<(Vec<usize>, RlncEncoder)> {
        let n = self.n;
        let symbol_size = self.symbol_size;
        let outer = self.outer; // move out (into_groups consumes self)
        let mut out = Vec::with_capacity(g);
        for gi in 0..g {
            let owned: Vec<usize> = (0..n).filter(|i| i % g == gi).collect();
            // Share the outer symbols by `Arc` clone (refcount bump, no byte
            // copy) instead of `outer[i].clone()` which duplicated each group's
            // slice out of the shared context.
            let src: Vec<Arc<[u8]>> = owned.iter().map(|&i| Arc::clone(&outer[i])).collect();
            let enc = RlncEncoder::new_shared(src, symbol_size, seed.wrapping_add(gi as u64));
            out.push((owned, enc));
        }
        out
    }
}

/// Hierarchical decoder: collect outer symbols (recovered per-group via RLNC,
/// or received directly), then RS-decode the context once `k` outer symbols are
/// present.
pub struct HierDecoder {
    k: usize,
    m_outer: usize,
    symbol_size: usize,
    /// outer symbols recovered so far: (outer_index, bytes)
    outer: Vec<(usize, Vec<u8>)>,
}

impl HierDecoder {
    pub fn new(k: usize, m_outer: usize, symbol_size: usize) -> Self {
        Self {
            k,
            m_outer,
            symbol_size,
            outer: Vec::new(),
        }
    }

    /// A group decodes its assigned outer symbols via RLNC and contributes them.
    pub fn add_group(
        &mut self,
        owned: &[usize],
        packets: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<usize, HierError> {
        let kg = owned.len();
        let mut dec = RlncDecoder::new(kg, self.symbol_size);
        for (coeffs, payload) in packets {
            dec.add(crate::rlnc::CodedPacket {
                coeffs: coeffs.clone(),
                payload: payload.clone(),
            })
            .map_err(|e: RlncError| HierError::Rlnc(e.to_string()))?;
        }
        let syms = dec
            .decode()
            .map_err(|e: RlncError| HierError::Rlnc(e.to_string()))?;
        for (i, s) in syms.into_iter().enumerate() {
            self.outer.push((owned[i], s));
        }
        Ok(self.outer.len())
    }

    /// Have we collected k distinct outer symbols?
    pub fn ready(&self) -> bool {
        self.outer.len() >= self.k
    }

    /// RS-decode the context from the collected outer symbols.
    pub fn decode(&self) -> Result<Vec<Vec<u8>>, HierError> {
        if !self.ready() {
            return Err(HierError::Rs(format!(
                "only {} outer symbols, need {}",
                self.outer.len(),
                self.k
            )));
        }
        rs::decode(self.k, self.m_outer, self.symbol_size, &self.outer)
            .map_err(|e: RsError| HierError::Rs(e.to_string()))
    }
}
