//! End-to-end protocol lifecycle tests driving a real `Shim` against a real
//! `Receiver` over the frame codec (`encode_frame` / `decode_frame_bytes`).
//!
//! These are the first tests covering the protocol endpoints (`dlr-shim` +
//! `dlr-receiver`): steady-state round-trips, dedup, the full cold-start
//! handshake (RESYNC → Missing → BULK → completion ACK), out-of-order BULK
//! delivery, post-cold-start steady-state resumption, warm resync, batch
//! coalescing, multi-session isolation, and partial/resumable bulk.
//!
//! The receiver always gets its **own** `ContentStore` (never the shim's) —
//! the two sides communicate only via frames, exactly as they would across a
//! transport boundary.

use std::sync::Arc;

use bytes::Bytes;
use dlr_compress::Compressor;
use dlr_core::{
    decode_frame_bytes, encode_frame, Block, BlockKind, ContentStore, CuckooFilter, Frame,
    FrameBlock, MerkleRoot, ROOT_ZERO,
};
use dlr_receiver::{Receiver, ReceiverError};
use dlr_shim::Shim;

const SID_A: u128 = 0xA;
const SID_B: u128 = 0xB;

/// A deterministic payload generator — no RNG, so test runs are reproducible.
fn payload(seed: usize, len: usize) -> Bytes {
    let mut out = Vec::with_capacity(len);
    let mut s = seed as u64;
    for _ in 0..len {
        // xorshift64
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.push((s & 0xFF) as u8);
    }
    Bytes::from(out)
}

fn block(seq: u64, payload: Bytes) -> Block {
    Block::new(BlockKind::Message, seq, payload)
}

/// Fresh shim + receiver pair sharing only the compressor's dictionary
/// convention (empty-dict default) — independent stores.
fn harness() -> (Shim, Receiver) {
    let compressor = Compressor::default();
    let filter = Arc::new(CuckooFilter::with_capacity(1 << 16));
    let shim = Shim::with_filter(ContentStore::new(), compressor.clone(), filter);
    let receiver = Receiver::new(ContentStore::new(), compressor);
    (shim, receiver)
}

/// Ship a frame to the receiver through the codec, returning its response.
fn ship(receiver: &Receiver, frame: Frame) -> Option<Frame> {
    let wire = encode_frame(&frame);
    receiver
        .handle_frame(decode_frame_bytes(&wire).expect("decode"))
        .expect("handle_frame")
}

fn expect_ack(resp: Option<Frame>) -> MerkleRoot {
    match resp {
        Some(Frame::Ack(a)) => a.root,
        other => panic!("expected ACK, got {other:?}"),
    }
}

/// Ingest a turn on the shim, ship the APPEND, apply the ACK, return the
/// ACKed root.
fn append_turn(shim: &Shim, receiver: &Receiver, sid: u128, blocks: Vec<Block>) -> MerkleRoot {
    let append = shim.ingest(sid, blocks);
    let resp = ship(receiver, Frame::Append(append));
    let root = expect_ack(resp);
    assert!(shim.apply_ack(sid, root), "ACK must advance shim base_root");
    root
}

/// The blocks the receiver currently holds for a session, in manifest order.
fn receiver_blocks(receiver: &Receiver, sid: u128) -> Vec<Block> {
    receiver.reconstruct(sid)
}

/// The receiver's authoritative root for a session (manifest order).
fn receiver_root(receiver: &Receiver, sid: u128) -> MerkleRoot {
    receiver.session_root(sid).expect("session present")
}

/// Enough blocks to span multiple fountain generations (k=64, sym=1024 ⇒ a
/// generation is 64 KiB of flat stream). ~300 × 512 B ≈ 150 KiB ⇒ 3 generations.
fn many_blocks(start_seq: u64) -> Vec<Block> {
    (0..300)
        .map(|i| {
            block(
                start_seq + i as u64,
                payload((start_seq as usize + i) * 7 + 1, 512),
            )
        })
        .collect()
}

