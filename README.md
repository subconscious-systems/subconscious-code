# rc

A terminal agent harness (a Claude Code–style agent) written in first-party
Rust, speaking the OpenAI-compatible `/v1/chat/completions` backend. Single
static binary; no Python, no Node.

## Status — M4 (TUI)

A headless one-shot (`rc -p "..."`) or an interactive ratatui TUI (just `rc`),
speaking any OpenAI-compatible `/v1/chat/completions` endpoint. The agent loop
runs `Read`/`Write`/`Edit`/`Glob`/`Grep`/`Bash` behind a deny→allow→ask permission
engine (parsed Bash matching, four modes, path containment); the TUI cycles modes
with `Shift+Tab`, cancels with `Esc`, and answers inline permission prompts.

The TUI (`rc-tui`) is a synchronous poll loop above an `rc-rt` runtime that drives
`rc-core` through a `broadcast` of `AgentEvent`s and an `mpsc` of `UserAction`s — it
never calls into core synchronously, and `rc-core` stays a plain library with no
channel deps. Permission asks flow through an async `Prompter` whose `ask` emits an
event and awaits a keypress; at most one ask is ever pending.

Assistant output is rendered as incremental markdown (headings, fenced code,
lists, inline `` `code` `` / `**bold**` / `*ital*` / `[links]()`), and `Edit` calls
preview as an inline word-level diff. The composer autocompletes `@file` mentions
against the session cwd (bounded filesystem walk) and `/slash` commands (`/clear`,
`/help`, `/mode`): type the trigger, move with `Up`/`Down`, accept with `Tab`, and
dismiss with `Esc`.

```sh
export RC_API_KEY=...
cargo run -q --bin rc              # interactive TUI
cargo run -q --bin rc -p "say hi"  # headless one-shot
```

Override the endpoint or model via `RC_BASE_URL` / `RC_MODEL`, or write
`~/.rc/settings.json` or `./.rc/settings.json`:

```json
{
  "provider": { "base_url": "https://your-endpoint/v1", "api_key_env": "RC_API_KEY" },
  "model": "your-model"
}
```

Build and test:

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace -- -D warnings
```

The implementation plan (§14 milestones) lays out the road to feature parity:
streaming + tool loop (M1), core tools (M2), permissions (M3), TUI (M4 — the event
transport, the `rc-rt` driver/pump runtime, a minimal TUI, incremental markdown,
word-level `Edit` diff, and `@file`/`/slash` composer autocomplete), and on through
MCP, checkpoints, and polish.
