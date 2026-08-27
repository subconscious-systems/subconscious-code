# Architecture

Subconscious Code separates the agent loop, provider protocol, tools,
permissions, persistence, and presentation into small Rust crates. The CLI is
the composition root; lower layers do not depend on the terminal UI.

## One turn

```text
user input
   │
   ▼
context assembly ── session history, AGENTS.md, @files, tool schemas
   │
   ▼
request transport ── DLR when explicitly/default-eligible, otherwise JSON
   │
   ▼
OpenAI-compatible /v1/chat/completions endpoint
   │                         │
   │ streamed final text     └─ streamed tool calls
   │                                      │
   │                                      ▼
   │                         permission pass, then execution
   │                                      │
   └──────────────────────────────────────┘
                    repeat until final answer
```

Each completed event is persisted incrementally. A crash may lose the active
partial turn, but it does not corrupt the readable session prefix.

## Crate boundaries

| Crate | Responsibility |
| --- | --- |
| `rc-cli` | CLI parsing and construction of the complete application |
| `rc-tui` | Ratatui frontend, composer, menus, transcript, and prompts |
| `rc-rt` | Runtime actions, bounded/coalesced events, cancellation, persistence handoff |
| `rc-core` | Provider-independent agent/tool loop and turn model |
| `rc-proto` | OpenAI wire types, one-pass bodies, DLR client, retries, SSE decoding |
| `rc-ctx` | Context assembly, memory discovery, and `@file` expansion |
| `rc-tools` | Read/search/list/write/append/edit/Bash implementations |
| `rc-perm` | Permission modes, rules, Bash parsing, and path containment |
| `rc-session` | JSONL sessions, resume, rewind metadata, and content-addressed snapshots |
| `rc-config` | Layered settings and secret-safe API-key resolution |
| `rc-sandbox` | Linux Landlock/seccomp policy; no-op elsewhere |
| `rc-tokenize` | Context-size estimation for observability |
| `rc-algebra` | Hashing and state primitives used by the runtime and DLR path |

`rc-mcp`, `rc-hooks`, and `rc-skills` reserve future boundaries; they do not yet
provide user-facing integrations.

## Data ownership

- `rc-session::Session` is the durable conversation source of truth.
- Wire messages are fresh projections and are not stored as a second history.
- Large text bodies use shared `Arc<str>` allocations across projections.
- Requests at or below 8 MiB use immutable in-memory bytes; larger requests use
  an immutable temporary spool.
- A retry reuses the exact encoded body instead of rebuilding application state.
- Tool-result projection can be bounded without deleting the complete session
  or file-change artifact.

## Tool scheduling

The permission pass is sequential and follows model order. Approved tools then
run by declared concurrency class:

- Reads and searches may run concurrently.
- Writes and edits run serially in model order.
- Bash is exclusive.

Results return to the model in its original call order, independent of actual
completion order.

## DLR transport

DLR is application-layer append-log replication. It changes the client-to-edge
request representation, not the model API:

```text
sc ── small append frame ──► DLR sidecar
                              │ reconstruct complete request
                              ▼
                     /v1/chat/completions gateway or SGLang
                              │
                              └── unbuffered SSE ──► sc
```

The sidecar durably ACKs receiver state before the client discards retry data.
Safe `auto` mode falls back only when capability discovery fails before DLR is
active. It never replays an accepted model invocation through JSON.

DLR complements SGLang RadixAttention: DLR reduces WAN upload, while prefix/KV
caching reduces repeated model prefill. The deployable sidecar needs only an
OpenAI-compatible upstream and requires no SGLang fork.

## Security boundaries

Permission prompts are policy, while the optional Linux sandbox is kernel
confinement. They are deliberately separate. The hard-deny floor rejects known
catastrophic shell forms even in auto/bypass operation, but it is not a complete
substitute for running untrusted tasks in an isolated VM or container.

See [SECURITY.md](../SECURITY.md) for reporting and deployment guidance.
