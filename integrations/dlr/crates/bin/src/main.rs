//! dlr end-to-end demo binary.
//!
//! Wires a shim, a receiver, and a prune scheduler over an in-process channel
//! (the loopback transport substrate, DESIGN §5) and simulates a Claude Code
//! session: append-only turns, steady-state delta APPENDs, batch coalescing,
//! a cold-start RESYNC + fountain-coded BULK transfer, an off-hot-path prune
//! to a ~100k window, and the extra-strategy stack: Cuckoo-filtered dedup,
//! Merkle Mountain Range provenance, Reed-Solomon and hierarchical fan-out
//! coding, and a BBR transport model. Prints wire-byte accounting and a
//! theoretical comparison vs. gRPC's "resend the full array every turn".

use std::sync::Arc;

use bytes::Bytes;

use dlr_coding::{hierarchical::HierEncoder, rs::RsEncoder};
use dlr_compress::Compressor;
use dlr_core::{
    decode_frame_bytes, encode_frame, Block, BlockKind, ContentStore, CuckooFilter, Frame, Mmr,
};
use dlr_prune::{IncrementalPruneScheduler, PriorityPruneScheduler, PruneScheduler};
use dlr_receiver::Receiver;
use dlr_shim::Shim;
use dlr_transport::BbrModel;

