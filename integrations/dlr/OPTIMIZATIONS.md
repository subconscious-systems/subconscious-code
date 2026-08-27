# dlr — 20 Optimization Plans

Analysis performed 2026-08-13 by reading every source file in the workspace (~6k LOC, 8 crates)
and verifying against a building baseline.

## Critical context (read first)

> **Update (2026-08-23):** the baseline issues called out below have been
> resolved. The workspace now compiles clean (`cargo build`) and **all tests
> pass** (64 tests across 8 crates, including 11 protocol-lifecycle tests in
> `dlr-receiver`). The `block_id_from_canonical` re-export problem and the
> failing tests (including the `fib_hash64` top-bits collision bug) are fixed.
> Treat the per-plan adoption status in this document as **stale** — several
> plans have since been implemented; verify against the code, not this doc.

Analysis performed 2026-08-13 by reading every source file in the workspace (~6k LOC, 8 crates)
and verifying against a building baseline.

Original findings (historical, since fixed):

- **The workspace did not compile at the time.** `crates/core/src/lib.rs:22`
  re-exported `block::Block::block_id_from_canonical`, but
  `block_id_from_canonical` is an inherent associated function and that path is
  not a valid `pub use` target ("`Block` is a struct, not a module"). Fixed by
  converting it to a free function in `block.rs` (matching
  `canonical_bytes`/`from_canonical`) and re-exporting
  `block::block_id_from_canonical`.
- **6 tests failed on `main` at the time**, independent of the fix:
  `bulk::parallel_bulk_roundtrip`,
  `fountain::systematic_plus_repairs_decode`,
  `fountain::peeling_handles_heavy_loss_with_repairs`,
  `rlnc::sparse_roundtrip_peels`, `fib_hash::distributes_consecutive_keys`,
  `placement::balanced_at_every_n`. Several were real correctness bugs (e.g.
  `fib_hash64` hashed the *top* bits of `hi*GOLDEN ^ lo`, which are zero for
  keys < 2^56, so all small keys collided in shard 0). Now fixed.
- **Dead dependencies:** `serde`, `serde_json`, `crc32fast`, `dashmap`, and
  `rand` were declared but referenced nowhere in `crates/*/src`. (Some, e.g.
  `dashmap`, are now used — verify against the current `Cargo.toml` files.)

Plans are ordered roughly hot-path-first. Effort: **S** small, **M** medium,
**L** large.

---

## 1. Back `ContentStore` blocks with `DashMap` for concurrent inserts
**Location:** `crates/core/src/store.rs:45-65, 112-172`
**Problem:** A single `Arc<RwLock<StoreInner>>` guards the entire store. Every
`insert_with_id` / `reference` / `get` / `contains` takes a write (or read) lock
on one structure, so inserts from different sessions — the common multi-session
case — serialize behind each other's hash insert and root update.
**Proposal:** Split `StoreInner` into (a) a `DashMap<BlockId, Block>` for the
content map (lock-free-ish concurrent reads/writes) and (b) a separate
`DashMap<u128, SessionLog>` (or sharded `RwLock`) for per-session logs. The hot
`contains`/`get` dedup path becomes lock-free; cross-session inserts no longer
contend. `dashmap = "6"` is already a workspace dependency but unused.
**Impact:** Removes the single biggest contention point on the append path
under concurrency; near-linear scaling with sessions.
**Effort:** M-L
**Steps:**
1. Replace `blocks: AHashMap<...>` with `DashMap<BlockId, Block>`, keep the
   `Entry`-based insert (DashMap has `entry`).
2. Move `SessionLog` to its own `DashMap<u128, SessionLog>`; protect each
   `SessionLog` (ids/root) with its own `RwLock` so appends to different
   sessions are independent.
3. Update `stats` to per-counter atomics or a sharded aggregate.
4. Re-port `reconstruct`/`reconstruct_range`/`session_ids` to read without the
   global lock.
**Tests:** Existing store tests + a new multi-thread insert stress test
asserting no races and correct roots under `loom` or heavy `rayon` parallelism.

## 2. Collapse the double session-map lookup in `insert_with_id`
**Location:** `crates/core/src/store.rs:145-153`
**Problem:** `was_new_session = !g.sessions.contains_key(&session_id)` does one
hash lookup, then `g.sessions.entry(session_id).or_insert_with(...)` does a
second. Two probes per insert for the same key.
**Proposal:** Use the `entry` API once: `match g.sessions.entry(session_id) {
Entry::Vacant(v) => { stats.sessions += 1; v.insert(SessionLog{...}) }, Entry::Occupied(o) => o.into_mut() }`.
**Impact:** Removes one hashmap probe per insert; tiny but pure hot-path.
**Effort:** S
**Steps:** Rewrite the session-creation block in `insert_with_id` and mirror in
`reference`. Verify stats still match in tests.

## 3. CuckooFilter: grow instead of permanently latching `victim`
**Location:** `crates/core/src/filter.rs:91-113, 117-130`
**Problem:** On `KICKS` eviction failure the filter sets `victim = 1` (Release)
and **never clears it**. After that one saturation event, `contains` returns
`true` for *everything* forever (line 118-121) — the filter becomes a permanent
no-op, forcing every dedup back through the locked store for the rest of the
process lifetime.
**Proposal:** (a) Add a `grow()` that doubles `buckets` and rehashes all
fingerprints on saturation; (b) if growth is disabled, set `victim` but expose
a `reset()`/`clear()` so the caller can rebuild the cache from the authoritative
store. (c) Track a `saturated` flag separately from a transient `victim` so
`contains` can fall back without the structure being wedged.
**Impact:** Correctness/longevity of the dedup fast path under sustained load.
**Effort:** S-M
**Steps:** Add `grow`/`reset`; have `insert` call `grow` once on failure before
giving up; clear `victim` on successful grow. Test that load beyond capacity
recovers rather than permanently degrading.

## 4. CuckooFilter: cheaper `alt_index` mix
**Location:** `crates/core/src/filter.rs:79-86`
**Problem:** `alt_index` calls `fp_hash` → `xxh3_64(&fp.to_le_bytes())` on every
insert/contains — a full xxh3 over 2 bytes, called twice per `contains` and
once per eviction kick. For a structure whose whole point is "cheaper than the
locked map", this is disproportionate.
**Proposal:** Replace with a cheap integer mix, e.g.
`(fp as u64).wrapping_mul(GOLDEN64)` (the same constant already used elsewhere),
masked to the table. A 16-bit fp only needs a 64-bit multiply for good avalanche
in a power-of-two table.
**Impact:** Cuts the per-query cost of the dedup fast path meaningfully on the
novel-block negative path.
**Effort:** S
**Steps:** Swap `fp_hash`; keep the `i ^ alt` derivation; re-run the existing
false-positive-rate test to confirm FPR stays < 50/1000.

