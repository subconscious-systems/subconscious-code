//! Wire frames (DESIGN §3.2, §3.3).
//!
//! Steady-state frame:
//!   APPEND { session_id : u128, base_root : `[u8;32]`, blocks : [FrameBlock],
//!            coeff_hdr : optional }
//! The single `base_root` replaces any per-block manifest on the hot path.
//!
//! Resync / cold start (the only full transfer):
//!   RESYNC { session_id, client_root, manifest: [BlockId] }
//!   BULK   { session_id, generation, k, symbol_size, symbols: [coded bytes] }
//!   ACK    { session_id, root }
//!
//! `FrameBlock` is either `Inline` (full canonical payload) or `Ref` (a
//! block_id already in the receiver's store — the dedup path that lets a turn
//! repeat prior content without re-sending its bytes).

use bytes::{BufMut, Bytes, BytesMut};

use crate::block::{Block, BlockId, BlockKind};
use crate::merkle::MerkleRoot;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame truncated")]
    Truncated,
    #[error("unknown frame tag {0:#x}")]
    UnknownTag(u8),
    #[error("unknown block kind {0}")]
    UnknownKind(u8),
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("referenced block id not present")]
    MissingRef,
}

// Frame tags. The high bit of a block entry marks Ref (1) vs Inline (0).
const TAG_APPEND: u8 = 1;
const TAG_RESYNC: u8 = 2;
const TAG_BULK: u8 = 3;
const TAG_ACK: u8 = 4;
const TAG_MISSING: u8 = 5;
const REF_BIT: u8 = 0x80;

/// Upper bound on a single inline block's payload carried in a frame. Mirrors
/// `canonical::MAX_BLOCK_PAYLOAD_BYTES` (the inline body *is* a canonical
/// block); declared here so the wire decoder doesn't depend on the canonical
/// module's ceiling and so the cap is checked before the zero-copy slice is
/// taken. A claimed `plen` larger than this is rejected as `TooLarge` rather
/// than driving a slice off the end of the transport buffer.
const MAX_INLINE_PAYLOAD_BYTES: u64 = 1 << 30; // 1 GiB
/// Upper bound on the count of variable-length entries in one frame (blocks,
/// manifest ids, bulk symbols, missing ids). The decoder pre-reserves
/// `Vec::with_capacity(n)`; an unbounded `n` from a malformed frame would try
/// to reserve that capacity up front. A real frame is many orders of
/// magnitude under this.
const MAX_FRAME_COUNT: usize = 1 << 24; // 16M entries

/// A frame on the wire.
#[derive(Debug, Clone)]
pub enum Frame {
    Append(AppendFrame),
    Resync(ResyncFrame),
    Bulk(BulkFrame),
    Ack(AckFrame),
    /// The receiver's answer to RESYNC: the block ids it is missing (§3.3 step 2).
    /// The sender then codes *only* this set (sparse reconstruction) rather than
    /// everything after its base — a warm-but-diverged receiver (network blip
    /// with partial state) names a far smaller set than "all after base".
    Missing(MissingFrame),
}

#[derive(Debug, Clone)]
pub struct AppendFrame {
    pub session_id: u128,
    pub base_root: MerkleRoot,
    pub blocks: Vec<FrameBlock>,
}

#[derive(Debug, Clone)]
pub enum FrameBlock {
    /// New content, full canonical payload.
    Inline(Block),
    /// Content already in the receiver store; referenced by id.
    Ref(BlockId),
}

#[derive(Debug, Clone)]
pub struct ResyncFrame {
    pub session_id: u128,
    pub client_root: MerkleRoot,
    /// Ordered block ids the client has (the manifest). ~100KB for 50M tokens.
    pub manifest: Vec<BlockId>,
}

#[derive(Debug, Clone)]
pub struct BulkFrame {
    pub session_id: u128,
    pub generation: u32,
    pub k: u32,
    pub symbol_size: u32,
    /// Each entry is a compactly-encoded coded symbol (see coding crate).
    pub symbols: Vec<Bytes>,
}

#[derive(Debug, Clone)]
pub struct AckFrame {
    pub session_id: u128,
    pub root: MerkleRoot,
}

/// Receiver → sender: the block ids the receiver is missing after a RESYNC.
/// Drives sparse reconstruction (§3.3): the sender fountain-codes only these.
#[derive(Debug, Clone)]
pub struct MissingFrame {
    pub session_id: u128,
    pub missing: Vec<BlockId>,
}

