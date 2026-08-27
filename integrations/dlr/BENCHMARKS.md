# dlr — Theoretical Performance Model vs. gRPC

This document is the analytical argument that dlr is **≥5× faster than
gRPC** for the harness→gateway context-transfer workload, regime by regime, and
how each layer (doc §6 + the added strategies) contributes. It is a model, not a
measurement — but every factor is a structural / information-theoretic / coding-
theoretic bound, not a tuning claim.

## 0. The workload

- Claude Code speaks the **stateless Messages API**: every turn re-sends the
  full message array. At 50M tokens that is ~200 MB of JSON per turn.
- Only ~0.2 % of that array is novel per turn (the new content blocks); the
  rest is identical to the previous turn.
- The prune → ~100k tokens is heavy and must stay server-side and secret.

## 1. The structural collapse (why gRPC is already beaten before any math)

gRPC over HTTP/2 is a generic RPC transport. Used naively for this workload it
re-sends the full array every turn: **200 MB / turn**, forever. dlr
observes that the client never prunes — the array is **append-only** — so the
transport's job is *replicating an append-only log*, and an append-only log is
never re-sent, only tailed. Steady-state per-turn wire cost collapses from the
size of the history to the size of the **delta**:

```
gRPC:       W_gRPC       = |history_N|              ≈ 200 MB / turn   (grows)
dlr: W_cascade    = |delta_N|                ≈ KB–low MB / turn (constant)
```

The ratio is:

```
steady-state speedup = |history| / |delta| ≈ 200 MB / (typ. 0.1–1 MB) ≈ 200×–2000×
```

**This single structural change is already ≫ 5×.** Everything below is about
the regimes where the structural win is not enough on its own (cold start,
fan-out, fabric) and the multipliers applied on top.

## 2. Regime-by-regime model

| Regime | gRPC cost | dlr cost | Speedup | Driver |
|---|---|---|---|---|
| Steady-state / turn | 200 MB full resend | delta only, zstd-dict compressed | **~200×–2000×** | append-log replication (§3) |
| Cold resume | 200 MB **every** turn | K(1+ε) coded stream **once**, ε≈2 % | **amortized ≫ 5×** | RaptorQ (§6.4) |
| Fan-out to N agents | N × context | ~1× context (coded once) | **~N×** | RLNC (§6.5) |
| Lossy WAN recovery | per-loss retransmit storms | any-K decode, no retransmit | unbounded at high p | fountain/RLNC |
| Prune latency | on the wire (blocks transport) | off the wire (own pool) | ∞ (decoupled) | §4 decoupling |
| Cuckoo-filtered dedup | locked-map lookup/block | lock-free negative path | concurrency win | added strategy |
| Batch coalescing | 1 frame/turn | 1 frame/N turns | up to N× on framing | added strategy |

### 2.1 Steady state — ≥5× proof

- `W_gRPC / turn ≈ 200 MB` (and growing linearly with history).
- `W_cascade / turn ≈ |delta|` compressed by zstd with a trace-trained dictionary
  (SWE traces are brutally repetitive; a trained dict typically achieves 3–10×
  on the boilerplate-heavy delta).
- Lower bound on speedup: `200 MB / (1 MB / 10) = 2000×`. Even pessimistically
  (`|delta| = 4 MB`, dict only 2×): `200 / 2 = 100×`.
- **≥ 5× holds for any history ≥ ~10 MB**, i.e. for the entire regime the
  system is designed for.

### 2.2 Cold resume — ≥5× proof (amortized)

Cold resume is the *only* full transfer. gRPC pays 200 MB **every turn**; even
if a turn-based gRPC client cached, the stateless API forces the resend.

- dlr pays 200 MB **once**, as a coded stream with ε≈2 % overhead, and
  resumes mid-stream on any drop (no 200 MB restart).
- Over a session of T turns, gRPC wire = `T × 200 MB`; dlr wire =
  `200 MB × 1.02 + (T−1) × |delta|`.
- Crossover where dlr beats 5×: solve
  `T·200 ≥ 5·(204 + (T−1)·Δ)`. For `Δ = 0.5 MB`: `T·200 ≥ 1020 + 2.5·(T−1)`
  ⇒ `197.5·T ≥ 1017.5` ⇒ `T ≥ 6`. **By the 6th turn of any session,
  dlr is ≥5× faster cumulative**, and the gap widens linearly forever
  after.

**Decode cost — where dlr also wins on CPU, not just wire.** A gRPC
client deserializes the full 200 MB JSON every turn: O(history) per turn. The
dlr receiver decodes the cold-start bulk **once**:

- A naive fountain decoder runs O(K³) Gaussian elimination over the ~50 K
  source symbols of a 200 MB / 4 KB-symbol bulk transfer — prohibitive.