## 5. Combine canonical materialization and block-id into one pass
**Location:** `crates/core/src/canonical.rs:14-21`, `crates/shim/src/lib.rs:106-107`
**Problem:** `ingest_turn` does `let c = canonical_bytes(&b)` (alloc + full
payload copy) then `block_id_from_canonical(&c)` (a *second* full pass over `c`
to BLAKE3-hash it). The canonical bytes are streamed twice.
**Proposal:** Add `canonical_and_id(block: &Block) -> (Vec<u8>, BlockId)` (or a
`canonical_into_with_id` that hashes via `h.update` *while* writing the Vec),
so the id is computed in the same pass that builds `c`. The shim gets both `c`
(for compression) and `id` from one traversal.
**Impact:** Removes one full pass over every inline block's payload on the
per-turn hot path (each block traversed once instead of twice).
**Effort:** S-M
**Steps:** Implement in `canonical.rs`; use in `SessionShim::ingest_turn`;
keep `block_id_from_canonical` for callers (receiver) that already have `c`.
Add a test asserting the id equals `Block::block_id()`.

## 6. WAL: write the record header directly, drop per-append `Vec` alloc
**Location:** `crates/core/src/wal.rs:55-75`
**Problem:** `append_insert`/`append_reference` allocate a temp
`Vec::with_capacity(HEADER + len)`, copy header + payload in, then `write_all`.
Every durable insert allocates and frees a payload-sized buffer.
**Proposal:** Write the header fields directly to the `BufWriter`: four
`write_all` calls (`session_id.to_le_bytes()`, `[variant]`, `len.to_le_bytes()`,
`canonical`). `BufWriter` already batches, so this is one logical record with no
intermediate allocation.
**Impact:** Eliminates a per-durable-append allocation on the receiver's
durability path.
**Effort:** S
**Steps:** Rewrite both `append_*` to write straight to the locked writer; keep
the on-disk format byte-identical; the `replay` path is unchanged. Verify the
WAL demo + restart-recovery test still round-trips.

## 7. GF(256) SIMD `pshufb` multiply for `axpy`/`scal`
**Location:** `crates/coding/src/gf256.rs:78-99`
**Problem:** The vector kernels are `for (d,&s) in ... { *d ^= row[s as usize] }`
— a gather + table-lookup + XOR that does **not** auto-vectorize. For the 4 KB
cold-start symbols this byte-at-a-time loop is the CPU ceiling of the whole
fountain/RLNC/Bulk path.
**Proposal:** Implement the standard ISA-L `pshufb` GF-multiply: a 16-byte
operand is split into low/high nibbles, each used to index a 16-entry `pshufb`
lookup that reconstructs `a*x` for all 16 bytes in one SIMD op, then XOR into
the accumulator. Provide x86 SSE2/AVX2 and aarch64 NEON paths behind
`#[cfg(target_arch)]` with a scalar fallback; gate `axpy`/`scal`/the gauss
inner loop on it.
**Impact:** The single largest cold-start CPU win — typically 5-10× on the
GF-multiply kernels, which dominate encode/decode.
**Effort:** L
**Steps:** (1) Add `gf_mul_slice_simd`; (2) rewrite `axpy`/`scal` to use it
when lengths are aligned (tail-loop scalar for the remainder); (3) use it in
`gf_gauss_eliminate`'s inner `rr[j] ^= frow[prow[j]]` loop via two pshufb
lookups; (4) keep the existing table-based path for non-x86/aarch64; (5)
property-test against the scalar `gf_mul` for correctness across all 256
multipliers.

## 8. Flat-buffer matrix for `gf_gauss_eliminate`
**Location:** `crates/coding/src/gf256.rs:160-202`, callers in `rlnc.rs`,
`rs.rs`, `fountain.rs`
**Problem:** The matrix is `Vec<Vec<u8>>` — rows scattered across the heap.
The elimination inner loop `rr[j] ^= frow[prow[j]]` chases two pointers per
byte with poor locality, and the `mem::take`/restore dance (lines 185-197) to
avoid cloning the pivot row is only needed because rows are individually owned.
**Proposal:** Store the augmented matrix as one flat `Vec<u8>` of length
`n * stride`, operate on `&mut [u8]` row slices. Pivot "swap" is a row-index
swap; the pivot row is read by index without take/restore. Improves cache
locality and composes with #7 (contiguous SIMD-friendly strides).
**Impact:** Better decode throughput at large K; prerequisite for the SIMD
inner loop being efficient.
**Effort:** M-L
**Steps:** Change `gf_gauss_eliminate` signature to accept `&mut [u8]` + `n` +
`stride` (keep a `Vec<Vec<u8>>`-adapter for callers). Update the three callers.
Benchmark fountain decode at K=1024.

## 9. RLNC decoder: maintain a pivot→row map
**Location:** `crates/coding/src/rlnc.rs:154-214`
**Problem:** `add` calls `first_nonzero(&e[..k])` (an O(K) linear scan) for
*every* existing pivot row, on *every* add — O(K²) scans per packet just to
locate pivots. `decode` re-scans all rows for pivots again.
**Proposal:** Keep `pivots: Vec<Option<usize>>` (col → owning row index) and a
`col_of_row: Vec<Option<usize>>` updated during elimination. Elimination and
the final extract then index directly; no `first_nonzero` scans.
**Impact:** Reduces per-packet decode cost from O(K²·S) probes to O(rank·S)
with no scans; matters for large multicast generations.
**Effort:** S-M
**Steps:** Add the maps, update them in `add`'s reduction loops and in `decode`;
replace `first_nonzero` lookups with map reads; keep the roundtrip tests green
(fix the existing `sparse_roundtrip_peels` failure first — see context).

## 10. Fountain peeler: `mem::take` payload + O(1) cover removal
**Location:** `crates/coding/src/fountain.rs:308-357`
**Problem:** (a) On every peel, `let mut val = syms[si].payload.clone()` clones
the full symbol (up to 4 KB) — peeling K sources is O(K·S) of cloning. (b)
`syms[sj].cover.iter().position(|(idx,_)| *idx == target)` is an O(degree) scan
per substitution, making the ripple O(K·d̄²).
**Proposal:** (a) `mem::take` the revealing symbol's payload (it's never used
again after revealing) instead of cloning. (b) Maintain, per symbol, a
`HashMap<usize, u8>` (cover index → coefficient) so target removal is O(1); or
store cover as a small `Vec` plus a per-source reverse index built once at the
start of peeling.
**Impact:** Removes K·S bytes of cloning per decode + cuts the ripple's scan
cost; fixes the decode-throughput bottleneck for the cold-start path. Likely
also resolves the flaky `peeling_handles_heavy_loss_with_repairs` /
`systematic_plus_repairs_decode` failures if they stem from the clone/take
interaction.
**Effort:** M
**Steps:** Switch to `mem::take`; add the cover index; keep `degree` accounting;
re-run the fountain + bulk tests (expect to fix the two decode failures).

