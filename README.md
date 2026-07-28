# rc

A terminal agent harness (a Claude Code–style agent) written in first-party
Rust, speaking the OpenAI-compatible `/v1/chat/completions` backend. Single
static binary; no Python, no Node.

## Status — M6 (context assembly)

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

Interactive sessions persist to `~/.rc/sessions/<id>.jsonl` (one JSON header line +
one JSON `Turn` per line, flushed after each turn). Resume with `--continue` (the
newest file) or `--resume <path>`; the loaded turns replay into the conversation
and new turns append to the same file. The headless `-p` path stays ephemeral.

Every model request now goes through `rc-ctx` context assembly (§4.6 / §8): the
`ContextAssembler` builds a real system prompt — identity, an environment block
(working directory, platform, today's date, git branch), and a hierarchical
`AGENTS.md` memory chain (`~/.rc/AGENTS.md` → `<cwd>/.rc/AGENTS.md` →
`<cwd>/AGENTS.md`, lowest precedence first) — expands `@file` mentions in the last
user turn into fenced file blocks, and head-truncates oversized tool-result bodies
(the microcompaction seam). A calibrated `Estimator` (`rc-tokenize`) tracks the
chars-per-token factor from authoritative `usage` and applies a 10% compaction
margin. The loop stays backward-compatible: with no assembler wired in it falls
back to the legacy flat `project` path.

```sh
export RC_API_KEY=...
cargo run -q --bin rc              # interactive TUI (persists to ~/.rc/sessions)
cargo run -q --bin rc --continue   # resume the most recent session
cargo run -q --bin rc -p "say hi"  # headless one-shot (no persistence)
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
word-level `Edit` diff, and `@file`/`/slash` composer autocomplete), session
persistence + resume (M5), context assembly (M6 — this slice: the §4.6 system
prompt, environment block, hierarchical `AGENTS.md` memory, `@file` mention
expansion, tool-output truncation, and a calibrated token estimator), and on
through MCP, checkpoints, and polish.