- The **peeling decoder** (added) reduces that to **~O(K·d̄) ≈ O(K)** for the
  systematic-and-near-systematic case (degree-1 symbols ripple through the cover
  graph), leaving only a **≈ √K residual** for Gaussian. The common "all
  systematic symbols arrived" case is exactly **O(K)**.
- The **parallel bulk transfer** (added) shards the bulk into `G` independent
  generations decoded with a rayon thread pool, so decode wall-clock is
  `max-generation`, not `sum-generation`: a further **G×**, bounded by core
  count. With G = 16 and 16 cores that is one order of magnitude on the cold
  start, on top of the wire savings.

### 2.3 Fan-out (Nightshift) — ≥5× proof

One context → N agents.
- gRPC: send the context N times ⇒ `N × |context|`.
- dlr: RLNC-code the context once; each agent reconstructs from any K
  independent combinations. Wire ≈ `|context|` (+ parity), **not** `N ×`.
- **Speedup = N**. ≥5× for any `N ≥ 5`, which is exactly Nightshift's regime.

The **hierarchical two-layer code** (added strategy) keeps that win while
decoupling slow hops: a lossy intra-group link costs only its own group's
redundancy, not the whole fan's, so the N× win survives heterogeneous loss.

## 3. Multipliers stacked on the structural win

These do not *create* the 5× — the structure does — they push the constant
further and protect the win under harder conditions.

| Layer | Mechanism | Effect on the constant |
|---|---|---|
| RaptorQ fountain (§6.4) | any-K decode, ε≈2 %, no retransmit | kills retransmit-storm RTT on cold start |
| RLNC (§6.5) | random linear combinations, capacity-optimal | multicast = 1× not N×; multipath uses all links |
| Reed-Solomon (added) | fixed-rate MDS, cheap on clean fabric | inner layer of hierarchical fan-out |
| Hierarchical (added) | outer RS + inner RLNC | decouples lossy hops in fan-out |
| Golden-ratio placement (§6.2) | lowest-discrepancy ring load | balanced at every N, no hot shard |
| Fibonacci hashing (§6.1) | φ-constant multiplicative hash | even sharding, no clustering |
| Zeckendorf framing (§6.3) | self-synchronizing Fibonacci code | re-locks after corruption, no length-prefix |
| Homomorphic hash (§6.6) | verify coded blocks without decoding | pollution-resistant RLNC on untrusted hops |
| Cayley overlay (§6.7) | vertex-transitive low-diameter gossip | O(log n) state replication across replicas |
| zstd-dict (§7) | trained on trace corpus | 3–10× on repetitive SWE deltas |
| Cuckoo filter (added) | lock-free dedup negative path | removes per-block read-lock on novel content |
| Batch coalescing (added) | N turns → 1 frame | up to N× framing/ACK amortization |
| BBR model (added) | holds BDP inflight, no sawtooth | full fabric utilization on RDMA paths |
| MMR (added) | O(log N) inclusion/range proofs | fork + provenance for distillation sink |
| Priority prune (added) | binary-heap live-session boost | live window lands first under backlog |
| Reference-delta (added) | prior block as zstd reference frame | 20–50× on near-identical file snapshots |
| Incremental prune (added) | maintained top-K window | O(log K)/block, independent of log length |
| Staged pipeline (added) | compress/code/send on bounded channels | throughput = min stage, not sum latency |
| Peeling decode (added) | degree-1 ripple + residual Gaussian | cold-start decode O(K³)→O(K); systematic case linear |
| Parallel bulk (added) | G fountain generations decoded with rayon | G× cold-start decode wall-clock |
| Parallel encode (added) | G fountain generations encoded with rayon | G× cold-start encode wall-clock |
| GF(256) full table (added) | 64 KB 256×256 multiply, branchless | 1 lookup/byte vs 2+branch; speeds every coding path |
| Sparse RLNC (added) | low-degree multicast packets + peeler | ~linear multicast decode at large N |
| Adaptive overhead (added) | ε sized from observed loss rate | pays only the wire the channel demands |

## 4. Why this is a *theoretical* bound, not a tuning claim

- The steady-state ratio is **information-theoretic**: the per-turn *novel*
  entropy is `|delta|`, not `|history|`. No transport — gRPC, QUIC, RDMA — can
  send fewer than `|delta|` novel bytes. dlr sends exactly `|delta|`
  (compressed); gRPC sends `|history|`. The ratio is bounded by the workload,
  not by implementation quality.
- The cold-start bound is **coding-theoretic**: rateless codes achieve the
  capacity of the lossy channel (ε→0 overhead); gRPC has no analogue and pays
  the full resend per turn.
- The multicast bound is the **network-coding theorem**: coding achieves
  multicast min-cut capacity that routing cannot; linear random codes attain
  it w.h.p. gRPC routing is bounded by `N×`.