#[test]
fn steady_state_roundtrip() {
    let (shim, receiver) = harness();
    let mut expected: Vec<Block> = Vec::new();

    for turn in 0..5 {
        let blocks = vec![
            block(turn * 2 + 1, payload((turn * 2 + 1) as usize, 64)),
            block(turn * 2 + 2, payload((turn * 2 + 2) as usize + 99, 128)),
        ];
        expected.extend(blocks.clone());
        let root = append_turn(&shim, &receiver, SID_A, blocks);
        // The receiver's root must match the shim's current root after the ACK.
        assert_eq!(root, shim.session(SID_A).root());
        // base_root advances to the ACKed root.
        assert_eq!(shim.session(SID_A).base_root(), shim.session(SID_A).root());
    }

    let got = receiver_blocks(&receiver, SID_A);
    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(&expected) {
        assert_eq!(g.kind, e.kind);
        assert_eq!(g.seq, e.seq);
        assert_eq!(g.payload, e.payload, "payload mismatch at seq {}", e.seq);
    }
}

#[test]
fn lost_ack_replay_is_idempotent() {
    let (shim, receiver) = harness();
    let append = shim.ingest(SID_A, vec![block(1, payload(1, 128))]);

    let first = receiver
        .handle_append(append.clone())
        .expect("first append");
    assert_eq!(receiver.reconstruct(SID_A).len(), 1);

    // Simulate an ACK disappearing in the network. The sender replays the
    // identical frame with its old base root; the receiver recognizes that
    // the target root is already current and must not append a second copy.
    let replay = receiver.handle_append(append).expect("idempotent replay");
    assert_eq!(replay.root, first.root);
    assert_eq!(receiver.reconstruct(SID_A).len(), 1);
}

#[test]
fn invalid_tail_reference_rejects_the_whole_append() {
    let (shim, receiver) = harness();
    let mut append = shim.ingest(SID_A, vec![block(1, payload(2, 128))]);
    append.blocks.push(FrameBlock::Ref([0xabu8; 32]));

    assert!(matches!(
        receiver.handle_append(append),
        Err(ReceiverError::MissingRef)
    ));
    assert_eq!(receiver.store().session_len(SID_A), 0);
    assert_eq!(receiver.store().session_root(SID_A), ROOT_ZERO);
}

#[test]
fn wal_repairs_a_truncated_tail_before_future_appends() {
    use std::io::Write;

    let wal_path = std::env::temp_dir().join(format!(
        "dlr-truncated-tail-{}-{}.wal",
        std::process::id(),
        SID_A
    ));
    let _ = std::fs::remove_file(&wal_path);

    let first = block(1, payload(3, 64));
    let first_root = {
        let store = ContentStore::with_wal(&wal_path).expect("create wal");
        store.insert(SID_A, first.clone());
        store.flush_wal(true).expect("flush first record");
        store.session_root(SID_A)
    };
    std::fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .unwrap()
        .write_all(b"partial")
        .unwrap();

    let second = block(2, payload(4, 64));
    let final_root = {
        let store = ContentStore::with_wal(&wal_path).expect("repair and replay wal");
        assert_eq!(store.session_root(SID_A), first_root);
        store.insert(SID_A, second.clone());
        store.flush_wal(true).expect("flush second record");
        store.session_root(SID_A)
    };

    let recovered = ContentStore::with_wal(&wal_path).expect("replay repaired wal");
    assert_eq!(recovered.session_root(SID_A), final_root);
    let blocks = recovered.reconstruct(SID_A);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].payload, first.payload);
    assert_eq!(blocks[1].payload, second.payload);
    let _ = std::fs::remove_file(wal_path);
}

#[test]
fn wal_rejects_a_second_live_writer() {
    let wal_path = std::env::temp_dir().join(format!(
        "dlr-exclusive-writer-{}-{}.wal",
        std::process::id(),
        SID_A
    ));
    let _ = std::fs::remove_file(&wal_path);
    let first = ContentStore::with_wal(&wal_path).expect("first WAL writer");
    assert!(
        ContentStore::with_wal(&wal_path).is_err(),
        "a second live writer must not acquire the same WAL"
    );
    drop(first);
    ContentStore::with_wal(&wal_path).expect("lock released after writer drop");
    let _ = std::fs::remove_file(wal_path);
}

