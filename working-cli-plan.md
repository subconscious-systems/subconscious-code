# `sc` — Subconscious Code: working-CLI plan

**Status: implemented.** Every code phase below is done, committed, and green
(243 tests, clippy clean at `-D warnings`). What remains is what only you can do:
run it against the real gateway on the Linux box. Phase 0 and Phase 5 are the
live-verification steps, and `sc --doctor` now automates most of Phase 0.

## 0. Product identity — done

| Field | Value |
| --- | --- |
| Binary / command | `sc` |
| Product name | Subconscious Code |
| Default base URL | `https://api-dev.subconscious.dev/v1` |
| Default model | `subconscious/glm-5.2` |
| API key | `$SC_API_KEY` (user-supplied; never in the repo) |
| Config dir | `~/.sc/` (settings, sessions, memory, bg logs) |
| Project dir | `./.sc/` |
| Env prefix | `SC_*` |

Crate names stay `rc-*` (Phase 6 — cosmetic, deliberately not done).

---

## Phase 0 — Provider bring-up → run `sc --doctor` on the box

This is now one command instead of four curl invocations:

```sh
export SC_API_KEY=...
sc --doctor                 # config + non-streaming + streaming + tool calling
sc --doctor --body-ladder   # also measures the maximum request size
```

`sc --doctor` reports:

- the fully resolved config, with every cap spelled out (a silent 16 KB
  tool-result cap is exactly what this is meant to catch);
- **non-streaming** — the endpoint speaks `/chat/completions`;
- **streaming** — SSE works, first-event latency, and whether
  `stream_options.include_usage` is honored (a warning if not: metering and
  estimator calibration degrade, nothing breaks);
- **tool calling** — the model actually emits `tool_calls`. A hard failure,
  because the agent loop is built on it;
- **`--body-ladder`** — uploads 1 / 10 / 32 / 100 / 500 MB bodies until one is
  refused, then names the likely culprit.

Exits non-zero if any check fails, so it works in a script.

Verified against a local fake gateway (all four checks pass; the ladder
correctly detects a synthetic 32 MB limit and reports HTTP 413). **Not yet run
against the real endpoint** — that's the first thing to do on the Linux box.

---

## Phase 1 — Rebrand `rc` → `sc` — done

- `crates/rc-config/src/lib.rs` — new defaults (`awsgateway.orangelinelabs.com`,
  `gw-glm-5.2`), `SC_API_KEY`, and all 16 `SC_*` env vars
- `crates/rc-cli/Cargo.toml` — `[[bin]] name = "sc"`
- `crates/rc-cli/src/main.rs` — clap name/about, `~/.sc/sessions`, `~/.sc/bg`
- `crates/rc-ctx/src/lib.rs` — memory chain is `~/.sc/AGENTS.md` →
  `<cwd>/.sc/AGENTS.md` → `<cwd>/AGENTS.md`
- Identity is now ``You are `sc` (Subconscious Code)…`` in both places it
  lives: `crates/rc-ctx/src/lib.rs` (`IDENTITY`, the live assembler path) and
  `crates/rc-core/src/project.rs` (`SYSTEM_PROMPT`, the legacy no-assembler
  fallback). Verified on the wire, not just in source.
- `crates/rc-tools/src/bash.rs` — env hygiene scrubs `SC_API_KEY` and marks
  child shells with `SC_SESSION=1`. (The key was never actually exposed: the
  generic `*_API_KEY` sweep already covered it.)
- Module-header docs in `bash.rs` / `grep.rs` / `glob.rs` / `read.rs` /
  `rc-ctx` no longer advertise the removed caps.
- `crates/rc-proto/src/error.rs` — `NoApiKey` names `$SC_API_KEY`
- `README.md` — rewritten around `sc` and the unlimited-context thesis

**Bug found while verifying:** `--continue` never existed. The clap field
`continue_last` derived `--continue-last`, so the documented flag failed with a
usage error. Now `#[arg(long = "continue", alias = "continue-last")]` — both work.

---

## Phase 2 — No context-window limit — done

There was no window enforcement to remove: `rc-tokenize` was a declared
dependency of `rc-cli` that no Rust file referenced, so the `Estimator` and its
`MARGIN` were unreachable. The work was lifting the per-item caps.

### Caps, all configurable, all unlimited by default

New `[context]` block in `Settings` (`crates/rc-config/src/lib.rs`), with an
`SC_*` env var per field. **`0` means unlimited everywhere**:

| Setting | Default | Was | Enforced in |
| --- | --- | --- | --- |
| `tool_result_cap` | unlimited | 16 KB | `rc-ctx` `Caps::tool_result` |
| `inline_file_cap` | unlimited | 8 KB | `rc-ctx` `Caps::inline_file` |
| `bash_output_cap` | unlimited | 30 KB | `Bash::with_cap` |
| `grep_output_cap` | unlimited | 30 KB | `Grep::with_cap` |
| `read_default_limit` | whole file | 2000 lines | `Read::with_limits` |
| `read_max_line_chars` | untruncated | 2000 chars | `Read::with_limits` |
| `glob_cap` | all matches | 1000 paths | `Glob::with_cap` |
| `max_iters` | 1000 | 100 | `AgentLoop::with_max_iters` |

Details worth knowing:

- `rc_ctx::Caps` has `unlimited()` (the `Default`) and `bounded()` — the
  historical 8 KB/16 KB preset, kept for small-context models and used by the
  tests that pin truncation behaviour.
- `cap_output` short-circuits on `cap == 0` *before* counting chars, so the
  unlimited path doesn't walk the string.
- `Grep`'s early-exit (`out.len() > CAP + 4096`) is skipped when unlimited —
  otherwise it would have stopped scanning at 34 KB regardless.
- **Tool descriptions are generated from the cap in force.** `Bash`, `Read`, and
  `Glob` build their description at construction, so the model is never told
  output is "capped at 30k chars" when it isn't. Advertising a phantom limit
  changes how a model uses a tool.

### Estimator wired for observability only

`rc-core` now depends on `rc-tokenize` and holds an `Estimator`. Per iteration it
sums the assembled context's char length, converts via the calibrated factor
(new `Estimator::estimate_chars`, which avoids re-walking a huge context), and
reports it through a new `EventSink::on_context`. After each response,
`usage.prompt_tokens` calibrates the factor.

It gates nothing — no threshold, no compaction trigger. The TUI status bar shows
`ctx: 12.1M (~2.7M tok)`; headless prints a peak-context line to stderr.

---

## Phase 3 — No request-size cap — done

### The body was copied four times; now once

`canonical::to_string` built a `serde_json::Value` tree, a canonicalized copy of
that tree, and then a `String` — then `send_with_retry` did `body.to_string()`
*per attempt*. New `canonical::to_bytes` serializes straight to `Vec<u8>`, which
`Bytes` adopts without copying, and retries clone a refcount.

Byte-stability (what §4.6 actually needs for prefix caching) is preserved by two
properties, both now pinned by tests:

- struct fields serialize in declaration order, fixed at compile time;
- `serde_json::Map` is a `BTreeMap` — `preserve_order` is not enabled — so
  dynamic maps (tool schemas, tool-call arguments) sort their keys regardless of
  insertion order. `to_bytes_sorts_dynamic_map_keys` fails loudly if anyone
  enables that feature.

The wire bytes differ from the old alphabetized form; they are equally stable.
`chat_client.rs` was updated to assert stability and compactness rather than
alphabetical order.

### Measured result, stated honestly

A 12 MB tool result, end-to-end through the real binary:

- request body on the wire: **12,085,642 bytes** — the whole file, uncut. Under
  the old 16 KB cap this was ~17 KB.
- peak RSS: **86.7 MB**, against a **15.2 MB** baseline → **~6× the payload.**

So the earlier "~1× peak memory" framing was wrong, and the code comments and
README have been corrected. Serialization is now one copy of that total instead
of four; the remaining multiple was the clone chain in context assembly
(`prepare_turns` clones the turn list → `project_with` clones into
`WireMessage` → `to_bytes`).

**That clone chain is now gone.** The large string fields — `Turn::User.content`,
`Turn::Assistant.text`/`reasoning`, `ToolCall.arguments`, `ToolResultBody::Ok.content`,
and the matching `WireMessage`/`UserContent`/`FunctionCall` fields — are `Arc<str>`.
The response text and tool-call arguments are wrapped in `Arc` once at the source
(agent loop / stream consumer); projecting a `Turn` into a `WireMessage` (per
request) and re-projecting the same turns on a later request are refcount bumps,
not deep copies. The one remaining copy is the single `to_bytes` serialization
into the request buffer, which is inherent. Pinned by
`projection_shares_body_allocations_via_arc` (`rc-core`): `Arc::ptr_eq` holds from
the session turn through both projections — a regression to `.to_string()`
anywhere on the path makes it fail.

The **~6× measured multiple was taken before this change** and has not been
re-measured (it needs the Linux box and a real ≥12 MB body). Expect it to drop
— the assembly clones were the bulk of the multiple — but the new number is
yours to record with `sc --doctor --body-ladder` and a scale run. Until then,
size the box to the old figure.

### Also done

- **No total request timeout by default.** `ChatClient::new` takes
  `Option<Duration>`; `timeout_ms` defaults to `0` = off. A total budget covers
  the upload, so on a large body it expires mid-upload and triggers a retry that
  re-uploads from scratch. Liveness is the idle bound instead
  (`idle_timeout_ms`, now defaulting to 120 s), which distinguishes a stalled
  stream from a merely large one.