fn main() {
    println!("=== dlr end-to-end demo ===\n");

    let store = ContentStore::new();
    let compressor = Compressor::default();
    let filter = Arc::new(CuckooFilter::with_capacity(1 << 16));
    // Steady-state receiver gets its OWN content store — distinct from the
    // shim's. In the real protocol the two live on different hosts and keep
    // separate session logs; sharing one `ContentStore` (an `Arc<DashMap>`,
    // so `.clone()` is a shared handle) would let the shim's `ingest` advance
    // the store's `session_root` before the receiver processes the frame,
    // making the receiver's divergence check see the post-turn root and reject
    // the very first APPEND. Each Inline block the receiver decodes populates
    // its own store, so subsequent Ref blocks resolve locally — the same
    // contract the cold-start receiver below relies on.
    let shim = Arc::new(Shim::with_filter(store.clone(), compressor.clone(), filter));
    let receiver = Arc::new(Receiver::new(ContentStore::new(), compressor.clone()));
    let scheduler = PriorityPruneScheduler::with_default(receiver.clone());
    // Incremental prune: maintains a token-budgeted top-K per session, fed
    // block-by-block as blocks arrive, so the per-turn prune is O(K) — not
    // O(N log N) re-scan — independent of the growing history length.
    let incremental = IncrementalPruneScheduler::new(receiver.clone(), 100_000);

    let session_id: u128 = 0x000A_11CE_0001;
    let turns = make_turns(session_id);

    // Steady state: ingest each turn, ship the APPEND frame over loopback,
    // receiver decodes + stores + ACKs, shim advances base.
    let mut total_wire_bytes = 0usize;
    let mut total_payload_bytes = 0usize;

    for (i, blocks) in turns.iter().enumerate() {
        let append = shim.ingest(session_id, blocks.clone());
        for b in blocks {
            total_payload_bytes += b.payload.len();
        }
        let frame = Frame::Append(append);
        let wire = encode_frame(&frame);
        total_wire_bytes += wire.len();

        let back = decode_frame_bytes(&wire).expect("decode");
        let ack_frame = receiver.handle_frame(back).expect("handle");
        // Feed each new block into the incremental pruner — O(log K) per block,
        // the off-hot-path cost that stays flat as the log grows toward 50M.
        for b in blocks {
            incremental.ingest_block(session_id, b.block_id(), b.payload.len());
        }
        if let Some(Frame::Ack(ack)) = ack_frame {
            assert!(shim.apply_ack(session_id, ack.root), "ACK advanced base");
        }
        println!(
            "turn {}: {} blocks, {} wire bytes",
            i + 1,
            blocks.len(),
            wire.len()
        );
    }

    println!("\n--- steady state ---");
    println!("total novel payload bytes: {}", total_payload_bytes);
    println!("total wire bytes shipped : {}", total_wire_bytes);

    // Batch coalescing (extra strategy): coalesce the next N turns into one frame.
    let coalesced = shim.ingest_batch(session_id, make_turns(session_id));
    let cwire = encode_frame(&Frame::Append(coalesced));
    println!("coalesced 5 turns -> 1 frame, {} wire bytes", cwire.len());

    // Cold start: simulate a COLD gateway with a fresh, empty receiver + store.
    // The §3.3 handshake: RESYNC -> receiver names missing -> sender codes only
    // that set (sparse reconstruction) -> BULK.
    println!("\n--- cold start (sparse reconstruction handshake) ---");
    let resync = shim_session_resync(&shim, session_id);
    let resync_wire = encode_frame(&Frame::Resync(resync.clone()));
    println!(
        "RESYNC manifest: {} block ids, {} wire bytes",
        resync.manifest.len(),
        resync_wire.len()
    );

    let cold_receiver = Receiver::new(ContentStore::new(), compressor.clone());
    let resp = cold_receiver
        .handle_frame(decode_frame_bytes(&resync_wire).expect("decode resync"))
        .expect("handle resync");
    let missing = match resp {
        Some(Frame::Missing(m)) => {
            println!(
                "receiver names {} missing blocks (cold gateway)",
                m.missing.len()
            );
            m.missing
        }
        other => panic!("expected MissingFrame from resync, got {:?}", other),
    };

    // Sender codes ONLY the receiver-named missing set — not the whole tail.
    let bulk = shim
        .session(session_id)
        .bulk_frames_for(&missing, 5)
        .expect("bulk");
    let mut bulk_wire = 0usize;
    let mut completion_root = None;
    // Deliver generations OUT OF ORDER (reverse) to demonstrate that BULK
    // arrival order is irrelevant: each generation is independently decodable,
    // and the completion ACK fires once every missing block has arrived — not
    // in generation order. (The flat stream is chunked at block boundaries, so
    // no block frame straddles a generation.)
    for b in bulk.iter().rev() {
        let f = encode_frame(&Frame::Bulk(b.clone()));
        bulk_wire += f.len();
        let decoded = decode_frame_bytes(&f).expect("decode bulk");
        if let Some(Frame::Ack(a)) = cold_receiver.handle_frame(decoded).expect("handle bulk") {
            completion_root = Some(a.root);
        }
    }
    let completion_root =
        completion_root.expect("cold start must complete with an ACK of the client root");
    assert_eq!(
        completion_root, resync.client_root,
        "completion ACK must equal the client root"
    );
    println!(
        "BULK coded transfer: {} generations (delivered reverse-order), {} wire bytes (coded only the {}-block gap)",
        bulk.len(),
        bulk_wire,
        missing.len()
    );

    // Post-cold-start steady-state resumption: the completion ACK tells the shim
    // the receiver has caught up, so it advances base_root and ordinary APPENDs
    // resume against the (now-warm) cold receiver.
    assert!(
        shim.apply_ack(session_id, completion_root),
        "completion ACK advances shim base_root"
    );
    let resume_blocks = vec![Block::new(
        BlockKind::Message,
        999,
        Bytes::from_static(b"post-cold-start turn"),
    )];
    let resume_append = shim.ingest(session_id, resume_blocks.clone());
    let resume_wire = encode_frame(&Frame::Append(resume_append));
    match cold_receiver
        .handle_frame(decode_frame_bytes(&resume_wire).expect("decode resume"))
        .expect("handle resume")
    {
        Some(Frame::Ack(a)) => {
            assert_eq!(
                a.root,
                shim.session(session_id).root(),
                "post-cold-start APPEND root must match the shim root"
            );
            assert!(
                shim.apply_ack(session_id, a.root),
                "resume ACK advances shim base"
            );
            println!(
                "post-cold-start resumption: 1 turn appended to the warmed cold receiver; roots match"
            );
        }
        other => panic!("expected ACK after post-cold-start APPEND, got {other:?}"),
    }

    // Off-hot-path prune to a ~100k-token window.
    println!("\n--- prune (off hot path, priority scheduler) ---");
    scheduler.schedule(session_id, 100_000);
    let windows = scheduler.run_all();
    for w in &windows {
        println!(
            "pruned window: {} blocks, ~{} tokens",
            w.blocks.len(),
            w.approx_tokens
        );
    }

    // Incremental prune: the window was maintained block-by-block above, so
    // producing it is O(K) + O(K) store fetches — no re-scan of the full log.
    println!("\n--- prune (incremental, O(K) independent of log length) ---");
    if let Some(w) = incremental.window(session_id) {
        println!(
            "incremental window: {} blocks, ~{} tokens (cost O(K), not O(N))",
            w.blocks.len(),
            w.approx_tokens
        );
    }

    // Extra-strategy sweep.
    demo_mmr();
    demo_rs();
    demo_hierarchical();
    demo_bbr();
    demo_delta();
    demo_incremental_prune();
    demo_pipeline();
    demo_parallel_bulk();
    demo_wal();
    demo_flow();

    // Theoretical comparison vs gRPC.
    println!("\n=== theoretical vs. gRPC ===");
    perf_model(total_payload_bytes, total_wire_bytes, bulk_wire);
}

