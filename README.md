# sc — Subconscious Code

![CI](https://github.com/subconscious-systems/subconscious-code/actions/workflows/ci.yml/badge.svg)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![Rust](https://img.shields.io/badge/rust-1.89%2B-orange)

A fast, native terminal coding agent for OpenAI-compatible chat-completions
endpoints. `sc` provides an interactive TUI, resumable sessions, permissioned
tools, large-request streaming, and an optional delta transport for long agent
conversations. The client is a single Rust binary with no Python or Node runtime.

> Project status: active development. The core agent, terminal UI, session
> persistence, tools, permissions, and HTTP DLR sidecar are implemented and
> tested. MCP, hooks, and skills crates are placeholders and are not yet part of
> the user-facing product.

## Why `sc`

- Interactive terminal UI and headless automation from the same binary.
- Works with compatible self-hosted or hosted `/v1/chat/completions` APIs.
- Resumable JSONL sessions with crash-safe incremental persistence.
- Read, search, edit, append, and shell tools with explicit permission modes.
- Large bodies are serialized once and streamed from memory or disk on retry.
- DLR can send only new conversation blocks while safely falling back to JSON.
- Linux sandbox and process-tree resource containment are available when needed.

## Quick start

### Requirements

- Git
- Rust 1.89 or newer, installed with [rustup](https://rustup.rs/)
- An API key and model exposed through an OpenAI-compatible Chat Completions API
  with streaming and tool-call support

### Install from source

```sh
git clone https://github.com/subconscious-systems/subconscious-code.git
cd subconscious-code
cargo install --locked --path crates/rc-cli
```

### Install a release

Tagged releases provide precompiled binaries for Apple Silicon and Intel Macs,
plus static `x86_64` and `aarch64` Linux binaries. Replace `VERSION` with the
release you want to install:

```sh
VERSION=v0.1.0
case "$(uname -s):$(uname -m)" in
  Darwin:arm64) TARGET=aarch64-apple-darwin ;;
  Darwin:x86_64) TARGET=x86_64-apple-darwin ;;
  Linux:aarch64|Linux:arm64) TARGET=aarch64-unknown-linux-musl ;;
  Linux:x86_64|Linux:amd64) TARGET=x86_64-unknown-linux-musl ;;
  *) echo "Unsupported platform: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
curl -fLO "https://github.com/subconscious-systems/subconscious-code/releases/download/$VERSION/sc-$TARGET.tar.gz"
curl -fLO "https://github.com/subconscious-systems/subconscious-code/releases/download/$VERSION/sc-$TARGET.tar.gz.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum --check "sc-$TARGET.tar.gz.sha256"
else
  expected="$(awk '{print $1}' "sc-$TARGET.tar.gz.sha256")"
  actual="$(shasum -a 256 "sc-$TARGET.tar.gz" | awk '{print $1}')"
  test "$actual" = "$expected"
fi
tar -xzf "sc-$TARGET.tar.gz"
mkdir -p "$HOME/.local/bin"
install -m 0755 sc "$HOME/.local/bin/sc"
sc --version
```

Release archives and their checksum files include keyless Sigstore bundles.
The `subc sc install` command uses the same OS/architecture mapping and installs
the matching archive without requiring Cargo.

To see whether the installed binary is behind the newest release:

```sh
sc update
```

Use `sc update --json` for scripts. While the repository is private, the check
uses an authenticated GitHub CLI session when available; otherwise set
`GH_TOKEN`. Install an available update with `subc sc install`.

Launch `sc`. On first use, the CLI securely prompts for your Subconscious API
key and saves it to `~/.sc/key` with user-only permissions:

```sh
cd /path/to/your/project
sc
```

For automation, provide the key through the environment and verify the endpoint
before running a headless task:

```sh
export SC_API_KEY="your-api-key"
sc doctor
sc -p "explain the architecture and identify the main entry point"
```

For another compatible provider, set its base URL and model. Custom providers
use ordinary JSON unless you explicitly configure a DLR endpoint:

```sh
export SC_API_KEY="your-provider-key"
export SC_BASE_URL="https://provider.example/v1"
export SC_MODEL="provider/model-name"
sc doctor
sc
```

Inside the TUI, type a request normally. Use `@path` to include a file, `/menu`
to edit settings or resume a session, `Shift+Tab` to change permission mode,
`Tab` to queue a draft while a turn runs, `Esc` to stop, and `Ctrl+C` to quit.
If a message is queued, `Esc` waits for the current tool call to finish and
then sends it; press `Esc` again to stop immediately.

For a non-interactive read-only task:

```sh
sc -p "explain the architecture and identify the main entry point"
```

Headless writes and shell commands fail closed unless allowed in project
settings or explicitly bypassed. Read [Permissions](#permissions) before using
headless mode for code changes.

## Documentation

| Guide | Use it for |
| --- | --- |
| [Getting started](docs/GETTING_STARTED.md) | Installation, first run, provider setup, and terminal controls |
| [Configuration](docs/CONFIGURATION.md) | Settings files, environment variables, permissions, DLR, and examples |
| [Architecture](docs/ARCHITECTURE.md) | Crate boundaries, request flow, persistence, and transport design |
| [Troubleshooting](docs/TROUBLESHOOTING.md) | Endpoint, terminal, context-size, DLR, and sandbox problems |
| [Benchmarks](docs/BENCHMARKS.md) | How to reproduce transport and harness measurements |
| [DLR sidecar](integrations/dlr/README.md) | Protocol status, local testing, deployment, and internals |
| [Contributing](CONTRIBUTING.md) | Development setup, tests, review expectations, and PR workflow |

Please report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md), not in a public issue.

## How It Works

### Core Philosophy

`sc` is built for models with large context windows rather than aggressively
shrinking every read. Most inputs remain unlimited. Tool results alone have a
conservative model-facing default because provider token limits are real and a
single broad command can otherwise make the next request fail before the model
can recover.

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
- **Tool results** — projected with a 64 KiB per-result default; configurable

The assembled context is borrowed directly by the wire request and serialized
**exactly once**. Bodies through 8 MiB stay in refcounted `Bytes`; larger bodies
promote to an immutable temporary spool and stream from disk. With gzip, serde
streams directly into the compressor, so raw and compressed bodies are never
retained together. Retries reopen that exact spool (or clone the small `Bytes`)
and never re-serialize the request.

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
   | `Parallel` | `Read`, `ReadMany`, `Glob`, `Grep`, `GrepMany` | Run concurrently, bounded to 8 in flight |
   | `SerialWrite` | `Write`, `Append`, `Edit` | Run one at a time, in order |
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

## Provider and transport configuration

Defaults target `https://api.subconscious.dev/v1` with model
`subconscious/glm-5.2`. Override these per invocation with `--base-url` and
`--model`, per shell with `SC_BASE_URL` and `SC_MODEL`, or persistently in
`~/.sc/settings.json`. See [Configuration](docs/CONFIGURATION.md) for precedence
and complete examples.

### Optional DLR sidecar

DLR avoids re-uploading the complete conversation on every tool-loop turn. The
sidecar lives in this repository at `integrations/dlr`; it reconstructs the
ordinary OpenAI request beside the gateway, so neither the gateway nor SGLang
needs to change.

Build and run it independently:

```sh
cargo build --manifest-path integrations/dlr/Cargo.toml --release \
  -p dlr-sidecar --bin dlr-sidecar

export DLR_UPSTREAM_URL=https://api.subconscious.dev
export DLR_WAL=/var/lib/dlr/receiver.wal
export DLR_INGRESS_TOKEN='replace-with-a-secret'
integrations/dlr/target/release/dlr-sidecar --listen 0.0.0.0:32180
```

For the default Subconscious provider, Subconscious Code tries DLR first and
leaves `SC_BASE_URL` as the normal JSON endpoint used for fallback. Custom
providers use JSON unless DLR is explicitly configured. The shipped DLR URL is
`https://api.subconscious.dev`, ready for the gateway's `/v1/dlr/*` routes. To
test against the local sidecar before those routes are deployed, override it:

```sh
export SC_DLR_URL=http://127.0.0.1:32180
export SC_DLR_INGRESS_TOKEN="$DLR_INGRESS_TOKEN"
sc doctor
```

The setting `provider.dlr_enabled` (also available under `/menu` → Settings)
controls the feature. `true` means DLR first with normal JSON fallback; `false`
means normal JSON only. `SC_DLR_ENABLED=true|false` overrides the file. The
fallback happens only when the bounded capability probe fails before DLR
becomes active; it never silently resends a model request in the middle of an
active DLR run. The older `provider.request_transport` /
`SC_REQUEST_TRANSPORT` setting remains compatible: `auto` is equivalent to
enabled, `json` to disabled, and expert `dlr` mode fails closed. The boolean
setting wins if both forms are present. Other DLR file settings are
`provider.dlr_url`, `provider.dlr_ingress_token_env`, and
`provider.dlr_repair_margin_pct` (default 5). Secrets are resolved only from
the named environment variable.

The sidecar must have durable, sticky state (one WAL owner, or routing affinity
to the owner) and should sit on the gateway-side of the slower network link.
It streams SSE responses without buffering. Its current binary envelope is
capped at 64 MiB; the reconstructed gateway request is still subject to the
gateway's existing body limit. See
[`integrations/dlr/docs/SIDECAR.md`](integrations/dlr/docs/SIDECAR.md).

To isolate transport contribution to time-to-first-token against an
immediate-SSE test gateway, use the repeatable size-ladder example:

```sh
SC_TTFT_JSON_URL=http://gateway.test/v1 \
SC_TTFT_DLR_URL=http://sidecar.test:32180 \
SC_TTFT_DLR_TOKEN="$DLR_INGRESS_TOKEN" \
SC_TTFT_SIZES_MIB=1,10,25,45 \
SC_TTFT_REPEATS=3 \
cargo run --release -p rc-proto --example dlr_ttft
```

Set `SC_TTFT_SYNTHETIC_SOURCE=1` for a highly compressible generated-Rust
corpus. The default high-entropy corpus is deliberately unfavorable to gzip.
Set `SC_TTFT_HISTORY_BLOCK_KIB=4` to split the stable context into many small
messages and exercise long-history bookkeeping and HTTP chunking rather than a
single large message.
The benchmark includes upload, sidecar reconstruction, its local full-JSON
forward, and first SSE bytes; it intentionally excludes model queue and prefill.

## Use

```sh
sc                       # interactive TUI
sc --continue            # resume the most recent session
sc -p "explain src/"     # headless one-shot, prints the answer to stdout
sc doctor --body-ladder    # measure the gateway's real maximum request size
```

### Headless benchmarks

The CLI can write a stable performance report and an ATIF v1.7 trajectory for
headless evaluation runs:

```sh
SC_API_KEY="your-api-key" sc \
  --benchmark-report report.json \
  --benchmark-trajectory trajectory.json \
  -p "fix the task"
```

The report contains timing, token, cost, retry, tool, and build-provenance
fields without prompt or tool-result content. The trajectory is an explicit
transcript artifact and may contain sensitive task data; review it before
sharing.

In the TUI: `Shift+Tab` cycles permission mode, `Tab` queues a draft during a
turn, `Esc` stops (or sends a queued message after the active tool call), and
`Ctrl+C` quits. `@` completes file paths and `/` completes commands (`/menu`,
`/clear`, `/help`, `/mode`, `/rewind`). The status bar shows the model, mode,
and current context tokens/cache-hit rate; a preflight estimate is shown until
the provider returns the authoritative prompt-token count.

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

`Read`/`Glob`/`Grep` run freely. `Write`/`Append`/`Edit`/`Bash` escalate to an Ask, which
the TUI answers inline (`y` once / `s` session / `a` always / `n` no).

**Headless runs fail closed.** With no TTY there's nobody to ask, so a `-p` run
*denies* every write and command — the model gets a denied tool result and
carries on. That's deliberate, but it means `sc -p "fix the bug"` won't modify
anything until you either grant rules or bypass:

```json
{ "permissions": { "allow": ["Write", "Append", "Edit", "Bash(cargo:*)", "Bash(git:*)"] } }
```

Save that object as `./.sc/settings.json` to grant only this project those
headless permissions.

Or `sc -p "..." --dangerously-skip-permissions` (still hard-denies catastrophic
commands; refuses to run in CI without `SC_DANGEROUS=1`).

`Shift+Tab` cycles the mode, ordered from most cautious to most permissive:

| Mode | Behavior |
| --- | --- |
| `ask` | Confirm **every** tool call, reads included |
| `default` | Confirm mutating tools (`Write`/`Append`/`Edit`/`Bash`); reads run freely |
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
All remain configurable; only tool-result projection is bounded by default:

| Setting | Default | Was | Enforced in |
| --- | --- | --- | --- |
| `context.tool_result_cap` | 64 KiB | 16 KB per tool result | Context assembler |
| `context.inline_file_cap` | unlimited | 8 KB per `@file` mention | Context assembler |
| `context.bash_output_cap` | unlimited model projection | 30 KB of stdout+stderr | `Bash` tool |
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
the cap actually in force. `Bash` has a separate 2 MiB-per-stream transport
ceiling regardless of that setting: it retains the beginning and end of noisy
stdout/stderr rather than letting a child allocate until the host swaps. A
background shell similarly publishes a bounded 8 MiB rolling log. One session
supervisor owns all detached children, drains their nonblocking pipes, reaps
them, and rotates each log as two append-only 4 MiB segments instead of
periodically rewriting the whole log. These are process-safety bounds, not
context-budget policy.

From `bash_description` in `crates/rc-tools/src/bash.rs:85`:

```rust
let limit = if cap == 0 {
    "stdout+stderr are ANSI stripped and returned from a bounded head+tail capture window"
        .to_string()
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

The estimator does not control context. The independent tool-result projection
cap prevents a single runaway result from crossing the provider's real ceiling.

### Request Size and Memory Efficiency

The critical optimization that makes large context feasible:

**Single Serialization with Refcounting or Spooling**
- The request borrows messages/tools during serialization instead of cloning the
  whole request graph
- The request body is serialized **exactly once** to `Bytes` (≤8 MiB) or an
  immutable temp-file spool (>8 MiB)
- With gzip, serde writes straight into the compressor instead of retaining both
  raw JSON and compressed buffers
- Retries don't re-serialize or re-copy; they clone a refcount or reopen the
  same spool as a streaming body
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
  you measure on the Linux box: `sc doctor --body-ladder` with a real ≥12 MB file

### Other Size-Related Choices

**No total request timeout by default.** A total timeout covers the upload, so
on a large body it expires mid-upload and triggers a retry that re-uploads from
scratch. Instead, liveness comes from `idle_timeout_ms` (default 120 s), which
measures raw response-body activity: SSE heartbeat comments and partial frames
keep a healthy stream alive without appearing in the transcript. Set
`timeout_ms` in settings if you want a total budget.

**Client retries are off by default.** The Subconscious gateway/router owns
upstream retry and failover. Set `provider.max_retries` or `SC_MAX_RETRIES` only
for a direct endpoint that has no retrying intermediary; stacking retry layers
amplifies overload and circuit-breaker failures.

**The default completion allowance is 8192 tokens.** The GLM route otherwise
uses an observed 4096-token implicit ceiling that can cut a tool call after a
long reasoning trace. `SC_MAX_TOKENS=0` (or `--max-tokens 0`) restores the
provider default. A truncated response gets at most one synthetic continuation;
SC will surface `length`/`incomplete` rather than repeatedly sending recovery
prompts.

**GLM reasoning defaults to `high`, not `max`.** Spark traces showed hidden
reasoning dominating wall time, while prompt-cache hit rate remained about 98%.
SC now sends `reasoning_effort: "high"` by default. Use
`SC_REASONING_EFFORT=max`, `--reasoning-effort max`, or
`provider.reasoning_effort`; set `off` to omit the field for a provider that
does not support it.

**Long generated files use bounded appends.** `Append` commits one chunk
atomically and returns its byte `new_size`. Supplying that value as the next
chunk's `expected_size` prevents a repeated or stale call from duplicating
content. `GrepMany` similarly collects up to 32 already-known searches into one
model round trip.

**`--debug` truncates the body log** to 8 KB plus a byte count. Logging a 200 MB
request per debug run is its own outage. `SC_DEBUG_FULL_BODY=1` restores the full
dump only for in-memory bodies; disk-spooled requests always remain preview-only.

**Adaptive request gzip** (`SC_REQUEST_GZIP` or `provider.request_gzip` in
settings). It is enabled automatically for `api.subconscious.dev`; JSON often
compresses 5–10×. If a gateway returns 415 or a clear compressed-body parse
error, SC retries that request once uncompressed and remembers to leave gzip off
for the rest of the process. Set `SC_REQUEST_GZIP=0` to disable the probe.

Failed streams persist bounded private diagnostics: partial text/reasoning/tool
arguments, event counts, and the last raw/semantic activity timestamps. These
remain in the session JSONL and are deliberately omitted from ATIF trajectories.

### Linux Session Resource Containment

On a systemd Linux host, every interactive or headless `sc` run automatically
re-enters a transient user scope. The scope contains the editor and all tool
descendants, so concurrent builds cannot force the entire host into memory
reclaim. Where a user systemd manager is unavailable (common inside benchmark
containers), `sc` applies an inherited `RLIMIT_AS` hard-memory fallback instead.
This is process containment only; it does not limit model context.

Defaults are sized from the host: one eighth of RAM (4–12 GiB), a soft memory
threshold at 75% of that, 2 GiB of swap, 512 processes/threads, and half the
host CPUs capped at 8 cores. Every Bash call also gets an independent process
group, which is killed in full on timeout, turn cancellation, or session exit.

Override these for a benchmark worker with:

| Variable | Meaning |
| --- | --- |
| `SC_RESOURCE_MEMORY_MAX_MB` | Hard memory limit for one `sc` process tree |
| `SC_RESOURCE_MEMORY_HIGH_MB` | Reclaim threshold below the hard limit |
| `SC_RESOURCE_SWAP_MAX_MB` | Swap allowed to the scope |
| `SC_RESOURCE_TASKS_MAX` | Maximum processes/threads in the scope |
| `SC_RESOURCE_CPU_QUOTA_PERCENT` | CPU quota (`100` = one core) |
| `SC_RESOURCE_TERMINATE_PERCENT` | Sustained memory percentage that triggers graceful cancellation (default `90`) |
| `SC_RESOURCE_LIMITS=0` | Disable the systemd scope |

Benchmark reports include the build id plus scope or rlimit memory current,
peak, monitor peak, maximum, controlled-pressure termination, and the cgroup
OOM-kill counter where available. At 75%, 85%, and the termination threshold,
the monitor emits telemetry; three consecutive terminal samples cancel the
active turn before the kernel's hard OOM boundary.

### Gateway Ceiling — Measure This First

**The client is not the bottleneck — the gateway is.** A 12 MB request is
possible on the wire, but the gateway may reject it. Measure with `sc doctor
--body-ladder`:

```sh
sc doctor --body-ladder
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
| `rc-tools` | `rc-core`, `rc-perm`, `rc-sandbox` | `Read`, `ReadMany`, `Write`, `Append`, `Edit`, `Glob`, `Grep`, `GrepMany`, `List`, `Bash` |
| `rc-session` | `rc-core` | JSONL persistence, resume, `/rewind` |
| `rc-rt` | `rc-core`, `rc-session` | Bounded/coalesced event transport, action ownership, async persistence |
| `rc-tui` | `rc-rt`, `rc-core`, `rc-session`, `rc-config` | The ratatui frontend |
| `rc-cli` | all of the above | Entry point, wiring, `doctor` |

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
    ├─ Save the turn to ~/.sc/sessions/<id>.jsonl
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

**rc-tools** — The built-in read, search, write, append, listing, and shell
tools. Each checks caps, truncates output if needed, and
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
- Drains a single-consumer queue with a bounded/coalesced presentation budget;
  tool lifecycle, permission, artifact, error, and turn-boundary events are
  structural and are never evicted
- Sends `UserAction` through bounded channels with one explicitly owned active turn
- Feeds permission prompts back via `Prompter::ask`

Completed interactive turns are handed to a dedicated background session
writer, so filesystem flush latency is not part of the model/tool event path.
File rewind snapshots use a durable per-session content-addressed store:
repeated before-images share one blob, references survive restart, and event/UI
clones carry `Arc` handles rather than duplicating file bytes.

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

Provider retries default to zero: the gateway/router is the single retry owner.
Direct endpoints can opt in with `SC_MAX_RETRIES`; the immutable body means an
explicit retry re-sends the same `Bytes` or spool rather than rebuilding it.

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo test --manifest-path integrations/dlr/Cargo.toml --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The Linux sandbox is the only OS-gated code. To type-check it from macOS:

```sh
rustup target add x86_64-unknown-linux-gnu
cargo check -p rc-sandbox --target x86_64-unknown-linux-gnu --all-targets
```

Planned work is tracked in the repository issue tracker. MCP, hooks, skills,
and full compaction are not yet part of the user-facing product.
