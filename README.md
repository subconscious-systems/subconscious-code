# sc — Subconscious Code

A terminal coding agent in first-party Rust, speaking any OpenAI-compatible
`/v1/chat/completions` endpoint. Single static binary; no Python, no Node.

**The defining feature: no context-window limit and no request-size cap.**
Claude Code caps a request at 32 MB and truncates tool output to protect a fixed
window. `sc` does neither. Every per-item truncation cap is configurable and
ships at `0` — unlimited — because the model behind it is built to ingest entire
conversations and arbitrarily large files.

## How It Works

### Core Philosophy

`sc` is built on a simple inversion: instead of a client that guards a small
context window and truncates aggressively, it's a client that trusts the model.
The agent loop does not police the context — it assembles it faithfully and sends
everything. Truncation is still *possible* (cap any field in `~/.sc/settings.json`),
but the defaults let the model see the whole picture.

### The Agent Loop

Every user message triggers an agent loop:

1. **Assemble** — collect the full conversation history, system prompt, memory
   files, inline `@file` mentions, and the pending user query
2. **Estimate** — count characters and convert to an approximate token estimate
   (using a calibrated factor from past responses)
3. **Request** — send the full context in a single POST, with streaming enabled
4. **Stream** — buffer the response as it arrives, detecting when the model calls
   tools
5. **Execute** — run each tool call (respecting permission rules: `ask` / `default`
   / `acceptEdits` / `plan` / `auto`)
6. **Loop** — if the model asked for tool results, reassemble the context with
   those results and send another request
7. **Stop** — when the model sends a final message (no tool calls) or hits
   iteration/time limits

### Request Assembly

Large requests are the key. The assembly pipeline:

- **Turn history** — every prior message (user + model + tool results), loaded
  from the session file (`~/.sc/sessions/<id>.jsonl`)
- **System prompt** — generated fresh each turn, embedding model identity and
  tool definitions
- **Memory** — `AGENTS.md` files (global + project-local + repo-local), loaded
  in order and merged into the context
- **Inline expansions** — `@file` paths in the user prompt are read and inlined
- **Tool results** — truncated only if a cap is set (default: unlimited)

The assembled context is serialized **exactly once** to bytes, then wrapped in a
refcounted `Arc<Bytes>`. Retries don't re-serialize — they just clone the refcount.
This is why a 12 MB body doesn't balloon the process memory: one copy for the wire,
not four.

When the model returns a batch of tool calls, the loop runs two distinct passes
over it:

1. **Permission pass, strictly sequential.** Every call is checked in order —
   Allow runs it, Deny records a denied result, Ask suspends the whole batch on
   the prompter until you answer. Grants accumulate *within* the pass, so
   answering `s`/`a` on the first `Bash` can silently auto-allow a later one in
   the same batch.
2. **Execution pass, by concurrency class.** Only the approved calls reach it,
   bucketed by each tool's declared class:

   | Class | Tools | Behavior |
   | --- | --- | --- |
   | `Parallel` | `Read`, `Glob`, `Grep` | Run concurrently, bounded to 8 in flight |
   | `SerialWrite` | `Write`, `Edit` | Run one at a time, in order |
   | `Exclusive` | `Bash` | Runs alone, nothing else in flight |

Each parallel tool runs in its own `tokio::spawn`, so a panicking tool becomes an
error result for that call rather than taking down the loop. Results are
reassembled in the model's original call order regardless of finish order — the
model never sees the scheduling.

