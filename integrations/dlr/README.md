# dlr

An append-log replication protocol for moving a growing coding-agent message
history to an OpenAI-compatible model gateway without resending the full history
on every turn.

This repo is the **implementation** of [`DESIGN.md`](./DESIGN.md). See [`BENCHMARKS.md`](./BENCHMARKS.md) for the theoretical performance model vs gRPC.

## Status

The core protocol and an HTTP deployment path are implemented and tested. The
`dlr-sidecar` binary accepts DLR append frames, persists receiver state, rebuilds
a normal OpenAI-compatible `messages` array beside the gateway, and streams the
gateway response back unchanged. It requires no gateway or SGLang changes.

The QUIC, multipath, and RDMA modules are design-level adapters/placeholders,
not production network implementations. The currently deployable transport is
HTTP/1.1 or HTTP/2 through the sidecar; the in-process loopback transport is for
tests and the demo.

Run the full verification suite with `cargo test --workspace --all-targets`.

## Deploy the sidecar

```sh
export DLR_UPSTREAM_URL=http://127.0.0.1:8080
export DLR_WAL=/var/lib/dlr/receiver.wal
cargo run --release -p dlr-sidecar --bin dlr-sidecar
```

Or build the production container (the image intentionally refuses its
non-loopback bind unless `DLR_INGRESS_TOKEN` is supplied at runtime):

```sh
docker build -t dlr-sidecar .
docker run --rm -p 32180:32180 \
  -e DLR_UPSTREAM_URL=http://gateway.internal:8080 \
  -e DLR_INGRESS_TOKEN \
  -v dlr-state:/var/lib/dlr \
  dlr-sidecar
```

It listens on `127.0.0.1:32180` by default. Point the DLR-aware terminal/client
at `POST /v1/dlr/chat/completions`; the sidecar forwards an ordinary request to
`$DLR_UPSTREAM_URL/v1/chat/completions`. For a non-loopback listener, set
`DLR_INGRESS_TOKEN` and send it in `x-dlr-sidecar-token`. See
[`docs/SIDECAR.md`](./docs/SIDECAR.md) for the wire envelope, client lifecycle,
failure handling, and deployment boundaries.

## Workspace layout

| Crate | Role |
|---|---|
| `core` | Semantic content blocks, BLAKE3 `block_id`, append-only Merkle DAG (+ Merkle mountain ranges), content-addressed store, `APPEND`/`RESYNC`/`BULK`/`ACK` frame codec, session bookkeeping, fast-path dedup filters |
| `coding` | The mathematical layer: GF(256) field arithmetic, Fibonacci multiplicative hashing, golden-ratio ring placement, Zeckendorf/Fibonacci wire varints, RaptorQ-style rateless fountain, Random Linear Network Coding, homomorphic hashing, Cayley/circulant overlays, Reed-Solomon, hierarchical (two-layer) coding, Fibonacci backoff |
| `compress` | zstd-with-dictionary compression + **reference-delta** compression (prior block as reference frame) for near-identical file snapshots |
| `transport` | In-process loopback transport, transport interfaces/models, and placeholder QUIC/multipath/RDMA adapters |
| `shim` | The local loopback shim: framing + dedup + coding only, **no prune**, holds no policy |
| `receiver` | Accumulates the per-session log in a content-addressed store, hands the runtime a stable pointer, forks the full log async to a distillation sink |
| `prune` | The secret, heavy, async prune: off the transfer hot path, plus an **incremental** top-K pruner (O(log K)/block, independent of log length) |
| `sidecar` | Durable HTTP DLR ingress, OpenAI request reconstruction, header forwarding, and unbuffered response/SSE proxy |
| `bin` | A wired demo binary exercising the whole steady-state + cold-start path and every strategy |

## The win (why it beats gRPC)

gRPC over HTTP/2 re-sends the full 50M-token array every turn. dlr ships only the **append delta** (the new turn, KB) in steady state and pays the 200MB bulk transfer **once**, coded and resumable, on cold start. Per-turn wire cost drops ~500×; multicast fan-out is ~1× not N×; the expensive prune runs off the wire path. Details and the order-of-magnitude model are in [`BENCHMARKS.md`](./BENCHMARKS.md).

## Design principle

Do **not** reinvent L4. The deployable path uses HTTP over the existing network
stack. Reliable delivery, congestion control, loss recovery, and encryption
belong to the chosen HTTP/TLS proxy or service mesh. DLR innovation stays at the
application layer: append-log replication and resumable coded cold transfer.