#[test]
fn dedup_emits_ref() {
    let (shim, receiver) = harness();
    // The same block (same id — content + seq) ingested twice in one turn: the
    // first occurrence is `Inline` (stored), the second is a `Ref` (already in
    // the shim's content-addressed cache), not a re-sent inline payload.
    let dup = payload(42, 200);
    let b = block(1, dup.clone());

    let append = shim.ingest(SID_A, vec![b.clone(), b.clone()]);
    assert!(
        matches!(append.blocks[0], FrameBlock::Inline(_)),
        "first occurrence is inline"
    );
    assert!(
        matches!(append.blocks[1], FrameBlock::Ref(_)),
        "second occurrence is a Ref, not re-sent inline"
    );
    let resp = ship(&receiver, Frame::Append(append));
    assert!(shim.apply_ack(SID_A, expect_ack(resp)));

    // The receiver resolves the Ref to the inline block from the same frame and
    // reconstructs both positions (content-addressed).
    let got = receiver_blocks(&receiver, SID_A);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].payload, dup);
    assert_eq!(got[1].payload, dup);
}

#[test]
fn cold_start_in_order() {
    let (shim, receiver) = harness();
    let blocks = many_blocks(1);
    append_turn(&shim, &receiver, SID_A, blocks.clone());

    // A fresh cold receiver has none of it.
    let cold = Receiver::new(ContentStore::new(), Compressor::default());
    let resync = shim.session(SID_A).resync_frame();
    let client_root = resync.client_root;
    assert_eq!(client_root, shim.session(SID_A).root());

    let resp = ship(&cold, Frame::Resync(resync));
    let missing = match resp {
        Some(Frame::Missing(m)) => m.missing,
        other => panic!("expected Missing, got {other:?}"),
    };
    assert!(
        !missing.is_empty(),
        "cold receiver should be missing blocks"
    );

    let bulk = shim
        .session(SID_A)
        .bulk_frames_for(&missing, 5)
        .expect("bulk encode");
    assert!(bulk.len() >= 2, "test expects multiple generations");

    let mut completion: Option<MerkleRoot> = None;
    for b in &bulk {
        if let Some(Frame::Ack(a)) = ship(&cold, Frame::Bulk(b.clone())) {
            assert!(completion.is_none(), "ACK should fire once");
            completion = Some(a.root);
        }
    }
    let ack_root = completion.expect("cold start must complete and ACK");
    assert_eq!(ack_root, client_root, "completion ACKs the client root");

    let got = cold.reconstruct(SID_A);
    assert_eq!(got.len(), blocks.len());
    for (g, e) in got.iter().zip(&blocks) {
        assert_eq!(g.payload, e.payload);
    }
    assert_eq!(
        receiver_root(&cold, SID_A),
        client_root,
        "receiver authoritative root == client root"
    );
}

#[test]
fn cold_start_out_of_order() {
    // Regression test for the divergence-livelock bug: BULK generations
    // arriving out of (generation) order must still complete the cold start
    // and let steady state resume. Pre-fix, the store's insertion-order root
    // ≠ client_root after out-of-order bulk, so the next APPEND was rejected.
    let (shim, receiver) = harness();
    let blocks = many_blocks(1);
    append_turn(&shim, &receiver, SID_A, blocks.clone());

    let cold = Receiver::new(ContentStore::new(), Compressor::default());
    let resync = shim.session(SID_A).resync_frame();
    let client_root = resync.client_root;
    let resp = ship(&cold, Frame::Resync(resync));
    let missing = match resp {
        Some(Frame::Missing(m)) => m.missing,
        other => panic!("expected Missing, got {other:?}"),
    };
    let mut bulk = shim
        .session(SID_A)
        .bulk_frames_for(&missing, 5)
        .expect("bulk encode");
    assert!(bulk.len() >= 2, "need multiple generations to reorder");
    bulk.reverse(); // deliver last generation first

    let mut completion = None;
    for b in &bulk {
        if let Some(Frame::Ack(a)) = ship(&cold, Frame::Bulk(b.clone())) {
            assert!(completion.is_none(), "ACK fires once");
            completion = Some(a.root);
        }
    }
    let ack_root = completion.expect("out-of-order bulk must still complete");
    assert_eq!(ack_root, client_root);

    // Reconstruct is manifest-ordered regardless of arrival order.
    let got = cold.reconstruct(SID_A);
    assert_eq!(got.len(), blocks.len());
    for (g, e) in got.iter().zip(&blocks) {
        assert_eq!(g.payload, e.payload);
    }
    assert_eq!(receiver_root(&cold, SID_A), client_root);
}

