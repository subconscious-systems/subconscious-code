# rc

A terminal agent harness (a Claude Code–style agent) written in first-party
Rust, speaking the OpenAI-compatible `/v1/chat/completions` backend. Single
static binary; no Python, no Node.

## Status — M3 (permissions)

Headless agent loop (`-p`) with `Read`/`Write`/`Edit`/`Glob`/`Grep`/`Bash`, behind a
deny→allow→ask permission engine (parsed Bash matching, four modes, path containment,
a stdin prompt; `--dangerously-skip-permissions` for unattended runs) — against any
OpenAI-compatible endpoint:

```sh
export RC_API_KEY=...
cargo run -q --bin rc -p "say hi"
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
```

The implementation plan (§14 milestones) lays out the road to feature parity:
streaming + tool loop (M1), core tools (M2), permissions (M3), TUI (M4), and
on through MCP, checkpoints, and polish.