/// Encode a frame to bytes. Length-prefixed by the transport; here we emit the
/// body. Tagged so the receiver dispatches without parsing.
pub fn encode_frame(f: &Frame) -> Bytes {
    let mut buf = match f {
        Frame::Append(a) => {
            // Estimate capacity to avoid reallocations: 1 tag + 16 sid + 32 root
            // + 4 count, plus each frame block's body (21-byte header for inline
            // + payload, 37 for a bare ref id).
            let est = 53 + a.blocks.iter().map(frame_block_size).sum::<usize>();
            BytesMut::with_capacity(est)
        }
        Frame::Resync(r) => BytesMut::with_capacity(53 + r.manifest.len() * 32),
        Frame::Bulk(b) => {
            // Header is 1 tag + 16 sid + 4 gen + 4 k + 4 symbol_size + 4 count
            // = 33 bytes (was 21, which under-allocated every Bulk frame and
            // forced a regrowth). Per symbol: 4-byte length + bytes.
            BytesMut::with_capacity(33 + b.symbols.iter().map(|s| 4 + s.len()).sum::<usize>())
        }
        Frame::Missing(m) => BytesMut::with_capacity(21 + m.missing.len() * 32),
        Frame::Ack(_) => BytesMut::with_capacity(1 + 16 + 32),
    };
    match f {
        Frame::Append(a) => {
            buf.put_u8(TAG_APPEND);
            put_u128(&mut buf, a.session_id);
            buf.put_slice(&a.base_root);
            put_u32_le(&mut buf, a.blocks.len() as u32);
            for fb in &a.blocks {
                encode_frame_block(&mut buf, fb);
            }
        }
        Frame::Resync(r) => {
            buf.put_u8(TAG_RESYNC);
            put_u128(&mut buf, r.session_id);
            buf.put_slice(&r.client_root);
            put_u32_le(&mut buf, r.manifest.len() as u32);
            for id in &r.manifest {
                buf.put_slice(id);
            }
        }
        Frame::Bulk(b) => {
            buf.put_u8(TAG_BULK);
            put_u128(&mut buf, b.session_id);
            put_u32_le(&mut buf, b.generation);
            put_u32_le(&mut buf, b.k);
            put_u32_le(&mut buf, b.symbol_size);
            put_u32_le(&mut buf, b.symbols.len() as u32);
            for s in &b.symbols {
                put_u32_le(&mut buf, s.len() as u32);
                buf.put_slice(s);
            }
        }
        Frame::Ack(a) => {
            buf.put_u8(TAG_ACK);
            put_u128(&mut buf, a.session_id);
            buf.put_slice(&a.root);
        }
        Frame::Missing(m) => {
            buf.put_u8(TAG_MISSING);
            put_u128(&mut buf, m.session_id);
            put_u32_le(&mut buf, m.missing.len() as u32);
            for id in &m.missing {
                buf.put_slice(id);
            }
        }
    }
    buf.freeze()
}

/// On-wire size of a single `FrameBlock` (for capacity estimation only).
#[inline]
fn frame_block_size(fb: &FrameBlock) -> usize {
    match fb {
        FrameBlock::Inline(b) => 4 + 1 + 8 + 8 + b.payload.len(),
        FrameBlock::Ref(_) => 4 + 1 + 32,
    }
}

fn encode_frame_block(buf: &mut BytesMut, fb: &FrameBlock) {
    match fb {
        FrameBlock::Inline(b) => {
            // Body layout (mirrors `canonical_bytes` minus the duplicated kind):
            // kind:u8 | seq:u64_le | len:u64_le | payload — length-prefixed by a
            // u32 body length. Writing the fields directly avoids allocating a
            // `canonical_bytes` Vec per inline block on the wire hot path.
            let body_len = 1 + 8 + 8 + b.payload.len();
            put_u32_le(buf, body_len as u32);
            // tag byte: kind (low 7 bits), REF_BIT clear
            buf.put_u8(b.kind.to_byte() & !REF_BIT);
            buf.extend_from_slice(&b.seq.to_le_bytes());
            buf.extend_from_slice(&(b.payload.len() as u64).to_le_bytes());
            buf.extend_from_slice(&b.payload);
        }
        FrameBlock::Ref(id) => {
            put_u32_le(buf, 33); // body length: REF_BIT marker (1) + bare id (32)
            buf.put_u8(REF_BIT); // marker: ref
            buf.put_slice(id);
        }
    }
}

pub fn decode_frame(buf: &[u8]) -> Result<Frame, FrameError> {
    if buf.is_empty() {
        return Err(FrameError::Truncated);
    }
    let tag = buf[0];
    let body = &buf[1..];
    // `Bytes` borrow with `&Bytes` input: this is the zero-copy entry that
    // lets the receiver slice payloads / symbols out of the transport buffer
    // instead of copying. Reached via `decode_frame_bytes`; here we wrap the
    // borrowed slice in a `Bytes` so the payload views take the fast, cheap
    // path (`Bytes::slice`, copy-on-refcount).
    let owned = Bytes::copy_from_slice(body);
    decode_body(tag, &owned)
}