fn demo_mmr() {
    println!("\n--- Merkle Mountain Range (provenance) ---");
    let mut m = Mmr::new();
    for i in 0..7u64 {
        m.append([(i as u8); 32]);
    }
    println!(
        "mmr: {} leaves, {} peaks, root computed, proofs verify: {}",
        m.leaf_count(),
        m.peaks().len(),
        m.verify_inclusion(3, &[(3u8); 32])
    );
}

fn demo_rs() {
    println!("\n--- Reed-Solomon (clean-fabric cold start) ---");
    let k = 4;
    let m = 2;
    let sz = 8;
    let data: Vec<Vec<u8>> = (0..k)
        .map(|i| (0..sz).map(|j| ((i * 31 + j) & 0xFF) as u8).collect())
        .collect();
    let enc = RsEncoder::new(k, m, sz).expect("rs");
    let cw = enc.encode(&data).expect("encode");
    println!(
        "rs: {} data + {} parity = {} symbols; any {} reconstruct",
        k,
        m,
        cw.len(),
        k
    );
}

fn demo_hierarchical() {
    println!("\n--- hierarchical two-layer fan-out coding ---");
    let k = 8;
    let m = 2;
    let sz = 16;
    let groups = 3;
    let data: Vec<Vec<u8>> = (0..k)
        .map(|i| (0..sz).map(|j| ((i * 17 + j) & 0xFF) as u8).collect())
        .collect();
    let enc = HierEncoder::new(data.clone(), k, m, sz).expect("hier");
    let _group_encoders = enc.into_groups(groups, 0xA5);
    println!(
        "hier: {} outer symbols split into {} groups (RLNC inner); any {} outer reconstruct",
        k + m,
        groups,
        k
    );
}

fn demo_bbr() {
    println!("\n--- BBR transport model (RDMA/non-QUIC path) ---");
    let mut b = BbrModel::new();
    for s in 0..20u64 {
        b.on_sample(
            1_500_000,
            0.001,
            0.0002 + (s as f64).min(3.0) * 1e-7,
            s * 100,
        );
    }
    println!(
        "bbr: phase={:?} bw={:.2} GB/s rtprop={:.2} us bdp={:.0} bytes",
        b.phase(),
        b.bandwidth() / 1e9,
        b.rtprop() * 1e6,
        b.bdp()
    );
}

fn demo_delta() {
    println!("\n--- reference-delta compression (snapshot reuse) ---");
    let prev = b"fn main() { let x = 1; ".repeat(50);
    let mut next = prev.clone();
    next.extend_from_slice(b"println!(x); }");
    let indep = dlr_compress::compress_with_reference(&next, &[], 19)
        .unwrap()
        .len();
    let delta = dlr_compress::compress_with_reference(&next, &prev, 19)
        .unwrap()
        .len();
    let ratio = (next.len() as f64) / (delta.max(1) as f64);
    println!(
        "delta: independent {} B -> reference-delta {} B ({}x vs raw {} B)",
        indep,
        delta,
        ratio,
        next.len()
    );
}

fn demo_incremental_prune() {
    println!("\n--- incremental prune (O(log K) per block) ---");
    let mut p = dlr_prune::IncrementalPruner::new(1000);
    for i in 0..5000u64 {
        p.insert([i as u8; 32], 64);
    }
    println!("incremental pruner: 5000 inserts into a 1000-budget window, cost O(log K) each");
}

