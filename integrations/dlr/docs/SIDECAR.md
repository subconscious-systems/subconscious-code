# DLR HTTP sidecar

The sidecar is the first deployable DLR network path. It lets a terminal or
agent client send only newly appended messages while an existing gateway and
model runtime continue receiving ordinary OpenAI-compatible requests.

## Placement

```text
DLR-aware terminal/client
        |
        | DLR binary envelope over HTTP
        v
dlr-sidecar + durable WAL
        |
        | ordinary POST /v1/chat/completions
        v
existing gateway -> SGLang/model runtime
```

No gateway or SGLang protocol changes are required. Place the sidecar close to
the gateway so only the short append delta crosses the slower link. The
sidecar reconstructs `messages` before forwarding upstream. Output bytes are
streamed as they arrive, including SSE, so it does not add response buffering.

## Start it

```sh
export DLR_UPSTREAM_URL=http://gateway.internal:8080
export DLR_WAL=/var/lib/dlr/receiver.wal
export DLR_INGRESS_TOKEN='replace-with-a-secret'

cargo run --release -p dlr-sidecar --bin dlr-sidecar -- \
  --listen 0.0.0.0:32180
```

The default listener is `127.0.0.1:32180`. A non-loopback bind is rejected
unless the configured token environment variable is present. TLS is expected
to terminate in the existing ingress proxy or service mesh; do not expose the
plain HTTP listener directly to an untrusted network.

The repository `Dockerfile` builds a non-root runtime image with a persistent
`/var/lib/dlr` volume and a `/readyz` health check. The image binds to
`0.0.0.0:32180`, so `DLR_INGRESS_TOKEN` is mandatory at container startup.

Useful probes:

```sh
curl http://127.0.0.1:32180/healthz
curl http://127.0.0.1:32180/readyz
curl http://127.0.0.1:32180/v1/dlr/capabilities
```

## Client lifecycle

Use `dlr_sidecar::ChatSession` once per logical conversation and
`dlr_sidecar::DlrChatClient` for the HTTP exchange:

1. Construct it from a stable, private conversation key. DLR hashes the key to
   a 128-bit wire session id; the raw key is not sent.
2. Call `prepare(new_messages, request_without_messages)` exactly once for each
   local append. The request object may contain normal OpenAI fields such as
   `model`, `tools`, `temperature`, and `stream`, but must omit `messages`.
3. POST the returned immutable bytes to `/v1/dlr/chat/completions` with content
   type `application/vnd.dlr.chat+binary; version=1`.
4. Parse `x-dlr-ack-root` and call `apply_ack`. Keep the prepared bytes until
   the ACK root has been accepted.
5. On HTTP `409`, read `x-dlr-current-root` and use the low-level
   RESYNC/MISSING/BULK flow through `DlrChatClient::synchronize`.

Only one prepared append may be in flight per `ChatSession`. The client rejects
a second `prepare` until the first root is ACKed, preventing two deltas from
sharing a stale base root. `DlrChatClient::send_chat` applies a valid ACK as
soon as response headers arrive and returns the streaming response body.

After a terminal restart, rebuild `ChatSession` from the complete canonical
conversation before sending another delta (or persist that client state in the
terminal). If the first APPEND conflicts with an already-warm sidecar, run
RESYNC/MISSING/BULK and then an empty APPEND to invoke the model. Subconscious
Code performs this repair automatically. It uses the same replacement flow
when compaction or reprojection changes the effective transcript.

`prepare` validates before changing local state. Replaying the exact prepared
body after a lost ACK does not duplicate receiver history. It can still invoke
the model a second time, just like retrying any ambiguous chat-completions POST;
use the gateway's request-id/idempotency policy for generation retries.

The sidecar forwards `authorization`, OpenAI organization/project headers,
`x-request-id`, `x-trace-id`, `traceparent`, and the existing Subconscious
session/client headers. The sidecar token is consumed locally and never sent to
the model gateway.

### Steady-state reconstruction fast path

Accepted message payloads remain immutable `Bytes` in the content store. The
sidecar caches the validated projection at its ACKed root and extends it from
only the newly appended store range. It constructs the upstream request as a
length-known HTTP body stream: a small metadata prefix, cached message-array
chunks, and a closing suffix. Small blocks are coalesced to roughly 64 KiB to
avoid thousands of tiny HTTP writes, while large blocks keep their original
zero-copy allocation. It does not parse the complete history into a
`serde_json::Value` tree or copy it into another contiguous buffer.

The projection cache is LRU-bounded to 64 sessions and 256 MiB of represented
message JSON. Eviction affects performance only: the next request reconstructs
and validates that session from the durable content store.

Any RESYNC/BULK or other generic-frame operation invalidates the projection;
the next chat request reconstructs and validates the root fully before caching
it again.

## Binary chat envelope (version 1)

All integers are little-endian:

```text
"DLR1" | version:u16 | metadata_len:u32 | metadata_json | encoded APPEND frame
```

`metadata_json` has the shape:

```json
{"request":{"model":"your-model","stream":true}}
```

Metadata is capped at 2 MiB and the complete request at 64 MiB. The standalone
frame endpoint uses `application/vnd.dlr.frame+binary; version=1` and the frame
codec in `dlr-core`.

## Durability and failure behavior

- The ACK root is exposed only after the receiver WAL has been flushed. With
  the default `DLR_SYNC_WAL=true`, that includes `fsync`/`sync_data`.
- WAL append errors are latched and cause subsequent flushes to fail instead of
  returning a false durable ACK.
- A truncated crash tail is removed during startup before new records are
  appended, so later records remain replayable.
- The WAL is exclusively locked; a second sidecar using the same path fails at
  startup. Graceful shutdown always performs a final durable sync.
- APPEND validates every reference before applying any block and recognizes an
  identical lost-ACK replay by its resulting Merkle root.
- A root conflict returns `409` and `x-dlr-current-root`; malformed envelopes
  return `400`; upstream connection failures return `502` and still include the
  accepted DLR ACK root.

## Scope and current limits

- This reduces repeated input transfer; it does not make terminal painting or
  model token generation itself faster.
- The upstream gateway still receives the reconstructed full message array.
  Eliminating that local hop would require native gateway integration later.
- Generic clients should give forks a new `ChatSession` key. A client that owns
  the logical session may replace a compacted/truncated projection under the
  same key by rebuilding the complete local view and completing RESYNC before
  the next model invocation. Steady-state APPEND remains append-only.
- WAL compaction/session expiry and native QUIC/RDMA transports are not yet
  implemented. Size and rotate storage operationally until compaction lands.