/// Zero-copy frame decode over a transport `Bytes` buffer.
///
/// The loopback/QUIC/RDMA layers hand frames out as `Bytes`; this entry decodes
/// inline payloads and Bulk symbols as **slices** of that buffer (refcount-
/// extended views) instead of `Bytes::copy_from_slice`-ing every one. On the
/// cold-start Bulk path that removes a full pass over the ~200 MB stream; on
/// the append path it removes the per-inline-block payload copy.
pub fn decode_frame_bytes(buf: &Bytes) -> Result<Frame, FrameError> {
    if buf.is_empty() {
        return Err(FrameError::Truncated);
    }
    let tag = buf[0];
    let body = buf.slice(1..);
    decode_body(tag, &body)
}

fn decode_body(tag: u8, body: &Bytes) -> Result<Frame, FrameError> {
    let mut r: &[u8] = body;
    match tag {
        TAG_APPEND => {
            let session_id = take_u128(&mut r)?;
            let base_root = take_root(&mut r)?;
            let n = take_count(&mut r)?;
            let mut blocks = Vec::with_capacity(n);
            for _ in 0..n {
                blocks.push(decode_frame_block(&mut r, body)?);
            }
            Ok(Frame::Append(AppendFrame {
                session_id,
                base_root,
                blocks,
            }))
        }
        TAG_RESYNC => {
            let session_id = take_u128(&mut r)?;
            let client_root = take_root(&mut r)?;
            let n = take_count(&mut r)?;
            let mut manifest = Vec::with_capacity(n);
            for _ in 0..n {
                manifest.push(take_block_id(&mut r)?);
            }
            Ok(Frame::Resync(ResyncFrame {
                session_id,
                client_root,
                manifest,
            }))
        }
        TAG_BULK => {
            let session_id = take_u128(&mut r)?;
            let generation = take_u32(&mut r)?;
            let k = take_u32(&mut r)?;
            let symbol_size = take_u32(&mut r)?;
            let n = take_count(&mut r)?;
            let mut symbols = Vec::with_capacity(n);
            for _ in 0..n {
                let len = take_u32(&mut r)? as usize;
                if len > MAX_INLINE_PAYLOAD_BYTES as usize {
                    return Err(FrameError::TooLarge(len));
                }
                if r.len() < len {
                    return Err(FrameError::Truncated);
                }
                // Zero-copy symbol view of the transport buffer: `r` is a
                // moving window over `body`, so the offset is the consumed
                // prefix length.
                let off = body.len() - r.len();
                symbols.push(body.slice(off..off + len));
                r = &r[len..];
            }
            Ok(Frame::Bulk(BulkFrame {
                session_id,
                generation,
                k,
                symbol_size,
                symbols,
            }))
        }
        TAG_ACK => {
            let session_id = take_u128(&mut r)?;
            let root = take_root(&mut r)?;
            Ok(Frame::Ack(AckFrame { session_id, root }))
        }
        TAG_MISSING => {
            let session_id = take_u128(&mut r)?;
            let n = take_count(&mut r)?;
            let mut missing = Vec::with_capacity(n);
            for _ in 0..n {
                missing.push(take_block_id(&mut r)?);
            }
            Ok(Frame::Missing(MissingFrame {
                session_id,
                missing,
            }))
        }
        other => Err(FrameError::UnknownTag(other)),
    }
}

/// Decode one `FrameBlock` from the contiguous body. `body` is the buffer to
/// slice inline payloads out of (a `Bytes` the caller owns/borrows); `r` is
/// the moving window *into* `body`.
fn decode_frame_block(r: &mut &[u8], body: &Bytes) -> Result<FrameBlock, FrameError> {
    let len = take_u32(r)? as usize;
    if r.len() < len {
        return Err(FrameError::Truncated);
    }
    let off = body.len() - r.len(); // absolute offset of this block in `body`
    let marker = r[0];
    if marker & REF_BIT != 0 {
        // Ref: body[0] is REF_BIT, body[1..33] is the id
        if len != 33 {
            return Err(FrameError::Truncated);
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&r[1..33]);
        *r = &r[len..];
        Ok(FrameBlock::Ref(id))
    } else {
        let kind =
            BlockKind::from_byte(marker & !REF_BIT).ok_or(FrameError::UnknownKind(marker))?;
        if len < 1 + 16 {
            return Err(FrameError::Truncated);
        }
        let seq = u64::from_le_bytes(r[1..9].try_into().unwrap());
        let plen = u64::from_le_bytes(r[9..17].try_into().unwrap());
        if plen > MAX_INLINE_PAYLOAD_BYTES {
            return Err(FrameError::TooLarge(plen as usize));
        }
        let plen = plen as usize;
        if len < 1 + 16 + plen {
            return Err(FrameError::Truncated);
        }
        // Zero-copy payload: a refcount-extended window of the transport buffer.
        let payload = body.slice(off + 1 + 16..off + 1 + 16 + plen);
        *r = &r[1 + 16 + plen..];
        Ok(FrameBlock::Inline(Block { kind, seq, payload }))
    }
}