- **`--debug` no longer dumps the whole body.** It logs 8 KB plus a byte count,
  sliced on a UTF-8 boundary. `SC_DEBUG_FULL_BODY=1` restores the full dump.
  Logging a 200 MB conversation per request is its own outage.
- **Optional request gzip** via `SC_REQUEST_GZIP=1` /
  `provider.request_gzip` — `flate2`, one pass, `Content-Encoding: gzip`. Off by
  default: a gateway that ignores the header will try to parse compressed bytes
  as JSON. JSON-wrapped source compresses 5-10×, so confirm support and turn it
  on.
- `max_retries` now defaults to 2 (was 0) — transient 429/5xx are normal, and
  retrying is cheap now that the body is refcounted.

### The gateway ceiling — still yours to measure

`sc --doctor --body-ladder` does this in one command. The interpretation is baked
in, including the case that would sink the whole thesis:

- **exactly 10 MB** → AWS API Gateway's payload limit, which *cannot be raised*.
  On that route we'd be more limited than Claude Code's 32 MB, and the fix is
  infrastructure (ALB or direct-to-origin), not client code.
- **1 MB** → usually nginx's default `client_max_body_size`, raisable.
- **≥ 32 MB** → clears Claude Code's cap; the client imposes nothing.

---

## Phase 4 — Demo papercuts — done

1. **No-TTY failure** now reads `sc needs a terminal for the interactive TUI. For
   non-interactive use, run a one-shot: sc -p "<prompt>"` instead of
   `Device not configured (os error 6)`.
2. **Orphan session files** fixed on both sides: the `SessionStore` is created
   *after* the terminal check, so a failed startup leaves nothing behind; and
   `rc_session::latest` now skips header-only files, so `--continue` can't pick
   an empty session and silently resume nothing.
3. **Install** — `cargo install --path crates/rc-cli` installs `sc`.
4. **Committed** — the 38-file M7 tree landed as one commit before the rename, so
   the rename diff stays reviewable.

---

## Phase 5 — Verification

Done here:

- `cargo test --workspace` — **243 passed, 0 failed**
- `cargo clippy --workspace --all-targets` — zero warnings
- **Linux type-check.** All OS-gated code lives in `rc-sandbox`, and it
  type-checks for Linux including its Linux-only tests:
  `cargo check -p rc-sandbox --target x86_64-unknown-linux-gnu --all-targets`.
  The rest of the workspace can't be cross-checked from macOS without a C
  cross-toolchain (`ring` needs `x86_64-linux-gnu-gcc`), but nothing else is
  OS-gated. This matters because `linux.rs` had never been compiled before.
- **End-to-end against a fake gateway**: streaming, fragmented tool-call
  reassembly, tool execution, second turn, final answer, usage metering, a
  12 MB uncapped tool result on the wire, and `sc --doctor` including the ladder.
- **New regression tests**: uncapped tool results reach the wire
  (`rc-core/tests/agent_loop.rs`), caps default to unlimited and round-trip from
  JSON (`rc-config`, `rc-ctx`), `Read` returns whole files, `to_bytes` stability
  and BTreeMap ordering (`rc-proto`), `latest` skips orphans (`rc-session`),
  context size renders in the status bar (`rc-tui`), and `Arc<str>` sharing
  through the assembly path (`projection_shares_body_allocations_via_arc` in
  `rc-core`).

Also verified end-to-end against the fake gateway: `Write` creates files and
`Bash` executes (`exit: 0 / hello-from-bash`) once granted, a project-level
`./.sc/settings.json` is picked up, and the generated tool descriptions correctly
advertise "the whole file" / "no size limit" when caps are off.

One behaviour to know before testing: **headless runs fail closed.** With no TTY
there is nobody to answer an Ask, so `sc -p "fix X"` denies every `Write`/`Edit`/
`Bash` and the model just gets denied tool results. Grant rules in
`./.sc/settings.json` (`"allow": ["Write", "Edit", "Bash(cargo:*)"]`) or pass
`--dangerously-skip-permissions`. The TUI asks interactively, so it is unaffected.

### Verified on the box (2026-07-31, spark-39f8, real Linux kernel)

Driven through `tmux send-keys` against a local fake chat-completions SSE
gateway (Python, proper chunked framing) — `script`-fed pty stdin was the wrong
tool; `tmux` delivers keystrokes reliably:

- **Interactive TUI, end to end.** Launches and renders in a real pty;
  keystrokes land; composer submits on Enter; **streaming response renders**
  live; **tool call** (`-> Bash …`) + **permission prompt** (`[y]once [s]ession
  [a]lways [n]o`) + grant + **tool result** (`<- Bash: exit: 0 …`) + **second
  turn** final answer; usage metering and the **context estimator** both render
  in the status bar (`ctx: 598 (~150 tok)`).
