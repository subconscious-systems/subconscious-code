# sc — Subconscious Code

A terminal coding agent in first-party Rust, speaking any OpenAI-compatible
`/v1/chat/completions` endpoint. Single static binary; no Python, no Node.

**The point: no context-window limit and no request-size cap.** Claude Code caps
a request at 32 MB and truncates tool output aggressively to protect a fixed
window. `sc` does neither. Every per-item truncation cap is configurable and
ships at `0` — unlimited — because the model behind it is built to take the whole
thing.

## Install

```sh
cargo install --path crates/rc-cli    # puts `sc` on your PATH
export SC_API_KEY=...                 # your gateway key
sc doctor                             # verify the endpoint before trusting it
```

Defaults point at `https://awsgateway.orangelinelabs.com/v1` with model
`gw-glm-5.2`. Override per-invocation with `--base-url` / `--model`, per-shell
with `SC_BASE_URL` / `SC_MODEL`, or persistently in `~/.sc/settings.json`.

## Use

```sh
sc                       # interactive TUI
sc --continue            # resume the most recent session
sc -p "explain src/"     # headless one-shot, prints the answer to stdout
sc doctor --body-ladder  # measure the gateway's real maximum request size
```

In the TUI: `Shift+Tab` cycles permission mode, `Esc` cancels a turn, `Ctrl+C`
quits, `@` completes file paths, `/` completes commands (`/clear`, `/help`,
`/mode`, `/rewind`). The status bar shows the model, mode, token usage, and the
current context size.

## What "no limit" means concretely

Eight caps existed to protect a small context window. All are now configurable
and default to unlimited:

| Setting | Default | Was |
| --- | --- | --- |
| `context.tool_result_cap` | unlimited | 16 KB per tool result |
| `context.inline_file_cap` | unlimited | 8 KB per `@file` mention |
| `context.bash_output_cap` | unlimited | 30 KB of stdout+stderr |
| `context.grep_output_cap` | unlimited | 30 KB of matches |
| `context.read_default_limit` | whole file | 2000 lines |
| `context.read_max_line_chars` | untruncated | 2000 chars per line |
| `context.glob_cap` | all matches | 1000 paths |
| `context.max_iters` | 1000 | 100 tool-loop iterations |

`0` means unlimited in every one. The truncation code paths remain, so a
small-context model is still serviceable — set the caps you want in
`~/.sc/settings.json`:

```json
{
  "provider": { "base_url": "https://your-endpoint/v1", "api_key_env": "SC_API_KEY" },
  "model": "gw-glm-5.2",
  "context": { "tool_result_cap": 16384, "read_default_limit": 2000 }
}
```

Each tool's advertised description is generated from its cap, so the model is
never told about a limit that isn't enforced (or left unaware of one that is).

There is no context-window check anywhere in the codebase. The token estimator
(`rc-tokenize`) is wired for **observability only** — it calibrates against each
response's authoritative `prompt_tokens` and reports the context size to the UI.
It gates nothing.

### Request size

The request body is serialized exactly once, straight to bytes, into a
refcounted `Bytes` — so a retry costs a refcount bump, not a re-upload. The
previous path built a `serde_json::Value` tree, a canonicalized copy of that
tree, and then a `String`, which multiplied a large context several times over
before the request left the process.

Honest numbers, measured end-to-end with a 12 MB tool result: **86.7 MB peak RSS
against a 15.2 MB baseline — about 6× the payload.** Serialization is one copy of
that now; the rest is `Turn`/`WireMessage` cloning in the assembly pipeline
(`prepare_turns` → `project_with`), which is the next thing to fix. Budget
memory accordingly before pushing to hundreds of megabytes.

Other size-related choices:

- **No total request timeout by default.** A total budget also covers the
  upload, so on a large body it expires mid-upload and triggers a retry that
  starts over. Liveness comes from `idle_timeout_ms` (default 120 s), which
  distinguishes a *stalled* stream from a merely large one.
- **`--debug` truncates the body log** to 8 KB plus a byte count. Logging a
  200 MB conversation per request is its own outage. `SC_DEBUG_FULL_BODY=1`
  restores the full dump.
- **Optional request gzip** (`SC_REQUEST_GZIP=1`). Off until the gateway is
  confirmed to honor `Content-Encoding` — a server that ignores it will try to
  parse compressed bytes as JSON. JSON-wrapped source compresses 5-10×.

**The client is not the binding constraint — the gateway is.** `sc doctor
--body-ladder` uploads 1 / 10 / 32 / 100 / 500 MB bodies until one is refused
and names the likely culprit. A ceiling at exactly 10 MB is AWS API Gateway,
whose payload limit cannot be raised; 1 MB is usually nginx's default
`client_max_body_size`. Measure this once per endpoint.

## Architecture

15 crates, strict dependency direction:

- `rc-proto` — wire types, SSE decoding, tool-call reassembly, retry
- `rc-core` — the agent loop, tool trait, session/turn model
- `rc-tools` — `Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash`
- `rc-perm` — deny→allow→ask rules, parsed Bash matching, path containment
- `rc-ctx` — system prompt, environment block, `AGENTS.md` memory, `@file` expansion
- `rc-rt` / `rc-tui` — event transport and the ratatui frontend
- `rc-session` — JSONL persistence, resume, `/rewind`
- `rc-sandbox` — Landlock + seccomp confinement for `Bash` (Linux; no-op elsewhere)
- `rc-config`, `rc-tokenize`, `rc-cli` — settings, estimation, entry point
- `rc-mcp`, `rc-hooks`, `rc-skills` — declared, not yet implemented

The TUI is a synchronous poll loop above the `rc-rt` runtime, driving `rc-core`
through a `broadcast` of `AgentEvent`s and an `mpsc` of `UserAction`s. `rc-core`
stays a plain library with no channel dependencies. Permission asks flow through
an async `Prompter` whose `ask` emits an event and awaits a keypress.

Sessions persist to `~/.sc/sessions/<id>.jsonl` (one header line + one `Turn` per
line, flushed per turn). `--continue` takes the newest file with actual history;
header-only orphans from aborted startups are skipped.

Memory files load hierarchically, lowest precedence first: `~/.sc/AGENTS.md` →
`<cwd>/.sc/AGENTS.md` → `<cwd>/AGENTS.md`.

## Development

```sh
cargo build --workspace
cargo test  --workspace          # 242 tests
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
track plus `sc doctor` (M8). Still ahead: MCP, hooks, skills, and full
compaction — see `working-cli-plan.md`.