See [Permissions](#permissions) for the modes and how to configure rules.

### Sessions

A session is one conversation thread, appended to `~/.sc/sessions/<id>.jsonl` as
it happens — one line per turn, flushed immediately, so a crash leaves a readable
prefix rather than a corrupt file. `sc --continue` reloads the newest one and
restores its visible transcript, saved model, latest permission mode, and full
request context before continuing. The wire format is in
[rc-session](#the-core-crates) below.

The session ID also travels as `x-subconscious-code-session-id`, which groups
every tool-loop request from that session into one Conversation on the gateway
side — a resumed session keeps its original ID, so the grouping survives
`--continue`.

## Install

```sh
cargo install --path crates/rc-cli    # puts `sc` on your PATH
export SC_API_KEY=...                 # your gateway key
sc --doctor                             # verify the endpoint before trusting it
```

Defaults point at `https://api-dev.subconscious.dev/v1` with model
`subconscious/glm-5.2`. Override per-invocation with `--base-url` / `--model`, per-shell
with `SC_BASE_URL` / `SC_MODEL`, or persistently in `~/.sc/settings.json`.

## Use

```sh
sc                       # interactive TUI
sc --continue            # resume the most recent session
sc -p "explain src/"     # headless one-shot, prints the answer to stdout
sc --doctor --body-ladder  # measure the gateway's real maximum request size
```

In the TUI: `Shift+Tab` cycles permission mode, `Esc` cancels a turn, `Ctrl+C`
quits, `@` completes file paths, `/` completes commands (`/menu`, `/clear`,
`/help`, `/mode`, `/rewind`). The status bar shows the model, mode, and current
context tokens/cache-hit rate; a preflight estimate is shown until the provider
returns the authoritative prompt-token count.

### `/menu`

`/menu` opens a full-screen modal — arrows to move, `↵` to select, `←` to go
back, `Esc` to close:

- **Projects** — every directory `sc` has been run in, derived from the session
  files in `~/.sc/sessions` (there is no project registry to keep in sync).
  Each shows its session count and when it was last touched. Open one to see
  its sessions, labeled by their first prompt, and resume any of them or start
  a fresh session in that directory. Switching sessions rebuilds the agent
  in-process; no restart.
- **Settings** — the resolved value of every setting that
  `~/.sc/settings.json` actually backs, editable in place (`↵` to type, `←/→`
  to cycle a choice) and saved straight to that file. Unknown keys in the file
  survive a save. A field currently overridden by an `SC_*` environment
  variable is marked as such, and saving it says so — the file is written, but
  the env var still wins until you unset it.
  - **Models** — the `model` row keeps a roster. `←/→` switches between saved
    models, `↵` types a new one (which is selected *and* remembered), `d`
    drops one from the list. The row shows your position in the roster
    (`[2/3]`). Persisted as a `models` array beside `model`:

    ```json
    { "model": "subconscious/glm-5.2",
      "models": ["subconscious/glm-5.2"] }
    ```

    The roster always contains the model in use, so an existing install with
    no `models` key still starts with a working list of one.
  - **Base URL** — the `base_url` row is free text; `↵`, edit, `↵` to save.

## Permissions

`Read`/`Glob`/`Grep` run freely. `Write`/`Edit`/`Bash` escalate to an Ask, which
the TUI answers inline (`y` once / `s` session / `a` always / `n` no).

**Headless runs fail closed.** With no TTY there's nobody to ask, so a `-p` run
*denies* every write and command — the model gets a denied tool result and
carries on. That's deliberate, but it means `sc -p "fix the bug"` won't modify
anything until you either grant rules or bypass:

```json
// ./.sc/settings.json — grant what this project needs
{ "permissions": { "allow": ["Write", "Edit", "Bash(cargo:*)", "Bash(git:*)"] } }
```

Or `sc -p "..." --dangerously-skip-permissions` (still hard-denies catastrophic
commands; refuses to run in CI without `SC_DANGEROUS=1`).

`Shift+Tab` cycles the mode, ordered from most cautious to most permissive:

| Mode | Behavior |
| --- | --- |
| `ask` | Confirm **every** tool call, reads included |
| `default` | Confirm mutating tools (`Write`/`Edit`/`Bash`); reads run freely |
| `acceptEdits` | Edits apply automatically; `Bash` still confirms |
| `plan` | Mutating tools are denied outright |
| `auto` | Nothing confirms. Catastrophic commands are still hard-denied |

`auto` was previously called `bypassPermissions`; that spelling is still
accepted in `settings.json` and in existing session files, so nothing breaks.
Set the starting mode with `permissions.default_mode` (or the `/menu` settings
page) — a resumed session restores whichever mode it was last in.

## What "no limit" means concretely

### The Eight Configurable Caps

Eight limits existed in the original design to protect a small context window.
All are now configurable and default to unlimited:

| Setting | Default | Was | Enforced in |
| --- | --- | --- | --- |
| `context.tool_result_cap` | unlimited | 16 KB per tool result | Context assembler |
| `context.inline_file_cap` | unlimited | 8 KB per `@file` mention | Context assembler |
| `context.bash_output_cap` | unlimited | 30 KB of stdout+stderr | `Bash` tool |
| `context.grep_output_cap` | unlimited | 30 KB of matches | `Grep` tool |
| `context.read_default_limit` | whole file | 2000 lines | `Read` tool |
| `context.read_max_line_chars` | untruncated | 2000 chars per line | `Read` tool |
| `context.glob_cap` | all matches | 1000 paths | `Glob` tool |
| `context.max_iters` | 1000 | 100 tool-loop iterations | Agent loop |

For the seven size caps, `0` means unlimited. `max_iters` is the exception — it
is not a context limit but a runaway backstop, so it stays finite; the default
was raised from 100 to 1000, far above any legitimate task but still guaranteed
to terminate.

The truncation code paths all remain, so a small-context model is still
perfectly serviceable — set the caps you want in `~/.sc/settings.json`:

```json
{
  "provider": { "base_url": "https://your-endpoint/v1", "api_key_env": "SC_API_KEY" },
  "model": "subconscious/glm-5.2",
  "context": { "tool_result_cap": 16384, "read_default_limit": 2000 }
}
```

### Smart Tool Descriptions

`Bash`, `Read`, and `Glob` build their description string at construction, from
the cap actually in force. The model is never told output is capped when it
isn't — and never left unaware of a real cap. Advertising a phantom limit
changes how a model uses a tool.

From `bash_description` in `crates/rc-tools/src/bash.rs:85`:

```rust
let limit = if cap == 0 {
    "stdout+stderr are captured in full (ANSI stripped) with no size limit".to_string()
} else {
    format!(
        "stdout+stderr are captured, ANSI stripped, and capped at {} chars \
         (head {} + tail {})",
        cap, cap / 3, cap - cap / 3,
    )
};
```

When a cap is in force the elision keeps a 1:2 head/tail split, so both the
start of the output and its (usually more informative) end survive.

### Token Estimation: Observability Only

The token estimator (`rc-tokenize`) is **wired for observation, not control:**

- It calibrates a **chars-per-token factor** using the authoritative `prompt_tokens`
  from each response
- Before each request, it estimates context size and reports it to the UI (status
  bar shows `ctx: 12.1M (~2.7M tok)`)
- **It gates nothing.** No threshold, no compaction trigger, no "context is too
  large, please trim" logic. The agent loop trusts the model.

This design accepts that the provider-side context limit still applies — the model
has its own ceiling — but the client doesn't need to guess where it is.

### Request Size and Memory Efficiency

The critical optimization that makes unlimited context feasible:

**Single Serialization with Refcounting**
- The request body is serialized **exactly once** to bytes and wrapped in `Arc<Bytes>`
- Retries don't re-serialize or re-copy; they clone a refcount
- Contrast with the previous path: built a `serde_json::Value` tree, canonicalized
  a copy of it, then a `String`, then called `.to_string()` again per retry

**Arc<str> Through the Assembly Pipeline**
- Large string fields (`Turn::content`, `WireMessage::content`, tool arguments,
  tool results) are `Arc<str>`, not `String`
- A `Turn` is projected into a `WireMessage` for each request — this is now a
  refcount bump, not a deep copy
- Pinned by `projection_shares_body_allocations_via_arc`: the same `Arc` pointer
  is verified to flow from the session turn through both projections and into
  the serialized bytes

**Measured Memory Cost**
- **12 MB tool result on the wire**: peak RSS is **86.7 MB** against a **15.2 MB**
  baseline → **~6× the payload**
- That 6× was measured *before* the `Arc<str>` optimization and has not been
  re-measured
- The dominant copy was the `Turn` → `WireMessage` projection; that's now gone
  (refcount bumps instead)
- Expect the real multiple to be **lower**, but budget to the old number until
  you measure on the Linux box: `sc --doctor --body-ladder` with a real ≥12 MB file

### Other Size-Related Choices

**No total request timeout by default.** A total timeout covers the upload, so
on a large body it expires mid-upload and triggers a retry that re-uploads from
scratch. Instead, liveness comes from `idle_timeout_ms` (default 120 s), which
distinguishes a *stalled* stream from a merely large one. Set `timeout_ms` in
settings if you want a total budget.

**`--debug` truncates the body log** to 8 KB plus a byte count. Logging a 200 MB
request per debug run is its own outage. `SC_DEBUG_FULL_BODY=1` restores the full
dump.

**Optional request gzip** (`SC_REQUEST_GZIP=1` or `provider.request_gzip` in
settings). Off by default: confirm the gateway honors `Content-Encoding: gzip`
before enabling, else it will try to parse gzipped bytes as JSON. JSON compresses
5–10×.

### Gateway Ceiling — Measure This First

**The client is not the bottleneck — the gateway is.** A 12 MB request is
possible on the wire, but the gateway may reject it. Measure with `sc --doctor
--body-ladder`:

```sh
sc --doctor --body-ladder
```

This uploads 1 / 10 / 32 / 100 / 500 MB bodies until one is refused, then names
the likely culprit:

| Ceiling | Likely cause | Fix |
| --- | --- | --- |
| Exactly **10 MB** | AWS API Gateway's payload limit | **Cannot be raised.** Needs an ALB or direct-to-origin route — infrastructure, not client code |
| Exactly **1 MB** | nginx's default `client_max_body_size` | Raisable in the nginx config |
| **≥ 32 MB** | Nothing — clears Claude Code's cap | None needed |

If the gateway caps at 10 MB, `sc` is *more* limited than Claude Code's 32 MB —
a showstopper for the thesis. Measure first.

## Architecture

### Crate Dependency Graph

15 crates with strict dependency direction (lower crates depend on nothing above):

Arrows point from a crate to what it depends on. Five leaves depend on nothing
inside the workspace, and everything converges on `rc-cli`.

```
  layer 4   rc-cli ......................... entry point; wires everything
                │
  layer 3   rc-tui ......................... frontend (→ rc-rt, rc-core, rc-session, rc-config)
                │
  layer 2   rc-rt      rc-session   rc-tools   rc-ctx
            transport  persistence  the tools  assembly
                └───────────┴───────────┴──────────┘
                                │
  layer 1                   rc-core .......... the agent loop
                                │
                ┌───────────────┼───────────────┐
  layer 0   rc-proto        rc-perm        rc-tokenize      rc-config   rc-sandbox
            the wire        permissions    estimation       settings    confinement

  stubs: rc-mcp, rc-hooks, rc-skills (declared, not yet implemented)
```

Read as a table:

| Crate | Depends on | Role |
| --- | --- | --- |
| `rc-proto` | — | Wire types, SSE decoding, tool-call reassembly, retry |
| `rc-perm` | — | Deny→allow→ask rules, parsed Bash matching, path containment |
| `rc-tokenize` | — | Token estimation (observability only) |
| `rc-config` | — | `settings.json` parsing, defaults, `SC_*` env precedence |
| `rc-sandbox` | — | Landlock + seccomp (Linux; no-op elsewhere) |
| `rc-core` | `rc-proto`, `rc-perm`, `rc-tokenize` | The agent loop, `Tool` trait, `Turn`/`Session` model |
| `rc-ctx` | `rc-core`, `rc-proto`, `rc-tokenize` | System prompt, environment, `AGENTS.md`, `@file` expansion |
| `rc-tools` | `rc-core`, `rc-perm`, `rc-sandbox` | `Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash` |
| `rc-session` | `rc-core` | JSONL persistence, resume, `/rewind` |
| `rc-rt` | `rc-core`, `rc-session` | Event transport (`broadcast` + `mpsc`) |
| `rc-tui` | `rc-rt`, `rc-core`, `rc-session`, `rc-config` | The ratatui frontend |
| `rc-cli` | all of the above | Entry point, wiring, `--doctor` |

Note `rc-core` depends on neither `rc-tools` nor `rc-ctx` — it takes a
`ToolRegistry` and an optional `ContextAssembler` as trait objects, so the loop
never names a concrete tool or assembler. That's what keeps it testable with
fakes and what lets `rc-cli` be the only place the real graph is assembled.

### Data Flow: A Single Turn

```
User input (TUI or -p)
        ↓
rc-config loads Settings (env vars override file)
        ↓
rc-session loads or creates a Session
        ↓
rc-core::AgentLoop::run() — the main loop:
    ├─ rc-ctx assembles context:
    │  ├─ Load session turns from rc-session
    │  ├─ Generate system prompt
    │  ├─ Load AGENTS.md from rc-ctx
    │  ├─ Inline @file mentions (rc-ctx)
    │  └─ Truncate tool results (rc-ctx, respecting caps)
    │
    ├─ rc-tokenize estimates tokens (report to UI, no gating)
    │
    ├─ rc-proto serializes to bytes (once, into Arc<Bytes>)
    │
    ├─ rc-proto sends via ChatClient (streaming)
    │
    ├─ Buffer the response, detect tool calls
    │
    ├─ For each tool call:
    │  ├─ rc-perm checks permission (allow/deny/ask)
    │  │  └─ If ask: rc-rt emits PromptEvent, rc-tui renders it, awaits keypress
    │  ├─ rc-tools executes (Bash, Read, Write, etc.)
    │  └─ rc-core appends result to Turn
    │
    ├─ Save the turn to ~rc-session JSONL
    │
    └─ Loop if model asked for tool results, else stop
        ↓
rc-tui renders response, updates session file
```

### The Core Crates

**rc-core** — The agent loop lives here (`AgentLoop::run`). It's a plain async
library with no UI dependencies. The loop:
- Calls an injected `Model` (usually `rc-proto::ChatClient`) to stream a response
- Buffers tool calls and calls an injected `Tool` (via `ToolRegistry`)
- Checks each tool with an injected `PermissionChecker` (usually `rc-perm::PermissionEngine`)
- Emits events to an `EventSink` (the TUI renders them)
- Receives cancellation tokens (Esc to abort)

**rc-proto** — The wire layer. Sends streaming requests to `/v1/chat/completions`,
handles SSE framing, buffers `delta` chunks into complete tool calls (which can
span 5–10 chunks), retries on transient errors, and tracks usage.

**rc-tools** — Six tools. Each checks caps, truncates output if needed, and
returns a `ToolOutcome`. `Bash` additionally respects the sandbox (if enabled).

**rc-perm** — Permission checking. Rules are:
- `allow: [tool names]` — run freely
- `deny: [tool names]` — return a denied result, no ask
- `ask` mode — emit a prompt event and await a keypress (`y` once, `s` session,
  `a` always, `n` no)

Bash commands can be parsed (`Bash(cargo:*)` matches `cargo build` but not
`rm -rf /`).

**rc-ctx** — Context assembly:
- Load system prompt (identity + tool definitions)
- Load AGENTS.md from `~/.sc/AGENTS.md` → `./.sc/AGENTS.md` → `./AGENTS.md`
- Read inline `@file` paths
- Truncate tool results to cap
- Generate tool descriptions from the actual caps in use

**rc-session** — JSONL persistence, flushed on every append so a crash leaves a
valid prefix. Line 1 is a `SessionHeader`; every line after it is one `Turn`,
tagged by a `type` field:

```jsonl
{"id":"<uuid>","cwd":"/repo","model":"subconscious/glm-5.2","mode":"default","extra_dirs":[]}
{"type":"user","content":"explain src/","ts":1755300000000}
{"type":"assistant","text":"Let me look.","calls":[{...}],"usage":{...}}
{"type":"tool_result","call_id":"call_1","tool":"Bash",
 "result":{"kind":"ok","content":"exit: 0\nhello","truncated":false},"duration":42}
```

The header carries `mode` and `extra_dirs`, which is why a resumed session comes
back in the permission mode it was last in rather than the default. `Turn` is the
source of truth — the wire form is a fresh projection per request, never stored,
so changing how turns are rendered doesn't invalidate old session files.

`--continue` takes the newest file that has actual history; header-only orphans
from aborted startups are skipped on both sides (the store is created *after* the
terminal check, and `latest` filters them).

**rc-rt** — Event transport. The TUI is a synchronous poll loop that:
- Drives `rc-core` with a `broadcast` channel (AgentEvent stream)
- Listens on an `mpsc` for `UserAction` (keystrokes, mode changes)
- Feeds permission prompts back via `Prompter::ask`

**rc-tui** — The TUI frontend. Renders:
- Transcript (user messages, streaming responses, tool calls, tool results)
- Composer (multiline input, `@file` completion)
- Status bar (model, mode, token usage, context size)
- Modal menu (`/menu`)
- Permission prompts

**rc-config** — Settings. Loads from `~/.sc/settings.json`, overridden by `SC_*`
env vars. The resolved config is injectable (so tests can swap endpoints).

**rc-sandbox** — Linux-only. Wraps `Bash` execution with Landlock (filesystem
restrictions) and seccomp (syscall filtering). No-op on macOS/Windows.

### Memory Hierarchy

`AGENTS.md` files are loaded in order, lowest precedence first:
1. `~/.sc/AGENTS.md` — global memory (all projects)
2. `./.sc/AGENTS.md` — project-local memory (committed to repo)
3. `./AGENTS.md` — repo-root memory (fallback if project one doesn't exist)

Later files override earlier ones.

### Request Headers and Retries

Every inference request carries two identifying headers:

- `x-subconscious-client: subconscious_code`
- `x-subconscious-code-session-id: <uuid>`

New sessions get opaque UUID-based IDs; a resumed session keeps the ID already
in its file, so `--continue` and `--resume` stay inside the same gateway
Conversation.

Retries default to 2 attempts on transient failures (429/5xx). This is only
cheap because the body is refcounted — see
[Request Size](#request-size-and-memory-efficiency) — so a retry re-sends the
same `Bytes` rather than rebuilding a multi-megabyte payload.

## Development

```sh
cargo build --workspace
cargo test  --workspace          # 367 tests, 0 failures
cargo clippy --workspace --all-targets -- -D warnings
```

The Linux sandbox is the only OS-gated code. To type-check it from macOS:

```sh
rustup target add x86_64-unknown-linux-gnu
cargo check -p rc-sandbox --target x86_64-unknown-linux-gnu --all-targets
```

Milestones: chat completions (M0), streaming + tool loop (M1), core tools (M2),
permissions (M3), TUI (M4), session persistence (M5), context assembly (M6),
background shells + sandbox + rewind (M7), and the unlimited-context/request
track plus `sc --doctor` (M8). Still ahead: MCP, hooks, skills, and full
compaction — see `working-cli-plan.md`.