- **Slash commands** (`/help`, `/clear`, `/mode`, `/rewind`) execute — but note
  the wart: `refresh_menu` reopens the menu on an exact match, so a single Enter
  re-accepts instead of submitting; the user must press `Esc` then `Enter`.
- **`@file` completion** popup and **`Shift+Tab`** mode cycle
  (default → acceptEdits → plan → bypassPermissions) both work.
- **Session persistence + `--continue`**: a TUI session is written to
  `~/.sc/sessions/*.jsonl`; `sc --continue --debug -p …` loads the full prior
  conversation (user → tool call → tool result → assistant) into the assembled
  context. (A headless `-p` run is deliberately ephemeral — no session is
  written, so `--continue` after `-p` correctly finds nothing.)
- **`--sandbox` on a real kernel.** `sc --sandbox --dangerously-skip-permissions
  -p …` ran: Landlock+seccomp initialized, the process survived, outbound HTTP
  to the gateway worked, and the `Bash` tool actually `fork`/`execve`'d under
  the filter (`exit: 0 / hello-from-bash`). This was risk #5 ("never run").
- **Headless one-shot** end to end against the fake gateway: streaming,
  metering, peak-context line.

Bugs found and fixed while testing on the box:

- **Sandbox `NO_NEW_PRIVS` ordering.** It was set *between* `landlock_restrict_self`
  and `seccomp`, so `landlock_restrict_self` returned `EPERM` on an unprivileged
  caller — the sandbox was completely non-functional (every install path
  failed). Moved to *before* both (`crates/rc-sandbox/src/linux.rs`). The
  runtime test above is the first time this code has ever run.
- **TUI header said `rc`, not `sc`.** The status-line banner still showed the
  old binary name; fixed (`crates/rc-tui/src/app.rs`).

Still needs the real gateway (`SC_API_KEY`) — cannot be done from macOS and
not yet done on the box:

1. `sc --doctor` and `sc --doctor --body-ladder` against the real endpoint — the
   make-or-break body-ceiling measurement.
2. `sc -p "…"` — a real one-shot against `subconscious/glm-5.2`.
3. **The scale test.** Read several MB of real files into a session and record
   (a) peak RSS vs context size — **the `Arc<str>` change should bring the old
   ~6× multiple down; re-measure it**, (b) time-to-first-token vs body size,
   (c) behavior at the gateway ceiling. Those numbers are the demo.

---

## Phase 6 — Rename crates `rc-*` → `sc-*` — deliberately not done

Cosmetic, ~66 files, gates nothing, and it would bury the substantive diff. The
binary, config dirs, env vars, and identity are all `sc`; only internal crate
names still say `rc`. Worth one mechanical commit later.

---

## Open questions

1. **Small model.** `small_model` defaults to `subconscious/glm-5.2` (same as the main
   model) since no separate small model was specified. Nothing reads it yet.
2. **Gateway body ceiling** — `sc --doctor --body-ladder` answers this in a minute.
   It's the one result that could invalidate the thesis on this route.
3. **`stream_options.include_usage`** — doctor warns if unsupported; only
   metering and estimator calibration degrade.
4. **"Our strategies" for huge context.** This plan removed the artificial
   limits and made the transport handle the volume. If there are specific
   retrieval or compaction strategies intended beyond "send everything," they
   attach at `rc_ctx::truncate_tool_results` — the §8.5 microcompaction seam,
   still present and now a no-op by default.

## Risks

- **Highest: the gateway is AWS API Gateway.** Its 10 MB payload limit cannot be
  raised, and that would cap us *below* Claude Code's 32 MB. Measure first.
- **Memory at ~6× the payload (pre-`Arc` measurement).** Fine at 12 MB, ~3 GB at
  500 MB. The assembly clone chain is now eliminated (`Arc<str>` through the
  path; see Phase 3), so the real multiple should be lower — but it has not been
  re-measured. RAM is still the practical ceiling until you record the new
  figure on the Linux box.
- **Unlimited output is genuinely unlimited.** A runaway `Bash` can pour
  gigabytes into the context. `max_iters` (1000) and the turn timeout are the
  remaining backstops — keep them on.
- **Provider-side context limits still apply.** No client cap doesn't mean the
  model accepts 100 MB of tokens; confirm with the scale test before demoing it.

## Quick start on the Linux box

```sh
git clone <repo> && cd subconscious-code
cargo install --path crates/rc-cli
export SC_API_KEY=...

sc --doctor --body-ladder      # verify the endpoint and find the real ceiling
sc -p "summarize src/"       # headless smoke test
sc                           # the TUI
```