#[test]
fn post_cold_start_resumption() {
    // Regression test for the "no way to resume steady state after a cold
    // start" gap: after the completion ACK is applied, a new turn APPENDed to
    // the *cold* receiver must be accepted and ACKed with matching roots.
    let (shim, receiver) = harness();
    let blocks = many_blocks(1);
    append_turn(&shim, &receiver, SID_A, blocks.clone());

    let cold = Receiver::new(ContentStore::new(), Compressor::default());
    let resync = shim.session(SID_A).resync_frame();
    let resp = ship(&cold, Frame::Resync(resync));
    let missing = match resp {
        Some(Frame::Missing(m)) => m.missing,
        other => panic!("expected Missing, got {other:?}"),
    };
    let bulk = shim
        .session(SID_A)
        .bulk_frames_for(&missing, 5)
        .expect("bulk");
    let mut completion = None;
    for b in &bulk {
        if let Some(Frame::Ack(a)) = ship(&cold, Frame::Bulk(b.clone())) {
            completion = Some(a.root);
        }
    }
    let ack_root = completion.expect("cold start completes");
    assert!(shim.apply_ack(SID_A, ack_root), "shim advances base_root");
    assert_eq!(shim.session(SID_A).base_root(), ack_root);

    // Now resume steady state ON THE COLD RECEIVER.
    let extra = vec![block(1001, payload(0xAB, 300))];
    let append = shim.ingest(SID_A, extra.clone());
    let resp = ship(&cold, Frame::Append(append));
    let new_root = expect_ack(resp);
    assert_eq!(new_root, shim.session(SID_A).root());
    assert!(shim.apply_ack(SID_A, new_root));

    let got = cold.reconstruct(SID_A);
    assert_eq!(got.len(), blocks.len() + 1);
    assert_eq!(got.last().unwrap().payload, extra[0].payload);
}

#[test]
fn warm_resync_empty_missing() {
    // A receiver that already has every manifest block must ACK at RESYNC (not
    // demand a BULK transfer) so the shim advances base_root.
    let (shim, receiver) = harness();
    let blocks = many_blocks(1);
    append_turn(&shim, &receiver, SID_A, blocks.clone());
    let client_root = shim.session(SID_A).root();

    // The warm receiver already has everything.
    let resync = shim.session(SID_A).resync_frame();
    let resp = ship(&receiver, Frame::Resync(resync));
    let ack_root = match resp {
        Some(Frame::Ack(a)) => a.root,
        other => panic!("warm receiver should ACK, got {other:?}"),
    };
    assert_eq!(ack_root, client_root);
    assert!(shim.apply_ack(SID_A, ack_root));
}

#[test]
fn mid_cold_start_append_rejected() {
    // While a cold start is in progress (BULK incomplete), an APPEND must be
    // rejected with ColdStartInProgress — the defensive guard.
    let (shim, receiver) = harness();
    let blocks = many_blocks(1);
    append_turn(&shim, &receiver, SID_A, blocks.clone());

    let cold = Receiver::new(ContentStore::new(), Compressor::default());
    let resync = shim.session(SID_A).resync_frame();
    let resp = ship(&cold, Frame::Resync(resync));
    let missing = match resp {
        Some(Frame::Missing(m)) => m.missing,
        _ => unreachable!(),
    };
    let mut bulk = shim
        .session(SID_A)
        .bulk_frames_for(&missing, 5)
        .expect("bulk");
    // Deliver only the first generation — cold start incomplete.
    let first = bulk.remove(0);
    let _ = ship(&cold, Frame::Bulk(first));

    // An APPEND mid-cold-start must be rejected.
    let extra = vec![block(9001, payload(0x1, 64))];
    let append = shim.ingest(SID_A, extra);
    let wire = encode_frame(&Frame::Append(append));
    let res = cold.handle_frame(decode_frame_bytes(&wire).expect("decode"));
    assert!(
        matches!(res, Err(ref e) if e.to_string().contains("cold start in progress")),
        "mid-cold-start APPEND should be rejected, got {res:?}"
    );

    // Finishing the bulk completes the cold start and unblocks APPENDs.
    for b in &bulk {
        let _ = ship(&cold, Frame::Bulk(b.clone()));
    }
    let extra2 = vec![block(9002, payload(0x2, 64))];
    let append2 = shim.ingest(SID_A, extra2);
    // Apply the completion ACK to the shim first so base_root matches.
    // (Find it by replaying is unnecessary; just ship and expect an ACK now.)
    let resp = ship(&cold, Frame::Append(append2));
    // After completion, APPENDs are accepted again.
    assert!(
        matches!(resp, Some(Frame::Ack(_))),
        "APPEND accepted after cold start completes, got {resp:?}"
    );
}