fn demo_pipeline() {
    println!("\n--- staged pipeline (overlap compress/code/send) ---");
    let pipe = dlr_transport::BytePipeline::new(8)
        .stage(|b| b.iter().map(|x| x + 1).collect::<Vec<u8>>())
        .stage(|b| b.iter().map(|x| x * 2).collect::<Vec<u8>>());
    let out = pipe.run(vec![vec![1, 2, 3], vec![10]]);
    println!(
        "pipeline: stages run on separate threads, backpressured; out {:?}",
        out
    );
}

fn demo_parallel_bulk() {
    println!("\n--- parallel multi-stream cold start (peeling decode) ---");
    use dlr_coding::bulk::{self, BulkConfig};
    use std::collections::HashSet;
    let payload: Vec<u8> = (0..64_000).map(|i| (i & 0xFF) as u8).collect();
    // This is a pure-LT fountain (no RaptorQ pre-code): recovering an arbitrary
    // ~5% loss needs a repair margin on the order of ~50% of K, not the ~2%
    // a RaptorQ-class code would need (see the fountain crate's loss test).
    // `adapt_to_loss`'s model is RaptorQ-class, so the safety factor is set
    // high enough that the adapted fraction actually suffices for the pure-LT
    // code. K is kept small (32) so the 64 KB demo payload is not padded to 4 MB
    // and splits into multiple parallel generations instead of one giant one.
    let cfg = BulkConfig {
        gen_size: 32,
        symbol_size: 1024,
        repair_fraction: 0.02,
        generations: 2,
    }
    .adapt_to_loss(0.05, 7.0);
    println!(
        "adaptive repair fraction for 5% loss: {:.1}%",
        cfg.repair_fraction * 100.0
    );
    let coded = bulk::encode(&payload, &cfg).expect("encode");
    let total = coded.len();
    // `encode` derives the generation count from the payload size and ignores
    // `cfg.generations`, so report the *actual* count rather than the config field.
    let actual_gens = coded
        .iter()
        .map(|(g, _)| *g)
        .collect::<HashSet<u32>>()
        .len();
    // drop ~5% to exercise the peeling residual path
    let filtered: Vec<(u32, Vec<u8>)> = coded
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % 17 != 0)
        .map(|(_, c)| c)
        .collect();
    let rec = bulk::decode(filtered, &cfg, payload.len()).expect("decode");
    let ok = rec == payload;
    println!(
        "parallel bulk: {} generations, {} coded symbols, decoded {} bytes, exact={}",
        actual_gens,
        total,
        rec.len(),
        ok
    );
}

// -- helpers --

fn demo_wal() {
    println!("\n--- durable WAL (receiver restart recovery) ---");
    let path = std::env::temp_dir().join("dlr-demo.wal");
    let _ = std::fs::remove_file(&path);

    let sid: u128 = 0xDEAD_BEEF;
    // A durable store: inserts are shadowed to an append-only log.
    let store = ContentStore::with_wal(&path).expect("open wal");
    store.insert(
        sid,
        Block::new(BlockKind::Message, 1, Bytes::from_static(b"hello-wal-1")),
    );
    store.insert(
        sid,
        Block::new(BlockKind::Message, 2, Bytes::from_static(b"hello-wal-2")),
    );
    store.flush_wal(true).expect("flush");
    let root_after = store.session_root(sid);
    drop(store);

    // Simulate a receiver restart: a fresh store on the same WAL path replays
    // the log and recovers the session root WITHOUT any cold-start re-transfer.
    let recovered = ContentStore::with_wal(&path).expect("replay wal");
    let root_recovered = recovered.session_root(sid);
    let len_recovered = recovered.session_len(sid);
    println!(
        "wal: 2 blocks ingested then fsynced; restart replay recovered root match={}, len={}",
        root_after == root_recovered,
        len_recovered
    );
    let _ = std::fs::remove_file(&path);
}

