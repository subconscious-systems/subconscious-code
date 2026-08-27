//! Property-based fuzzing of the wire codec and canonical parser.
//!
//! A lightweight, CI-stable (runs on stable Rust) stand-in for a libfuzzer
//! harness over `decode_frame` / `from_canonical` / `from_canonical_owned` -
//! the input-munching parsers on the cold-start recovery path. The
//! length-field caps in `frame.rs` / `canonical.rs` are what keep the
//! "arbitrary input" and "truncation" properties from over-allocating.
//!
//! Properties checked:
//!   - canonical encode/decode round-trips, and the owned path derives the
//!     same `block_id` as `block_id_from_canonical`;
//!   - frame encode/decode round-trips for every frame variant;
//!   - arbitrary bytes fed to either parser never panic (Ok or Err only);
//!   - no proper prefix of a valid frame decodes as a complete frame.

use bytes::Bytes;
use dlr_core::{
    block_id_from_canonical, canonical_bytes, decode_frame, encode_frame, from_canonical,
    from_canonical_owned, AckFrame, AppendFrame, Block, BlockId, BlockKind, BulkFrame, Frame,
    FrameBlock, MissingFrame, ResyncFrame,
};
use proptest::prelude::*;

fn any_block_kind() -> impl Strategy<Value = BlockKind> {
    (1u8..=5).prop_map(|b| BlockKind::from_byte(b).unwrap())
}

fn any_block() -> impl Strategy<Value = Block> {
    (
        any_block_kind(),
        any::<u64>(),
        proptest::collection::vec(any::<u8>(), 0..128),
    )
        .prop_map(|(kind, seq, payload)| Block::new(kind, seq, payload))
}

fn any_block_id() -> impl Strategy<Value = BlockId> {
    any::<[u8; 32]>()
}

proptest! {
    #[test]
    fn canonical_roundtrip(
        kind in any_block_kind(),
        seq in any::<u64>(),
        payload in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let block = Block::new(kind, seq, payload.clone());
        let canon = canonical_bytes(&block);
        let back = from_canonical(&canon).unwrap();
        prop_assert_eq!(back.kind, kind);
        prop_assert_eq!(back.seq, seq);
        prop_assert_eq!(&*back.payload, &payload[..]);

        let (back2, id) = from_canonical_owned(canon.clone()).unwrap();
        prop_assert_eq!(back2.kind, kind);
        prop_assert_eq!(back2.seq, seq);
        prop_assert_eq!(&*back2.payload, &payload[..]);
        prop_assert_eq!(id, block_id_from_canonical(&canon));
    }

    #[test]
    fn canonical_arbitrary_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let _ = from_canonical(&bytes);
        let _ = from_canonical_owned(bytes);
    }

    #[test]
    fn frame_append_roundtrip(
        sid in any::<u128>(),
        root in any_block_id(),
        inlines in proptest::collection::vec(any_block(), 0..8),
        refs in proptest::collection::vec(any_block_id(), 0..4),
    ) {
        let mut blocks = Vec::new();
        for b in inlines {
            blocks.push(FrameBlock::Inline(b));
        }
        for r in refs {
            blocks.push(FrameBlock::Ref(r));
        }
        let frame = Frame::Append(AppendFrame {
            session_id: sid,
            base_root: root,
            blocks,
        });
        let wire = encode_frame(&frame);
        let back = decode_frame(&wire).unwrap();
        let wire2 = encode_frame(&back);
        prop_assert_eq!(&wire[..], &wire2[..]);
    }

    #[test]
    fn frame_resync_roundtrip(
        sid in any::<u128>(),
        root in any_block_id(),
        manifest in proptest::collection::vec(any_block_id(), 0..16),
    ) {
        let frame = Frame::Resync(ResyncFrame {
            session_id: sid,
            client_root: root,
            manifest,
        });
        let wire = encode_frame(&frame);
        let back = decode_frame(&wire).unwrap();
        let wire2 = encode_frame(&back);
        prop_assert_eq!(&wire[..], &wire2[..]);
    }

    #[test]
    fn frame_missing_roundtrip(
        sid in any::<u128>(),
        missing in proptest::collection::vec(any_block_id(), 0..16),
    ) {
        let frame = Frame::Missing(MissingFrame {
            session_id: sid,
            missing,
        });
        let wire = encode_frame(&frame);
        let back = decode_frame(&wire).unwrap();
        let wire2 = encode_frame(&back);
        prop_assert_eq!(&wire[..], &wire2[..]);
    }

    #[test]
    fn frame_ack_roundtrip(sid in any::<u128>(), root in any_block_id()) {
        let frame = Frame::Ack(AckFrame {
            session_id: sid,
            root,
        });
        let wire = encode_frame(&frame);
        let back = decode_frame(&wire).unwrap();
        let wire2 = encode_frame(&back);
        prop_assert_eq!(&wire[..], &wire2[..]);
    }

    #[test]
    fn frame_bulk_roundtrip(
        sid in any::<u128>(),
        gen in any::<u32>(),
        k in 1u32..64,
        symbol_size in 1u32..256,
        nsym in 0usize..8,
        seed in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        // symbols are arbitrary byte strings; the codec carries them verbatim.
        let mut symbols = Vec::with_capacity(nsym);
        for i in 0..nsym {
            let mut s = seed.clone();
            s.push(i as u8);
            symbols.push(Bytes::from(s));
        }
        let frame = Frame::Bulk(BulkFrame {
            session_id: sid,
            generation: gen,
            k,
            symbol_size,
            symbols,
        });
        let wire = encode_frame(&frame);
        let back = decode_frame(&wire).unwrap();
        let wire2 = encode_frame(&back);
        prop_assert_eq!(&wire[..], &wire2[..]);
    }

    #[test]
    fn frame_arbitrary_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        // arbitrary input: Ok or Err, never a panic / over-allocation
        let _ = decode_frame(&bytes);
    }

    #[test]
    fn truncated_frame_never_decodes(
        sid in any::<u128>(),
        root in any_block_id(),
        payload in proptest::collection::vec(any::<u8>(), 1..128),
    ) {
        let block = Block::new(BlockKind::Message, 1, payload);
        let frame = Frame::Append(AppendFrame {
            session_id: sid,
            base_root: root,
            blocks: vec![FrameBlock::Inline(block)],
        });
        let wire = encode_frame(&frame);
        // no proper prefix should decode as a complete frame
        for len in 0..wire.len() {
            if let Ok(f) = decode_frame(&wire[..len]) {
                panic!(
                    "prefix of length {len} (of {}) decoded as a complete frame: {f:?}",
                    wire.len()
                );
            }
        }
    }
}
