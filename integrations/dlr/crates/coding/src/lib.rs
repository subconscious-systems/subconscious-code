//! The mathematical layer (DESIGN §6), grouped by how load-bearing each piece is.
//!
//! Load-bearing (real speed):
//!   - `fountain`  RaptorQ-style rateless erasure coding for cold-start bulk (§6.4)
//!   - `rlnc`      Random Linear Network Coding for multicast/multipath (§6.5)
//! Cheap + optimal:
//!   - `placement` golden-ratio low-discrepancy ring placement (§6.2)
//!   - `fib_hash`  Fibonacci multiplicative hashing for session->shard (§6.1)
//! Solid engineering:
//!   - `zeckendorf` Fibonacci-coded self-synchronizing wire varints (§6.3)
//!   - `cayley`    circulant/Cayley replication overlay topology (§6.7)
//! Selective:
//!   - `homohash`  homomorphic hash securing coded blocks (§6.6)
//! Marginal (kept, not headline):
//!   - `fib_backoff` Fibonacci congestion-window growth law (§6.8)
//!
//! Field arithmetic lives in `gf256` and is shared by `fountain`, `rlnc` and
//! `homohash`.

// The erasure-coding kernels index several byte arrays by the same loop
// counter (GF(256) matrix ops), and the memoized generator cache uses a
// deeply nested static type. These lints flag math that is clearer written
// explicitly than rewritten to iterators or type aliases.
#![allow(
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::ptr_arg,
    clippy::while_let_loop,
    clippy::doc_lazy_continuation
)]

pub mod bulk;
pub mod cayley;
pub mod fib_backoff;
pub mod fib_hash;
pub mod fountain;
pub mod gf256;
pub mod hierarchical;
pub mod homohash;
pub mod placement;
pub mod rlnc;
pub mod rs;
pub mod zeckendorf;

pub use bulk::{BulkConfig, BulkError};
pub use cayley::{CayleyError, CayleyGraph};
pub use fib_backoff::FibBackoff;
pub use fib_hash::fib_hash64;
pub use fountain::{FountainDecoder, FountainEncoder, FountainError};
pub use gf256::GF_SIZE;
pub use hierarchical::{HierDecoder, HierEncoder, HierError};
pub use homohash::{HomoHashError, HomomorphicHash};
pub use placement::{placement, GoldenRing};
pub use rlnc::{RlncDecoder, RlncEncoder, RlncError};
pub use rs::{RsEncoder, RsError};
pub use zeckendorf::{zeck_decode, zeck_encode, ZeckStream};