#[test]
fn batch_coalescing() {
    let (shim, receiver) = harness();
    let turn1 = vec![block(1, payload(1, 64)), block(2, payload(2, 64))];
    let turn2 = vec![block(3, payload(3, 64))];
    let mut expected = Vec::new();
    expected.extend(turn1.clone());
    expected.extend(turn2.clone());

    let append = shim.ingest_batch(SID_A, vec![turn1, turn2]);
    let resp = ship(&receiver, Frame::Append(append));
    let root = expect_ack(resp);
    assert!(shim.apply_ack(SID_A, root));

    let got = receiver_blocks(&receiver, SID_A);
    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(&expected) {
        assert_eq!(g.payload, e.payload);
    }
}

#[test]
fn multi_session_isolation() {
    let (shim, receiver) = harness();
    let a_blocks = vec![block(1, payload(1, 64)), block(2, payload(2, 64))];
    let b_blocks = vec![block(1, payload(3, 64)), block(2, payload(4, 64))];

    append_turn(&shim, &receiver, SID_A, a_blocks.clone());
    append_turn(&shim, &receiver, SID_B, b_blocks.clone());

    let a = receiver_blocks(&receiver, SID_A);
    let b = receiver_blocks(&receiver, SID_B);
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 2);
    assert_ne!(a[0].payload, b[0].payload);
    // Roots differ between sessions.
    assert_ne!(
        receiver_root(&receiver, SID_A),
        receiver_root(&receiver, SID_B)
    );
}

#[test]
fn partial_bulk_resumable() {
    // Delivering only some BULK frames leaves the cold start incomplete (no
    // ACK); delivering the rest completes it and ACKs.
    let (shim, receiver) = harness();
    let blocks = many_blocks(1);
    append_turn(&shim, &receiver, SID_A, blocks.clone());

    let cold = Receiver::new(ContentStore::new(), Compressor::default());
    let resync = shim.session(SID_A).resync_frame();
    let resp = ship(&cold, Frame::Resync(resync));
    let missing = match resp {
        Some(Frame::Missing(m)) => m.missing,
        _ => unreachable!(),
    };
    let bulk = shim
        .session(SID_A)
        .bulk_frames_for(&missing, 5)
        .expect("bulk");
    assert!(bulk.len() >= 2);

    // First half: no completion ACK.
    let split = bulk.len() / 2;
    for b in bulk[..split].iter() {
        let resp = ship(&cold, Frame::Bulk(b.clone()));
        assert!(
            resp.is_none(),
            "no ACK before all generations are delivered"
        );
    }
    // Reconstruct is incomplete (or missing some blocks).
    // Second half: completion.
    let mut completion = None;
    for b in bulk[split..].iter() {
        if let Some(Frame::Ack(a)) = ship(&cold, Frame::Bulk(b.clone())) {
            completion = Some(a.root);
        }
    }
    let ack_root = completion.expect("completing the bulk ACKs");
    assert_eq!(ack_root, shim.session(SID_A).root());
    let got = cold.reconstruct(SID_A);
    assert_eq!(got.len(), blocks.len());
}

#[test]
fn receiver_root_starts_at_root_zero_for_fresh_append() {
    // A brand-new session's first APPEND (base_root == ROOT_ZERO) is accepted,
    // and the ACK root matches the shim's current root.
    let (shim, receiver) = harness();
    let blocks = vec![block(1, payload(7, 64))];
    let append = shim.ingest(SID_A, blocks.clone());
    assert_eq!(append.base_root, ROOT_ZERO);
    let resp = ship(&receiver, Frame::Append(append));
    let root = expect_ack(resp);
    assert_eq!(root, shim.session(SID_A).root());
    assert_ne!(root, ROOT_ZERO, "root advances past ROOT_ZERO");
    assert!(shim.apply_ack(SID_A, root));
}