- The prune bound is **architectural**: off-path async prune means transport
  latency is independent of prune latency — a divide-by-prune that gRPC cannot
  match because it couples transport to the full payload.

## 5. Bottom line

```
steady-state  : ~200×–2000×  (structural; ≫ 5×)
cold resume   : ≫ 5× by turn 6 of any session, widening linearly
fan-out       : ~N×  (≥5× for N≥5)
prune         : decoupled (∞ relative to on-wire prune)
```

**dlr is theoretically ≥5× faster than gRPC in every regime the system is
designed for, and ≫5× in steady state — the dominant regime.** The added
algorithmic strategies extend and protect these bounds (clean-fabric cold
start, lossy-hop fan-out, lock-free dedup, framing amortization, BDP-accurate
fabric pacing, provenance) without introducing the one thing the design forbids:
a re-invented L4.

## 6. Sparse reconstruction & decode-cost bounds (added)

Two more structural wins, both realized in code:

**Sparse reconstruction (§3.3 handshake, now closed).** The RESYNC → `MissingFrame`
→ `bulk_frames_for` handshake means the sender codes **only the receiver-named
gap**, not the whole tail after base. On a warm-but-diverged receiver (a network
blip that left partial state) the gap is far smaller than the tail, so cold-start
bulk cost scales with the **gap** `|G|`, not `|history|`:

```
blind-after-base : W = K(1+ε) · |tail|          (everything after base)
sparse handshake  : W = K(1+ε) · |G|,  |G| ≤ |tail|, often |G| ≪ |tail|
```

gRPC has no analogue: a stateless resend always pays the full array regardless of
what the receiver already holds.

**Decode-cost collapse (peeling + residual Gaussian).** The cold-start bulk is
~50K source symbols. A naive Gaussian-invert decoder is `O(K³)`. The implemented
`peel_decode` runs the LT peeling process (degree-1 symbols ripple through the
cover graph, each reveal reducing neighbors' degree), leaving only a `≈√K`
residual for Gaussian elimination — decode cost drops to **~`O(K · d̄ + (√K)³)`**,
i.e. roughly linear in `K` for the sparse regime. The "all-systematic-arrived"
fast path is exactly `O(K)`. gRPC pays no decode (it just re-sends), but it pays
the **200 MB re-send every turn** instead — the decode cost is one-time and now
near-linear; the resend cost is recurring and unbounded.

**GF(256) arithmetic + parallel generations.** Field ops run on byte tables
(table-lookup, branch-free); cold-start generations decode concurrently across
rayon workers, so decode wall-clock is `max-generation`, not `sum-generation`. On
a `G`-generation bulk over a `c`-core host this is a further `~min(G,c)×` on
decode latency with zero wire cost.

**Adaptive fountain overhead.** The repair margin `ε` adapts to the *observed*
loss rate (with a safety factor and a 2% peeling-failure floor) rather than a
fixed 2%, so on a clean fabric `ε → ~0` and on a lossy hop `ε` tracks the actual
erasure rate — never over- or under-provisioning the coded stream.

**Durable receiver (WAL).** The content-addressed store can shadow every
insert/reference to an append-only write-ahead log (`ContentStore::with_wal`).
On receiver restart the log is replayed, recovering session logs and Merkle
roots **without a cold-start re-transfer** — so the "cold resume paid ONCE"
bound holds *across restarts*, not just within a single process lifetime. The
append is a buffered sequential write with deferred fsync (`flush_wal`), so
the hot path stays off the fsync latency; crash-safety is tunable per flush.
gRPC has no durability story at all — a restarted gateway re-receives the full
array on the next turn regardless.

**Credit-based flow control.** A lock-free credit window (`CreditFlow`) bounds
in-flight bytes to the **BDP** (bandwidth × RTprop) for the bespoke/RDMA paths
that don't get a QUIC stream-credit window for free, and for the shim→receiver
hop where backpressure must express *application* buffers (store + async
prune), not just transport buffers. The window is re-aimed as the BBR estimate
moves. Effect on speed: prevents the latency-collapse / OOM regime where a
slow prune or a burst of large turns grows unbounded buffers — the sender is
held at the BDP instead. gRPC's per-call model has no such windowing across
calls; under burst it re-sends the full array into unbounded buffering.

### Updated bottom line

```
steady-state  : ~200×–2000×  (structural; ≫ 5×)
cold resume   : ≫ 5× by turn 6; bulk scales with |gap|, decode ~O(K)   (added)
cold resume   : paid once even across receiver restarts (durable WAL) (added)
fan-out       : ~N×  (≥5× for N≥5); sparse-RLNC decode ~O(K)            (added)
prune         : decoupled (∞ relative to on-wire prune)
```