// --- small reader helpers ---

fn put_u128(buf: &mut BytesMut, v: u128) {
    // Little-endian, matching `take_u128`'s `u128::from_le_bytes`. (The `bytes`
    // crate's `BufMut::put_u64` is big-endian, so composing it here as
    // `put_u64(lo); put_u64(hi)` would scramble the session id against the LE
    // decoder — write the raw LE bytes directly.)
    buf.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_u32_le(buf: &mut BytesMut, v: u32) {
    // Little-endian, matching `take_u32`'s `u32::from_le_bytes`. `BufMut::put_u32`
    // is big-endian; using it for the count/length fields made the decoder read
    // a small count (e.g. 3) as ~50M and run off the end of the frame (Truncated).
    buf.extend_from_slice(&v.to_le_bytes());
}

fn take_u32(r: &mut &[u8]) -> Result<u32, FrameError> {
    if r.len() < 4 {
        return Err(FrameError::Truncated);
    }
    let v = u32::from_le_bytes([r[0], r[1], r[2], r[3]]);
    *r = &r[4..];
    Ok(v)
}
/// Read a u32 count and bound it by `MAX_FRAME_COUNT`. Frames pre-reserve
/// `Vec::with_capacity(n)` for their entry lists; a malformed count would
/// otherwise drive that reservation (and, for id lists, the per-entry read
/// loop) off into OOM territory before the trailing bounds check rejects it.
fn take_count(r: &mut &[u8]) -> Result<usize, FrameError> {
    let n = take_u32(r)? as usize;
    if n > MAX_FRAME_COUNT {
        return Err(FrameError::TooLarge(n));
    }
    Ok(n)
}
fn take_u128(r: &mut &[u8]) -> Result<u128, FrameError> {
    if r.len() < 16 {
        return Err(FrameError::Truncated);
    }
    let mut a = [0u8; 16];
    a.copy_from_slice(&r[..16]);
    *r = &r[16..];
    Ok(u128::from_le_bytes(a))
}
fn take_root(r: &mut &[u8]) -> Result<MerkleRoot, FrameError> {
    take_block_id(r)
}
fn take_block_id(r: &mut &[u8]) -> Result<BlockId, FrameError> {
    if r.len() < 32 {
        return Err(FrameError::Truncated);
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&r[..32]);
    *r = &r[32..];
    Ok(id)
}

#[cfg(test)]
mod tests {
    //! Regression coverage for the wire codec. The integer fields were once
    //! encoded with `BufMut::put_u32`/`put_u64` (big-endian) but decoded with
    //! `from_le_bytes` (little-endian), so a block count of 3 was read as ~50M
    //! and every frame ran off the end (Truncated). These roundtrips pin the
    //! endianness, the 33-byte Ref body length, and the u128 session id.
    use super::*;

    fn block(kind: BlockKind, seq: u64, payload: &[u8]) -> Block {
        Block::new(kind, seq, Bytes::copy_from_slice(payload))
    }

    fn assert_append_roundtrip(a: &AppendFrame) {
        let wire = encode_frame(&Frame::Append(a.clone()));
        let got = decode_frame_bytes(&wire).expect("decode append");
        match got {
            Frame::Append(g) => {
                assert_eq!(g.session_id, a.session_id, "session_id mismatch");
                assert_eq!(g.base_root, a.base_root, "base_root mismatch");
                assert_eq!(g.blocks.len(), a.blocks.len(), "block count mismatch");
                for (gi, fb) in g.blocks.iter().enumerate() {
                    match (fb, &a.blocks[gi]) {
                        (FrameBlock::Inline(b), FrameBlock::Inline(eb)) => {
                            assert_eq!(b.kind, eb.kind, "inline kind @ {gi}");
                            assert_eq!(b.seq, eb.seq, "inline seq @ {gi}");
                            assert_eq!(b.payload, eb.payload, "inline payload @ {gi}");
                        }
                        (FrameBlock::Ref(id), FrameBlock::Ref(eid)) => {
                            assert_eq!(id, eid, "ref id @ {gi}");
                        }
                        _ => panic!("block kind mismatch @ {gi}"),
                    }
                }
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    #[test]
    #[allow(clippy::unusual_byte_groupings)] // intentional wordplay: A11CE 0001 DEAD BEEF
    fn append_roundtrip_inline_and_ref() {
        // A session id with both low and high 64 bits nonzero — catches the
        // old `put_u64(lo); put_u64(hi)` BE scrambling against the LE decoder.
        let sid: u128 = 0xA11CE_0001_DEAD_BEEF;
        let mut root = [0u8; 32];
        for (i, b) in root.iter_mut().enumerate() {
            *b = i as u8;
        }
        let id_a = block(BlockKind::Message, 1, b"hello").block_id();
        let id_b = block(BlockKind::ToolCall, 2, b"world").block_id();
        let append = AppendFrame {
            session_id: sid,
            base_root: root,
            blocks: vec![
                FrameBlock::Inline(block(BlockKind::Message, 1, b"hello")),
                FrameBlock::Inline(block(BlockKind::ToolResult, 3, &b"payload bytes"[..])),
                FrameBlock::Ref(id_a),
                FrameBlock::Ref(id_b),
            ],
        };
        assert_append_roundtrip(&append);
    }

    #[test]
    fn append_roundtrip_empty_blocks() {
        // A count of 0 once decoded as ~50M; pin that an empty block vec
        // roundtrips and stays empty.
        let append = AppendFrame {
            session_id: 1,
            base_root: [0u8; 32],
            blocks: Vec::new(),
        };
        assert_append_roundtrip(&append);
    }

    #[test]
    fn bulk_roundtrip() {
        let sym1 = Bytes::from_static(b"coded-symbol-1");
        let sym2 = Bytes::from_static(b"coded-symbol-2-longer");
        let bulk = BulkFrame {
            session_id: 0xBEEF,
            generation: 7,
            k: 32,
            symbol_size: 1024,
            symbols: vec![sym1.clone(), sym2.clone()],
        };
        let wire = encode_frame(&Frame::Bulk(bulk.clone()));
        match decode_frame_bytes(&wire).expect("decode bulk") {
            Frame::Bulk(g) => {
                assert_eq!(g.session_id, bulk.session_id);
                assert_eq!(g.generation, bulk.generation);
                assert_eq!(g.k, bulk.k);
                assert_eq!(g.symbol_size, bulk.symbol_size);
                assert_eq!(g.symbols, bulk.symbols);
            }
            other => panic!("expected Bulk, got {other:?}"),
        }
    }

    #[test]
    fn resync_and_ack_and_missing_roundtrip() {
        let root = block(BlockKind::Message, 9, b"r").block_id();
        let resync = ResyncFrame {
            session_id: 0xCAFE,
            client_root: root,
            manifest: vec![[0xAA; 32], [0xBB; 32], [0xCC; 32]],
        };
        let wire = encode_frame(&Frame::Resync(resync.clone()));
        match decode_frame_bytes(&wire).expect("decode resync") {
            Frame::Resync(g) => {
                assert_eq!(g.session_id, resync.session_id);
                assert_eq!(g.client_root, resync.client_root);
                assert_eq!(g.manifest, resync.manifest);
            }
            other => panic!("expected Resync, got {other:?}"),
        }

        let ack = AckFrame {
            session_id: 0xFEED,
            root,
        };
        let wire = encode_frame(&Frame::Ack(ack.clone()));
        match decode_frame_bytes(&wire).expect("decode ack") {
            Frame::Ack(g) => assert_eq!((g.session_id, g.root), (ack.session_id, ack.root)),
            other => panic!("expected Ack, got {other:?}"),
        }

        let missing = MissingFrame {
            session_id: 0xF00D,
            missing: vec![[0x11; 32]],
        };
        let wire = encode_frame(&Frame::Missing(missing.clone()));
        match decode_frame_bytes(&wire).expect("decode missing") {
            Frame::Missing(g) => {
                assert_eq!(g.session_id, missing.session_id);
                assert_eq!(g.missing, missing.missing);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frame_errors() {
        // A single tag byte with no body is not a valid frame of any kind.
        let wire = Bytes::from_static(&[TAG_APPEND]);
        assert!(matches!(
            decode_frame_bytes(&wire),
            Err(FrameError::Truncated)
        ));
        assert!(matches!(
            decode_frame_bytes(&Bytes::new()),
            Err(FrameError::Truncated)
        ));
    }
}