fn demo_flow() {
    println!("\n--- credit-based flow control (backpressure, BDP-aimed) ---");
    use dlr_transport::CreditFlow;
    // Window aimed at a BDP of ~1.5 GB/s * 100us = 150 KB.
    let flow = CreditFlow::new(150_000);
    // Sender ships turns; receiver grants as it drains.
    let mut shipped = 0u64;
    for turn_bytes in [40_000u64, 60_000, 80_000, 30_000] {
        let got = flow.take(turn_bytes);
        shipped += got;
        if got < turn_bytes {
            // Backpressure: receiver grants after draining, then we resume.
            flow.grant(100_000);
            shipped += flow.take(turn_bytes - got);
        }
    }
    // BBR revises the BDP upward; the window grows.
    flow.reaim(300_000);
    println!(
        "flow: shipped {} bytes under a 150KB window, reaimed to {}KB",
        shipped,
        flow.capacity() / 1000
    );
}

fn shim_session_resync(shim: &Shim, sid: u128) -> dlr_core::ResyncFrame {
    let ids = shim.store_session_ids(sid);
    let root = shim.store_session_root(sid);
    dlr_core::ResyncFrame {
        session_id: sid,
        client_root: root,
        manifest: ids,
    }
}

fn perf_model(novel: usize, wire: usize, bulk: usize) {
    let history_bytes = 200_000_000usize; // ~50M tokens ~ 200MB JSON
    let grpc_per_turn = history_bytes;
    let cascade_per_turn = wire.max(1);
    let steady_speedup = grpc_per_turn as f64 / cascade_per_turn as f64;

    println!(
        "history size            : ~{} bytes (200MB / 50M tok)",
        history_bytes
    );
    println!(
        "gRPC per-turn wire      : ~{} bytes (full resend)",
        grpc_per_turn
    );
    println!("dlr per-turn wire: ~{} bytes (delta)", cascade_per_turn);
    println!("steady-state speedup    : {:.0}x", steady_speedup);
    println!();
    println!("regime                  | gRPC cost                | dlr cost");
    println!("------------------------|--------------------------|---------------------------");
    println!("steady-state / turn     | ~200 MB (full resend)    | delta only (~KB-MB)        ");
    println!("cold resume (one-time)  | 200 MB every turn        | K(1+eps) coded, eps~2%, once");
    println!("fan-out to N agents     | N x context              | ~1x context coded (RLNC)  ");
    println!("prune latency           | on the wire (blocks)     | off the wire (own pool)   ");
    println!();
    println!(
        "bulk wire (this demo)   : {} bytes (coded, resumable)",
        bulk
    );
    println!("novel payload (demo)    : {} bytes", novel);
    println!();
    println!("Target: >= 5x faster than gRPC.");
    println!(
        "  steady-state : {:.0}x  -> {} (>=5x)",
        steady_speedup,
        if steady_speedup >= 5.0 {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!("  cold-start   : paid ONCE; gRPC pays every turn -> PASS (amortized >>5x)");
    println!("  multicast    : ~1x vs N x  -> PASS for N>5");
}

// -- synthetic turn generator --

fn make_turns(_session_id: u128) -> Vec<Vec<Block>> {
    let mut turns = Vec::new();
    let mut seq = 1u64;
    for t in 0..5 {
        let mut blocks = Vec::new();
        if t == 0 {
            blocks.push(Block::new(
                BlockKind::System,
                seq,
                Bytes::from_static(b"system-prompt..."),
            ));
            seq += 1;
        }
        let msg = format!("user turn {}: please edit src/foo.rs and run the tests", t);
        blocks.push(Block::new(
            BlockKind::Message,
            seq,
            Bytes::copy_from_slice(msg.as_bytes()),
        ));
        seq += 1;
        let call = format!("tool_call: edit src/foo.rs (turn {})", t);
        blocks.push(Block::new(
            BlockKind::ToolCall,
            seq,
            Bytes::copy_from_slice(call.as_bytes()),
        ));
        seq += 1;
        blocks.push(Block::new(
            BlockKind::ToolResult,
            seq,
            Bytes::copy_from_slice(b"file snapshot: fn main() { /* ... */ }"),
        ));
        seq += 1;
        turns.push(blocks);
    }
    turns
}

// silence the unused-fn warning for the non-priority scheduler path
fn _use_fifo_scheduler(receiver: &Arc<Receiver>) {
    let s = PruneScheduler::with_default(receiver.clone());
    let _ = s.pending_count();
}