#[test]
fn cold_start_survives_receiver_restart_with_wal() {
    // "Cold resume paid ONCE" across a receiver restart: a session rebuilt
    // via an OUT-OF-ORDER cold start is persisted to a WAL (SEED + CONTENT
    // records), and a fresh receiver on the same WAL recovers the session in
    // manifest order — same root, all content present, ready to resume steady
    // state WITHOUT a cold re-transfer. Pre-fix, the receiver rebuilt no
    // SessionState from the WAL (and the store log was arrival-ordered), so a
    // restart forced a full RESYNC+BULK.
    let wal_path = std::env::temp_dir().join("dlr-cold-start-wal-test.log");
    let _ = std::fs::remove_file(&wal_path);

    // 1. Build a shim session spanning multiple generations.
    let (shim, _receiver) = harness();
    let blocks = many_blocks(1);
    append_turn(&shim, &_receiver, SID_A, blocks.clone());
    let client_root = shim.session(SID_A).root();
    assert_eq!(shim.session(SID_A).base_root(), client_root);

    // 2. Cold-start a WAL-backed receiver, delivering BULK out of order.
    let cold_store = ContentStore::with_wal(&wal_path).expect("open wal");
    let cold = Receiver::new(cold_store, Compressor::default());
    let resync = shim.session(SID_A).resync_frame();
    let resp = ship(&cold, Frame::Resync(resync));
    let missing = match resp {
        Some(Frame::Missing(m)) => m.missing,
        other => panic!("expected Missing, got {other:?}"),
    };
    let mut bulk = shim
        .session(SID_A)
        .bulk_frames_for(&missing, 5)
        .expect("bulk encode");
    assert!(bulk.len() >= 2, "need multiple generations to reorder");
    bulk.reverse();
    let mut completion = None;
    for b in &bulk {
        if let Some(Frame::Ack(a)) = ship(&cold, Frame::Bulk(b.clone())) {
            assert!(completion.is_none(), "ACK fires once");
            completion = Some(a.root);
        }
    }
    let ack_root = completion.expect("out-of-order cold start completes");
    assert_eq!(ack_root, client_root);
    assert_eq!(receiver_root(&cold, SID_A), client_root);
    // Durably persist the cold-started session.
    cold.store().flush_wal(true).expect("flush wal");
    drop(cold);

    // 3. Simulate a receiver restart: a fresh store replays the WAL, and
    // `Receiver::new` rebuilds SessionState from the replayed store.
    let recovered_store = ContentStore::with_wal(&wal_path).expect("replay wal");
    let recovered = Receiver::new(recovered_store, Compressor::default());

    // The session is recovered in manifest order with all content present —
    // no cold re-transfer needed.
    assert_eq!(
        recovered.session_root(SID_A),
        Some(client_root),
        "recovered root == client root"
    );
    let got = recovered.reconstruct(SID_A);
    assert_eq!(got.len(), blocks.len(), "all content recovered");
    for (g, e) in got.iter().zip(&blocks) {
        assert_eq!(g.payload, e.payload, "manifest-order content matches");
    }

    // 4. Resume steady state directly on the recovered receiver: a new turn is
    // APPENDed (base_root == client_root), accepted, and ACKed with matching
    // roots — proving the restart did NOT force another cold start.
    let extra = vec![block(9001, payload(0xFE, 256))];
    let append = shim.ingest(SID_A, extra.clone());
    assert_eq!(append.base_root, client_root);
    let resp = ship(&recovered, Frame::Append(append));
    let new_root = expect_ack(resp);
    assert_eq!(new_root, shim.session(SID_A).root());
    assert!(shim.apply_ack(SID_A, new_root));
    let got = recovered.reconstruct(SID_A);
    assert_eq!(got.len(), blocks.len() + 1);
    assert_eq!(got.last().unwrap().payload, extra[0].payload);

    let _ = std::fs::remove_file(&wal_path);
}