## 11. Reed-Solomon: cache the generator matrix per (k, m)
**Location:** `crates/coding/src/rs.rs:36-63, 139-158`
**Problem:** `decode(...)` calls `generator(k, m)` on *every* invocation —
building a Vandermonde matrix and inverting a k×k matrix (Gauss-Jordan, O(k³))
per decode. The generator depends only on (k, m), not the data.
**Proposal:** Memoize the parity-rows of `generator(k, m)` in a
`OnceLock<HashMap<(usize,usize), Vec<Vec<u8>>>>` (or an `RsEncoder`-style
precomputed struct reused by `decode`). For the fixed (k,m) of a session this is
built once.
**Impact:** Turns an O(k³) per-decode cost into O(1) lookup; significant for
repeated RS decodes (hierarchical fan-out, fabric cold-start).
**Effort:** S-M
**Steps:** Extract `generator` into a cached function; thread the cached
parity matrix through `decode`; keep `RsEncoder` storing its `g`. Add a test
that `decode` with cached vs fresh gives identical output.

## 12. Homomorphic hash: precompute per-coordinate power tables
**Location:** `crates/coding/src/homohash.rs:124-158`
**Problem:** `hash`/`combine_many` call `powmod(gens[i], e, P)` per nonzero
coefficient — O(K·log Q) modular exponentiations per hash. `powmod` is a
bit-by-bit loop of `mulmod`s.
**Proposal:** Precompute `pow_table[i][0..Q] = g_i^e mod P` for each coordinate
(Q = 1321). Then `hash` is `∏ pow_table[i][c_i]` — O(K) `mulmod`s, no
exponentiation. Memory: K·1321·16 bytes; for K ≤ ~256 that's ~5 MB, fine for a
per-generation structure.
**Impact:** ~log2(Q) ≈ 11× fewer mulmod ops per hash on the verification path.
**Effort:** M
**Steps:** Build the table in `HomomorphicHash::new`; rewrite `hash` and
`combine_many` (the latter needs source unit-hash powers, derivable from the
table). Keep all existing homohash tests exact. Note this module isn't wired to
the GF(2⁸) RLNC yet (scope note in the file) — lower priority unless that
integration lands.

## 13. `bulk::encode`: zero-copy source symbols (no full payload copy)
**Location:** `crates/coding/src/bulk.rs:101-123`
**Problem:** `encode` does `let mut padded = payload.to_vec(); padded.resize(...)`
— copying the entire payload (up to ~200 MB for cold start) just to zero-pad the
tail, then copies *again* into per-symbol Vecs (`padded[off..].to_vec()`). The
200 MB cold start is materialized ~twice extra.
**Proposal:** Keep the input as `Arc<[u8]>` (or accept `Bytes`); compute the
pad length and pass each generation a `Vec<&[u8]>` of slices into the shared
buffer plus a single owned zero-filled tail symbol for the partial last
generation. `FountainEncoder::new` already takes owned `Vec<Vec<u8>>` — add a
`from_slices` constructor or have it own an `Arc<[u8]>` + ranges. The rayon
tasks share the `Arc`, no per-symbol cloning.
**Impact:** Cuts cold-start encode peak memory from ~3× payload to ~1× and
removes the per-symbol copy.
**Effort:** M
**Steps:** Change `encode` to share an `Arc<[u8]>`; add a slice-based
`FountainEncoder` path; verify `parallel_bulk_roundtrip` (after fixing its
decode failure).

## 14. Compressor: reuse the zstd `CDict`/`DDict` across blocks
**Location:** `crates/compress/src/lib.rs:80-135`
**Problem:** `compress`/`decompress` construct a **fresh**
`zstd::bulk::Compressor::with_dictionary(...)` / `Decompressor::with_dictionary`
on *every call* (every block). Building a zstd dictionary context per block is
expensive — the dict is re-parsed on the per-turn hot path. The doc comment
claims "rebuild per thread" but it's actually per-block.
**Proposal:** Build the `CDict`/`DDict` once in `Compressor::new` and store them
(thread-local or behind the existing `Arc<Mutex<Compressor>>`). Use
`zstd::stream`/`bulk` APIs that accept a pre-built dictionary object. For the
no-dict path, keep a long-lived `Compressor`/`Decompressor` and `reset` it
between blocks instead of reconstructing.
**Impact:** Removes the dominant per-block setup cost from the compress/decompress
hot path — large win for the append path where every inline block is compressed
and every received block is decompressed.
**Effort:** M
**Steps:** Investigate `zstd` 0.13's `CDict`/`DDict` Send/Sync story; store the
dict context in `Compressor`; `reset` encoders per block; keep the marker
(0x00/0x01) framing. Round-trip + passthrough tests must stay green.

## 15. Compression level tiering (fast append vs. cold-start)
**Location:** `crates/compress/src/lib.rs:72` (`Compressor::default` → level 19),
`crates/shim/src/lib.rs:131` (per-block), `bulk_frames_for` (cold-start)
**Problem:** Level 19 is zstd's slowest setting. Using it for every small
per-turn append block is wasteful — on small blocks level 19 is much slower than
level 3-9 with near-identical ratio. Cold-start bulk (large, repetitive) is
where level 19 pays off.
**Proposal:** Tier by path: a fast `Compressor` (level ~3-6) for the steady-state
append hot path, and the high-level one for cold-start bulk. Make the shim's
per-turn compressor configurable; default the append path to a fast level.
**Impact:** Substantial append-path CPU reduction (zstd level 19 → 3 is
typically 5-15× faster on small inputs) for negligible ratio loss on small blocks.
**Effort:** S-M
**Steps:** Add a level param to the shim's per-session compressor; default
append to fast; keep cold-start `bulk_frames_for` at high level. Benchmark
per-turn latency.

## 16. Reference-delta: prebuilt dict/compressor reuse
**Location:** `crates/compress/src/delta.rs:24-51`
**Problem:** `compress_with_reference`/`decompress_with_reference` call
`Dict::from_content(0xDE1A, reference.to_vec())` on *every* call — cloning the
entire reference block and building a fresh `Compressor` per delta. Delta
compression's whole use case is many blocks against the *same* prior version.
**Proposal:** Add entry points that accept a prebuilt `Dict` (or a
`DeltaCompressor` holding the reference dict + a long-lived zstd context, per
#14). The caller builds it once per reference and reuses it across the turn's
blocks.
**Impact:** Removes a per-block reference clone + zstd-context rebuild from the
snapshot-heavy delta path.
**Effort:** S-M
**Steps:** Add `DeltaCompressor::new(reference)` returning a reusable struct
with `compress`/`decompress`; keep the one-shot functions as thin wrappers for
ergonomics/tests.

## 17. Shim `bulk_frames_for`: reuse the parallel `dlr_coding::bulk::encode`
**Location:** `crates/shim/src/lib.rs:221-275`
**Problem:** The shim reimplements cold-start bulk coding with a serial
per-generation loop that re-copies each symbol (`flat[i*sym..].to_vec()`) and
calls `FountainEncoder` serially. `dlr_coding::bulk::encode` already does this
— in parallel across the rayon pool, with adaptive repair sizing, and (after
#13) zero-copy.
**Proposal:** Build the flat `[len:u32][compressed]` stream exactly as today,
then delegate to `bulk::encode` / `bulk::decode` for the fountain coding,
mapping its `Vec<(gen_id, wire)>` back into `BulkFrame`s. Delete the shim's
serial generation loop.
**Impact:** Cold-start bulk encode becomes parallel (max-generation, not
sum-generation) for free; removes duplicated fountain wiring; inherits the
adaptive repair-fraction logic.
**Effort:** M
**Steps:** Reuse `BulkConfig` (set `gen_size`/`symbol_size` from the shim's
fountain params); convert `bulk::encode` output to `BulkFrame`s; ensure the
receiver's `handle_bulk` framing (`[len:u32][compressed]`) still round-trips.
Update the demo's cold-start path.

## 18. Receiver `handle_bulk`: move fountain decode out of the sessions lock
**Location:** `crates/receiver/src/lib.rs:169-211`
**Problem:** `handle_bulk` holds the `sessions` `Mutex` across `dec.add` *and*
`dec.decode()` — the full Gaussian/peeling decode. This is exactly the
anti-pattern `handle_append` (lines 80-109) was refactored to avoid: CPU work
under a global lock serializes every session's bulk decode behind each other.
**Proposal:** Take the lock only to fetch/create the per-generation decoder
handle and to register the new symbols; *release* it, run `dec.decode()` on a
local (or per-generation) decoder, then take the lock again to store recovered
blocks and remove the generation entry. If the decoder must persist across
frames, store it per-generation under a finer-grained lock (per-session or
per-generation) rather than the global `sessions` mutex.
**Impact:** Decouples cold-start decode parallelism across sessions/generations;
stops bulk decode from blocking APPEND handling.
**Effort:** S-M
**Steps:** Split `handle_bulk` into a locked "ingest symbols" phase and an
unlocked "decode + store" phase; keep `decoders` correctness under concurrency
(per-generation mutex or take-then-restore). Test the cold-start demo + a
concurrent append/bulk test.

## 19. Prune: drive `ImportancePolicy` from `IncrementalPruner` (O(log K) vs O(N log N))
**Location:** `crates/prune/src/lib.rs:77-122`, `crates/prune/src/incremental.rs`
**Problem:** `ImportancePolicy::prune` scores *every* block (`par_iter`), allocates
a `Vec<(usize, f32)>` of size N, and `sort_unstable_by` (O(N log N)) — *every
prune*, on a log that grows to 50M tokens. The `IncrementalPruner` (O(log K)
per insert, maintained top-K) exists but isn't used by the default policy.
**Proposal:** Maintain an `IncrementalPruner`-backed index (or a scored
ring-buffer) updated on each append; `prune` reads the maintained top-K window
instead of re-scanning N. Make the score function pluggable so the real
"accurate" runtime scorer can plug in behind the same maintenance (the
incremental module already says this is the seam).
**Impact:** Prune cost becomes independent of N — critical as the log grows;
the O(N log N) re-scan is the prune path's scaling cliff.
**Effort:** M-L
**Steps:** Wire `IncrementalPruner` into `PruneScheduler`/`Receiver` append
path; have `ImportancePolicy::prune` consume the maintained window; keep the
pluggable scorer. Add a scaling test (N=100k blocks) showing prune time flat.

## 20. Workspace: remove unused dependencies
**Location:** `Cargo.toml` (workspace `[dependencies]`),
`crates/core/Cargo.toml`, `crates/coding/Cargo.toml`
**Problem:** Grep across `crates/*/src` finds **zero** references to `serde`,
`serde_json`, `crc32fast`, `rand`, or `dashmap`. `bytes` is built with
`features = ["serde"]` but nothing uses it. These compile for nothing,
lengthening build times and the dependency graph. (`dashmap` becomes used if
#1 lands — gate accordingly.)
**Proposal:** Remove `serde`, `serde_json`, `crc32fast`, `rand` from the
workspace and per-crate manifests; drop the `serde` feature from the `bytes`
workspace dep; keep `dashmap` only if #1 is adopted (else remove it). Run
`cargo machete`/`cargo udeps` to confirm.
**Impact:** Faster cold builds, smaller lockfile, less surface area.
**Effort:** S
**Steps:** Delete the deps; `cargo build --workspace` + `cargo test --workspace`
must stay green (modulo the pre-existing failures noted above). Re-add `dashmap`
if/when #1 lands.

---

## Suggested sequencing

1. **Correctness first:** fix the 6 failing tests (fountain/bulk/rlnc decode +
   `fib_hash64`/`placement` collisions) — Plans #10 and #1's hash path overlap
   here.
2. **Cheap hot-path wins:** #2, #4, #5, #6, #15, #20 — small, isolated, high
   ROI.
3. **Medium structural:** #1, #14, #16, #18, #11, #13 — biggest per-turn and
   cold-start wins.
4. **Large, last:** #7 (SIMD GF) and #8 (flat matrix) — the cold-start CPU
   ceiling, gated on benchmarks.

---

# Part II — 20 more optimization plans

Analysis performed 2026-08-13 by re-reading every source file in the workspace
after the first 20 plans were drafted, then re-audited 2026-08-24 against the
current code. **Adoption status (audited against code, not the doc):**

- **Implemented (36 of 40):** #1–#18, #20, #22–#35, #37–#39 — including the
  large ones the original sequencing gated on benchmarks: #7 (GF(256) SIMD
  `pshufb` kernels, x86 SSE2/AVX2 + aarch64 NEON + scalar fallback), #8 (flat
  matrix `gf_gauss_eliminate`), #30 (arena-linked persistent MMR), #34
  (parallel pre-sized bulk reassembly), #39 (Arc-shared hierarchical groups).
- **#19 — realized via the incremental path:** the N-independent prune is
  `IncrementalPruneScheduler` (O(log K)/block, O(K)/window), implemented and
  covered by `incremental_window_size_is_independent_of_log_length` (1k vs
  100k blocks, same survivor count under a fixed budget). `ImportancePolicy`
  remains as a *stateless one-shot baseline* for snapshot pruning; it is not
  the production path. The faithful "real" scorer plugs in at
  `IncrementalPruner::new_with`.
- **#21 — substantially done:** the query path takes one shared `RwLock` read
  (concurrent readers never block each other) with an atomic mask; the
  original 3× global `Mutex` complaint is fixed. The plan's *ideal* lock-free
  `Arc<Vec>` buckets is deferred — it needs hand-rolled unsafe `AtomicPtr`
  arc-swap or a new `arc-swap` dependency for a marginal gain over an already
  cheap shared read.
- **#28 — implemented (this PR):** the receiver's duplicate per-session id list
  (`SessionState.ids`/`root`) is removed. After the cold-start `seed_session`
  fix the store's session log is the single manifest-ordered source of truth
  for ids and root, so `reconstruct`/`pointer`/`session_root` read the store
  directly.
- **#36 — deferred:** the passthrough marker (0x00/0x01) is a load-bearing
  wire-format contract embedded in the compressed payload. Removing the
  full-input copy needs the marker moved out-of-band into the frame's per-block
  length prefix — a cross-cutting change to compress, the frame codec, shim,
  receiver, and bulk framing — for a benefit limited to incompressible blocks
  (rare in text-heavy SWE traces). Not worth the wire-format risk for this PR.
- **#40 — intentionally not done:** the lock-free `store.session_root()`
  divergence pre-check was *removed* for correctness. It used the store's
  insertion-order root as a proxy for the authoritative root, which diverges
  after an *out-of-order* cold start and spuriously rejected the first
  post-cold-start APPEND (livelock). The authoritative in-lock
  `SessionState`→store check is the oracle; divergent APPENDs (rare, only on
  the cold-start transition) pay the resolve before being rejected — an
  acceptable trade for correctness.

The plans below are the original text (verified against the *current* code).
Ordered hot-path-first; effort S/M/L.

## 21. CuckooFilter: remove the global `Mutex` from the query path
**Location:** `crates/core/src/filter.rs:93-99, 101-107, 121, 161-175`
**Problem:** `contains` acquires the table-wide `parking_lot::Mutex` **three
times per query** — once in `indices` to read `mask` (line 97), once in
`alt_index` to read `mask` again (line 106), once to index `buckets` (line 168);
`insert`'s eviction kick re-locks it every iteration (line 121). `mask` and the
`buckets` Vec change only on `grow`, yet the whole point of the filter — a
lock-free negative path so novel blocks skip the store — is itself serialized
behind a global lock on every probe.
**Proposal:** Split `FilterTable` into `mask: AtomicUsize` and
`buckets: Arc<Vec<RwLock<Bucket>>>`; queries read the atomic and index the `Arc`
deref with zero global locks. `grow` builds the new `Arc<Vec<>>` + mask and
swaps them under the `Mutex` (a brief critical section, as today); readers that
grabbed the old `Arc` finish on the stale table harmlessly (same as today's
grow-race contract).
**Impact:** Dedup probe cost drops from 3 mutex acquisitions to ~2 atomic loads
+ 4 bucket-bucket reads; cross-session dedup no longer funnels through one lock.
**Effort:** S-M
**Steps:** (1) Promote `mask` to `AtomicUsize`, `buckets` to `Arc<Vec<...>>` kept
behind the `Mutex` only for the grow swap; (2) rewrite `indices`/`alt_index`/
`contains`/`insert` to read via the atomic + `Arc`; (3) `grow` swaps both; (4)
re-run FPR + saturation tests and a concurrent contains/insert stress.

## 22. Shim: don't hold the session registry lock across `ingest_turn`
**Location:** `crates/shim/src/lib.rs:311-317` (`ingest`), `320-326`
(`ingest_batch`), registry `Mutex<HashMap<_, SessionShim>>` at line 285
**Problem:** `Shim::ingest`/`ingest_batch` take the single `Mutex` around the
*entire* `ingest_turn`, which does BLAKE3 hashing, zstd compression, DashMap
inserts, and filter inserts per block. Multi-session steady state therefore
serializes every session's compression+hashing behind every other's — exactly
the contention pattern the store was DashMap-sharded to avoid.
**Proposal:** Back the registry with `DashMap<u128, SessionShim>` (dashmap is
already a dep), or shard the map by `SessionId::shard(n)`; the per-session
`SessionShim` retains its own state, so appends to different sessions proceed
concurrently. `or_insert_with` on DashMap keeps the create-on-first-use shape.
**Impact:** N-session ingest throughput stops collapsing to per-session CPU
sum; each session's zstd+BLAKE3 runs in parallel.
**Effort:** S-M
**Steps:** Swap the map type and lock sites (`session`, `ingest`,
`ingest_batch`, `apply_ack`, `store_session_ids`), keeping the create-closure
semantics; test with the demo's multi-turn path + a two-session interleave test.

## 23. Store/WAL: hand the shim's canonical bytes through `insert_with_id`
**Location:** `crates/core/src/store.rs:195-200` (WAL branch re-derives
`canonical_bytes(&b)`), `crates/shim/src/lib.rs:108, 132-136`
**Problem:** The shim already materializes each block's canonical bytes `(c, id)`
and uses `c` for compression, then discards it; when a WAL is attached,
`insert_with_id` calls `canonical_bytes(&b)` *again* — a full payload copy +
header rebuild — per durable insert, so the canonical form is built twice for
every block that hits the WAL.
**Proposal:** Add `insert_with_canonical(session_id, block, id, canon: &[u8])`
(or pass `canon` through `insert_with_id`) that logs `canon` directly on the
newly-stored path. `canonical_bytes` stays for callers that lack the buffer.
**Impact:** Removes one payload-sized alloc + copy per durable insert on the
append hot path (the WAL is the durability default for a restarted receiver).
**Effort:** S
**Steps:** Thread the param through `insert_with_id` → WAL branch; keep the
dedup path (id-only reference) unchanged; verify WAL round-trip + restart tests.

## 24. Receiver: single-pass, zero-copy canonical decode per inline block
**Location:** `crates/receiver/src/lib.rs:97-102`,
`crates/core/src/canonical.rs:49-56` (`from_canonical`)
**Problem:** The receiver flow per inline block is `decompress` → owned `Vec`
`canon`, then `from_canonical(&canon)` which does `Bytes::copy_from_slice` of the
payload (full copy), then `block_id_from_canonical(&canon)` which re-reads the
whole buffer to BLAKE3 it. The payload is copied out and the buffer scanned a
second time.
**Proposal:** Add `from_canonical_owned(canon: Vec<u8>) -> (Block, BlockId)`:
build the payload as `Bytes::from(canon)` sliced at the payload offset (zero
copy — the Vec is owned and never mutated), and hash header+payload during the
same parse pass.
**Impact:** Removes one full-payload copy and one full-buffer read per inline
block on the receiver's per-turn hot path — the mirror of the sender-side
`canonical_bytes_and_id` win.
**Effort:** M
**Steps:** Implement in `canonical.rs`; use in `handle_append`'s resolve phase;
assert id equals `block_id_from_canonical`; keep the existing `&[u8]` entry for
borrowed buffers (WAL replay).

## 25. Fountain: keep symbols sparse from wire to peel (drop the dense round-trip)
**Location:** `crates/coding/src/fountain.rs:263-284` (`add` densifies `ncoeffs`
pairs into a k-byte vector), `316-327` (`peel_decode` re-scans all k columns to
rebuild the sparse cover), `325` (clones every payload)
**Problem:** `FountainDecoder::add` parses the sparse `(idx, coeff)` pairs it
just read off the wire and writes them into a `vec![0; k]` dense coeff array;
`decode` then scans all k columns of every row again to reconstruct crisp sparse
covers, and clones each payload into a `ParsedSym`. The wire format is already
sparse — the dense interim is a wasted O(k) allocation + O(k) rescan per symbol.
**Proposal:** Store the sparse cover parsed directly in `add` (a `ParsedSym`-like
entry, coeffs moved from the wire slice, payload slice borrowed) and feed
`peel_symbols` that representation; delete the dense-row bridging.
**Impact:** Halves per-symbol decoder memory and removes O(k) scans per symbol —
matters at K=1024 cold-start generations. Complements #10b (O(1) cover removal)
which still belongs inside the peel loop.
**Effort:** M
**Steps:** Change `add` to build `ParsedSym` directly; change `peel_decode` to
consume `&[ParsedSym]` (or keep a dense adapter for RLNC's `rows`); re-run
fountain + bulk tests.

## 26. zstd-dict decompress: size the buffer from the frame, not `len×64 + 1MB`
**Location:** `crates/compress/src/lib.rs:129`
**Problem:** The dict path allocates `cap = body.len().saturating_mul(64) + (1<<20)`
per call and hands it to `Decompressor::decompress`. Every inline block's
decompression on the receiver hot path therefore grabs ≥1 MB regardless of the
true content size (a 1 KB compressed block → 1 MB allocation).
**Proposal:** Read the zstd frame header's content size where present
(`zstd::zstd_safe` frame-header inspection) and size the buffer accordingly,
growing by doubling if the content-size is absent; or use `zstd::stream::decode_all`
which sizes from the frame.
**Impact:** Receiver per-block allocation drops from ≥1 MB to the true output
size — less memory churn and pressure on the store path.
**Effort:** S
**Steps:** Probe the frame header; fall back to a bounded-doubling loop; keep
the 0x00/0x01 marker framing; round-trip tests must stay green.

## 27. Frame decode: zero-copy over `&Bytes` (inline payloads + Bulk symbols)
**Location:** `crates/core/src/frame.rs:206-323` (`decode_frame`,
`take_bytes` at 318-322, `decode_frame_block` at 263)
**Problem:** `decode_frame` takes `&[u8]`, so every inline payload
(`Bytes::copy_from_slice` in `decode_frame_block`) and every Bulk symbol
(`${take_bytes}`, line 320) is copied out of the receive buffer. On cold start
that is the entire ~200 MB stream copied once more, per symbol.
**Proposal:** Change the decoder surface to take `&Bytes` and use `.slice()` /
`.slice_ref()` (zero-copy refcount-extended views) for payloads and symbols.
`encode_frame` already produces `Bytes`; the loopback transport hands out
`Bytes`. `decode_frame(&[u8])` can stay as a thin adapter for tests.
**Impact:** Removes a full copy of the cold-start bulk and every inline payload
on the receiver side; pushes the protocol toward `Bytes`-zero-copy end to end.
**Effort:** M
**Steps:** Add `decode_frame_bytes(&Bytes) -> Result<Frame,_>`; re-point
receiver + demo at it; validate payload lifetimes (slices borrow the input so
callers keep the buffer alive — fine over the in-process transport).

## 28. Drop the receiver's duplicate per-session id list
**Location:** `crates/receiver/src/lib.rs:49-56` (`SessionState.ids`),
`:110-113`; the same ordered list already lives in the shared store
(`crates/core/src/store.rs` `SessionLog.ids`) and in the shim
(`crates/shim/src/lib.rs:46`)
**Problem:** The ordered `Vec<BlockId>` per session is maintained in three
places — shim (needed for the client manifest / apply_ack), store (needed for
reconstruction and the WAL), and receiver `SessionState.ids` (fully derivable
from the shared store's `session_ids`/root/len). At 50M tokens the manifest is
~100 KB+ per session; three copies is 3× the memory and 3× the append push cost.
**Proposal:** The receiver reads `store.session_root`/`session_len`/`session_ids`
for its pointer + resync bookkeeping and deletes `SessionState.ids` (keep the
`decoders` map).
**Impact:** -1/3 of per-session id-list memory and one fewer `push` per block;
the root/len queries are lock-free DashMap reads.
**Effort:** S-M
**Steps:** Replace `SessionState.ids` uses (`pointer`, `handle_resync`) with
store reads; confirm session_len semantics (WAL replay re-inserts into the
store, so the store path is authoritative).

## 29. BytePipeline: keep worker threads alive across runs
**Location:** `crates/transport/src/pipeline.rs:49-77`
**Problem:** `run` spawns one thread per stage per invocation and joins them at
the end. A pipeline built once and exercised per turn pays thread create + join
every turn (µs–ms each), and stage state can't be reused.
**Proposal:** A reusable `BytePipelineRunner`: create the stage channels and
spawn the workers once (in `new`/`start`), `run` feeds the head channel and
drains the tail; a sentinel marks end-of-batch so `run` returns without tearing
the threads down.
**Impact:** Per-turn pipeline overhead drops from thread management to channel
ops; backpressure semantics unchanged.
**Effort:** M
**Steps:** Split build/run; guard against cross-run ordering (single in-flight
`run` at a time or sequence ids); keep the existing stage-order test.

## 30. MMR: share subtrees on merge instead of copying them
**Location:** `crates/core/src/mmr.rs:70-92` (`Mmr::append`)
**Problem:** Each merge concatenates both child node `Vec`s
(`nodes.extend_from_slice`) — every node is copied once per merge level it
participates in, i.e. O(log N) times over N appends → O(N log N) total copying
to build a full log. For an MMR maintained across the entire 50M-token log this
dominates append cost and doubles the tree's memory.
**Proposal:** An immutable/persistent tree layout (each subtree stored once,
merges link to existing child nodes — a "Grove"/Bagwell-style MMR) so append
stays O(log N) hashing with O(log N) node allocation and zero subtree copying.
`peaks`/`root`/`inclusion_proof` all read the same links.
**Impact:** MMR append becomes allocation- and copy-light at any log size; tree
memory drops to one copy of each subtree.
**Effort:** M-L
**Steps:** Rewrite `Tree` as shared nodes (`Arc`-linked or an arena index);
ports proofs/peaks; keep the existing properties tests and add a large-N append
benchmark.

## 31. GoldenRing: `BTreeMap` core instead of shifting Vecs
**Location:** `crates/coding/src/placement.rs:49-58` (`add`), `60-72` (`remove`)
**Problem:** `add` is a `Vec::insert` (O(N) shift); `remove` rebuilds and
re-sorts the entire ring (O(N log N)) *and reassigns positions* on every
removal, which also breaks the "insertion order doesn't matter" consistency of
the golden placement.
**Proposal:** Back the ring with `BTreeMap<u64, u64>` (position → entity id)
using `placement_fixed` for positions; `add`/`remove`/`route` all become
O(log N), and removal no longer renumbers survivors.
**Impact:** Session/agent churn cost becomes independent of ring size; routing
unchanged.
**Effort:** S
**Steps:** Replace `nodes: Vec<(u64,u64)>` with the map; implement add/remove/
route/len against it; keep the `balanced_at_every_n` test green.

## 32. Prune scheduler: stop snapshotting the full `Vec<Block>` per job
**Location:** `crates/prune/src/lib.rs:156-159` (`schedule` →
`receiver.reconstruct`), `PruneJob.log` at `124-130`
**Problem:** Every `schedule` call clones the *entire ordered log*
(`reconstruct` returns `Vec<Block>`, bumping every `Bytes` refcount and
allocating the Vec) and parks it in the pending queue. At 50M tokens that is a
full-log snapshot per scheduled prune — heavy even off the wire path.
**Proposal:** `PruneJob` holds the shared `Arc<Receiver>` (or store) +
`session_id` (+ an id-slice snapshot if the log is expected to move), and the
policy resolves blocks lazily from the store — which owns the bytes anyway —
instead of a materialized snapshot.
**Impact:** Scheduling a prune no longer clones the log; memory for queued jobs
collapses from O(log) to O(ids).
**Effort:** M
**Steps:** Change `PruneJob` to carry the receiver handle + session id (or an
`Arc` to `reconstruct_range` windows); keep the run_all contract; test with a
large synthetic session.

## 33. Fountain peel: move the revealed payload into `revealed[]`, don't clone
**Location:** `crates/coding/src/fountain.rs:367` (`revealed[target] =
Some(val.clone())`), ripples over dependents at `385-395`
**Problem:** The peeler `mem::take`s `val` (good, #10a) but then `val.clone()`s
the **full symbol into `revealed[target]` before** the dependents loop, and drops
`val` after it — one whole-symbol clone per peel, i.e. K × symbol_size bytes of
cloning per decode. That is exactly the cost `mem::take` was introduced to kill.
**Proposal:** Run the dependents loop first with `&val`, then
`revealed[target] = Some(val)` (move, zero copy). Safe: the ripple removes
`target` from every dependent's cover, so by the time the move lands nothing
reads `revealed[target]` before the residual phase.
**Impact:** Removes K full-symbol clones (up to ~4 MB per K=1024 generation).
**Effort:** S
**Steps:** Reorder the reveal after the dependents loop; re-run
`systematic_plus_repairs_decode` / `peeling_handles_heavy_loss_with_repairs`.

## 34. `bulk::decode`: parallel reassembly into a pre-sized buffer
**Location:** `crates/coding/src/bulk.rs:160-184`
**Problem:** Generation decode is parallel, but reassembly serializes
`out.extend_from_slice(&sym)` over every decoded symbol — the cold-start tail is
one long serial memcpy chain.
**Proposal:** Pre-allocate `out` at `padded_len` (encode already reports
`total_full_gens × gen_size × symbol_size`), compute each generation's byte
offset, and copy each generation's symbols in parallel (rayon) into the buffer;
truncate to `original_len` at the end.
**Impact:** The reassembly phase becomes a parallel pure-copy pass; decode
wall-clock stops growing with the sum of symbols.
**Effort:** S-M
**Steps:** Thread the padded length (from config + payload len) into `decode`;
offset each generation by its position in sorted order; keep truncation.

## 35. Multipath: atomic round-robin, not a per-frame Mutex
**Location:** `crates/transport/src/multipath.rs:35-42` (`pick`), `72-76`
(`MultipathSink::send`)
**Problem:** Both `MultipathTransport::pick` and `MultipathSink::send` lock a
`parking_lot::Mutex` per call — per open and per frame on the fan-out path.
**Proposal:** Round-robin with `AtomicUsize::fetch_add(1, Relaxed)` (weights, if
kept, via a weighted index table) — no lock on the packet path.
**Impact:** Removes a lock from every outbound frame on the multipath/multicast
fan-out.
**Effort:** S
**Steps:** Replace the `Mutex<usize>` with an atomic counter; keep the existing
behavior for `accept`.

## 36. Compressor passthrough: don't copy the whole input to mark it raw
**Location:** `crates/compress/src/lib.rs:81-87` (min-block + expansion paths
build `len+1` and copy the input to prepend 0x00)
**Problem:** Both passthrough branches allocate a `len+1` buffer and copy the
entire input just to carry a one-byte marker. Incompressible blocks (large
binaries, random snapshot data) pay a full-payload copy on the shim output path.
**Proposal:** Carry the marker in the per-block length prefix (the 4-byte length
already sent per block in `[len:u32]` framing) instead of inline in the payload,
or expose a `compress_into(&mut Vec<u8>)` that pushes the marker without a copy.
**Impact:** Passthrough blocks stop doubling in memory traffic; compressed
frames get 1 byte shorter.
**Effort:** S-M
**Steps:** Move the marker to the frame header / length nibble; update receiver
decompress + the `[len][payload]` bulk framing; round-trip + passthrough tests.

## 37. Zeckendorf: pack bits instead of one `Vec<u8>` per bit
**Location:** `crates/coding/src/zeckendorf.rs:44-70` (`zeck_encode` builds
`Vec<bool>` — 8 bytes per bit — then a second `Vec<u8>`)
**Problem:** Per codeword: a `hi`-byte `Vec<bool>`, then a `hi`-byte `Vec<u8>`;
8× waste + two allocations per framing on the raw-RDMA/self-sync path.
**Proposal:** Write bits directly into a packed `Vec<u8>` (bit `j` → byte
`j>>3`, mask `1<<(j&7)`), and have `zeck_decode`/the stream reader read the same
layout. (Fix the packing once; both ends live in this crate.)
**Impact:** 8× less memory + one allocation per codeword on the framing path;
`ZeckStream` decode's `11`-scan also touches dense bytes.
**Effort:** S
**Steps:** Rewrite encode/decode round packing; keep the LSB-first semantics and
the `11`-termination property; re-run `roundtrip`/`stream_roundtrip`.

## 38. homohash: build per-coordinate generators incrementally
**Location:** `crates/coding/src/homohash.rs:115-120` (`gens[i] = powmod(g, i+1, P)`)
**Problem:** `new` runs O(k) full modular exponentiations (each O(log P) ≈ 61
mulmods) to build the generators, but `g_{i+1} = g_i · g mod P` — a single
`mulmod` per step.
**Proposal:** Seed `gens[0] = g`, then `gens[i] = mulmod(gens[i-1], g)` per
coordinate.
**Impact:** Construction drops from k·61 mulmods to k mulmods (full k-power
slice for the modular-squaring side of #12 is unaffected).
**Effort:** S
**Steps:** Replace the `powmod` map with the incremental chain; keep all
homohash tests exact.

## 39. Hierarchical `into_groups`: don't clone the context once per group
**Location:** `crates/coding/src/hierarchical.rs:67-78` (`outer[i].clone()` into
each group's RLNC source)
**Problem:** `into_groups` clones every outer symbol into each group's source,
re-copying the whole coded context once *per group* on the N-agent fan-out path.
**Proposal:** Per-group sources as slice/`Arc` views (add a `from_slices`
constructor to `RlncEncoder`, mirroring the fountain slice path from #13) so the
groups share the one owned set of outer symbols.
**Impact:** Removes the per-group full-context copy before RLNC coding.
**Effort:** S-M
**Steps:** Add the slice-based `RlncEncoder` entry point; `into_groups` builds
`Vec<&[u8]>` groups; keep the round-trip tests.

## 40. Receiver: cheap divergence pre-check before the CPU-heavy resolve
**Location:** `crates/receiver/src/lib.rs:91-108` (resolve phase),
`:116-120` (base_root check happens *after*, under the lock)
**Problem:** `handle_append` decompresses + hashes every block in the frame
*first*, then takes the lock and discovers `base_root != st.root` — on an
out-of-sync/resync transition the entire batch's decompress+hash work is
discarded. The divergence check needs only the store's current session root.
**Proposal:** Early-exit with a lock-free store read up front:
`if frame.base_root != self.store.session_root(frame.session_id) { return Err(...) }`
before any resolve work.
**Impact:** Out-of-sync frames fail in O(1) (a DashMap lookup) instead of
burning a full turn of decompress+hash first.
**Effort:** S
**Steps:** Add the guard at the top of `handle_append` (keep the in-lock check
as the authoritative one for the race between the read and the store update);
verify the existing divergence demo path still triggers RESYNC.

---

## Suggested sequencing (Part II)

1. **S smalls:** #38, #35, #33, #37, #31, #26, #40, #23 — isolated, low risk,
   on the per-block/per-frame hot paths.
2. **Locking/contention:** #21 (cuckoo global lock), #22 (shim registry),
   #24/#27 (receiver zero-copy), #28 (id-list) — the multi-session ceilings.
3. **Memory:** #25 (sparse fountain), #32 (prune snapshot), #29 (pipeline), #36.
4. **Structural:** #30 (MMR), #34 (bulk reassembly), #39 — gated on benchmarks.
5. **Before any of it:** re-confirm the build (`cargo build --release`) and fix
   the pre-existing decode/hash test failures noted in Part I, since #25/#33
   touch that exact code.

---

## Implemented integration hot paths (Subconscious Code)

- The client retains ACKed `WireMessage` snapshots. Their large bodies are
  immutable `Arc<str>` values, so pointer equality proves the stable prefix.
  Only newly appended messages are materialized, their one required DLR
  serialization also supplies exact payload accounting, and a cached aggregate
  removes history-wide bookkeeping allocations. Reallocated but byte-equal
  messages remain correct through a content-comparison fallback.
- The HTTP sidecar reconstructs the upstream JSON request as a stream of the
  content store's existing refcounted `Bytes`, eliminating the full-history
  `Value` tree and the second contiguous serialization copy.
- A validated projection is cached by `(session_id, MerkleRoot)`. A trusted
  steady APPEND fetches only new blocks instead of reconstructing the complete
  store manifest. Small messages are coalesced into roughly 64 KiB upstream
  chunks while large blocks retain zero-copy storage. The generic frame
  endpoint invalidates the cache before RESYNC/BULK/APPEND mutations, forcing
  one safe full validation and projection rebuild on the next chat request.
  The LRU is bounded to 64 sessions and 256 MiB of represented JSON so this
  acceleration cannot grow without limit.

On the Mac-to-Spark immediate-SSE benchmark with generated Rust-shaped text,
these changes reduced median steady-state DLR request-to-first-SSE time from
61 to 26 ms at 10 MiB, 201 to 30 ms at 25 MiB, and 302 to 34 ms at 45 MiB.

A subsequent pass removed the duplicate new-message bookkeeping serialization,
cached aggregate JSON size, cached the ACK-root projection, and coalesced small
blocks. The next five-sample Spark run observed 12.1 ms at 10 MiB, 16.4 ms at
25 MiB, and 21.8 ms at 45 MiB. Network load and RTT vary between runs, so these
figures are the latest observed end-to-end transport medians rather than an
isolated attribution of every millisecond to the code change.

The release-mode projection ablation is deterministic: for a 10,000-message
history, the former store walk and 19,999-entry chunk-plan rebuild cost 0.466 ms
per request; the cached lookup cost 0.052 microseconds and retained only 80
upstream chunks. A Spark stress run with 4 KiB history blocks stayed at 9.7 ms
median for 2,560 messages / 10 MiB and 17.2 ms for 6,400 messages / 25 MiB.
