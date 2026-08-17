# `sc` harness — trace review findings

Review of all 51 session traces in `~/.sc/sessions/` on `spark-39f8`, cross-checked
against the harness source in `~/subconscious-code`. Read-only analysis; findings are
ordered worst-first.

- **Traces reviewed:** 51 session files (4.5 MB), spanning 2026-07-31 → 2026-08-17
- **Models seen:** `gw-glm-5.2` (older sessions), `subconscious/glm-5.2` (2026-08-17 on)
- **Tool calls:** 384 total — Bash 183, Read 85, Edit 52, Write 52, Grep 9, Glob 3
- **Prompt tokens:** 10.4 M, of which 97% served from cache
- **Primary trace:** `session-f9b0ed50-9a09-4750-8baf-5fcb7fbdf1d2.jsonl` — 522 records,
  235 tool calls, 51 user messages, ~15 min of wall clock, `cwd=/home/daniel/epr`, `mode=auto`

---

## 1. A pasted document became 51 separate prompts, 50 of which were silently dropped

**Severity: critical — this destroys user input with no feedback.**

Session `f9b0ed50` was the "implement this entire following plan" run. What the trace shows:

- **User message 1** is `"implement this entire following plan # Expert Paging Runtime — Design & Implementation Plan"`
  — 91 chars, a single line. The ~100-line plan body never arrived.
- The agent spent **8 tool calls hunting for the missing plan**, reasoning verbatim:
  *"The plan content seems to be missing from your message — it got cut off after the title."*
  It recovered the document by `cat`-ing `~/.sc/history.txt`, where an earlier paste had
  left one line per history entry.
- It then worked for **891 seconds / 235 tool calls** off that recovered text, and was
  **interrupted mid-`Write`** (`{"kind":"interrupted"}`, target `crates/epr-layout/src/lib.rs`).
- **User messages 2–51 then arrive** — one plan line each (`**Target:** …`, `**Vehicle:** …`,
  `---`, `## 0. Thesis`, `| Total params | 284B | |`, …) at a metronomic **~6.1 s apart**,
  **all 50 with zero assistant responses.** The session ends there.

### Root cause

Two independent defects compound:

`crates/rc-tui/src/app.rs:512` — the `KeyCode::Enter` arm calls `submit_prompt(text)` with
**no `view.busy` guard**, and `grep -n "queue\|queued" crates/rc-tui/src/app.rs` returns
nothing: **there is no prompt queue at all.** Anything submitted while a turn is in flight
is accepted by the TUI, written to the session log, and then lost.

Paste handling itself is correct *when it fires* — `handle_paste` at `app.rs:619` and
`append_paste` at `view.rs:286` deliberately keep newlines inside the composer, and
`EnableBracketedPaste` is set at `lib.rs:64`. But if bracketed paste does not survive the
terminal path (plain SSH, a multiplexer without passthrough, a terminal with it disabled),
every newline arrives as `KeyCode::Enter` and the guard-less submit arm fires once per line.

### Fixes

1. **Guard `Enter` on `self.view.busy`** (`app.rs:512`). Queue the text and drain it as a
   single message when the turn completes, or append it to the composer. Never accept a
   submit you are going to discard.
2. **Heuristic paste rescue.** If N submits arrive within a few milliseconds, coalesce them
   into one multi-line prompt. Cheap insurance against terminals that don't forward `\e[200~`.
3. **Don't drop pastes silently.** `handle_paste` returns early when `menu_overlay.is_some()`
   or `pending_ask.is_some()` (`app.rs:622-624`). A discarded 10 KB design doc deserves a toast.

---

## 2. `exit: 0` is lying — 16% false-green rate on Bash

**Severity: critical — the agent cannot distinguish a passing build from a failing one.**

`crates/rc-tools/src/bash.rs:136` runs `sh -c <command>` with no `pipefail`, and
`bash.rs:199` formats the result as `exit: {code}`. The model's habit is
`cargo build 2>&1 | tail -40`, so the code reported is **`tail`'s**, not the build's.

**12 of the 77 `exit: 0` Bash results in session `f9b0ed50` contained a real failure.**
Instances from the trace:

| Reported | Actual content |
|---|---|
| `exit: 0` | `error: failed to load manifest for workspace member .../epr-trace` |
| `exit: 0` | `error: failed to parse manifest … can't find library epr_trace` |
| `exit: 0` | `error[E0308]: mismatched types` (from `cargo test -p epr-trace`) |
| `exit: 0` | `error: failed to download io-uring v0.6.4` |
| `exit: 0` | `error[E0599]: no method named resident_set` (from `cargo build -p epr-policy`) |
| `exit: 0` | `error: duplicate key --> Cargo.toml:7:2` |

### Fix

Run the command under `set -o pipefail` (or report `${PIPESTATUS[@]}`) in `bash.rs:136`.
This is the highest-value single line in this document — an agent that reads a red build as
green will confidently report success it hasn't achieved.

---

## 3. The output cap already works; the model doesn't know it

**Severity: high — causes both wasted calls and hidden errors.**

`cap_output(s, cap, head, tail)` at `crates/rc-tools/src/util.rs:113` already does head+tail
elision, and `bash_description` (`bash.rs:85-104`) advertises the exact limits. Yet in the
primary trace:

- the model piped to `head`/`tail` in **58 commands**, and
- `truncated: true` appears in **0 of 235 results** — the cap was never once hit.

So the defensive piping is pure loss. `cargo build | tail -40` discards the **first** compile
errors — the ones that matter — to preserve the summary line, and it is what breaks the exit
code in finding #2. In one stretch the agent fixed a single error at a time across three
`cargo test … | tail -30` round trips because it could only ever see the tail.

### Fix

State affirmatively in the Bash description that full output is safe, and that piping to
`head`/`tail` **hides errors and breaks the exit code**. Combined with `pipefail`, this class
of thrash disappears.

---

## 4. `Read` is sandboxed; `Bash` is not

**Severity: high — the containment guarantee is decorative, and it leaked history.**

Twice the agent was refused by `Read`:

```
Read  /home/daniel/minglang/Cargo.toml   → path outside allowed roots
Read  /home/daniel/.sc/history.txt       → path outside allowed roots
```

and **both times immediately succeeded with `Bash cat` on the same path.** The
`resolve_within` root check (`crates/rc-tools/src/read.rs:7`) buys nothing while Bash is
unrestricted; it only costs the model turns to discover the workaround.

Second-order consequence: this is how the agent read **`~/.sc/history.txt` — 27 KB, 313
lines of your entire cross-session input history**, including prompts from unrelated
sessions (`"can you delete the cursor editor"`, `"whats on this machine"`, …) straight into
context. It's what rescued the run, but it's an accidental channel, not a designed one.

### Fix

Pick one model and apply it to both tools. Either extend containment to Bash path arguments
(rc-perm already parses Bash — `crates/rc-perm/src/bash.rs`), or drop the check on `Read` so
the sandbox story is honest. A half-enforced boundary is worse than either.

---

## 5. Silent `cd` rejection cost 4 calls and an 8.5 KB reasoning block

**Severity: high — the tool description states the opposite of the behaviour.**

`bash.rs:193-194` deliberately lets an out-of-workspace `cd` run in the subshell but does
**not** persist it, **and says nothing**. Meanwhile `bash_description` (`bash.rs:97`) states
flatly: *"`cd` persists across calls (a successful, in-workspace `cd` updates the session's
working directory)."*

The trace sequence:

1. `cd /home/daniel/minglang && ls crates` → succeeds (subshell)
2. `cat crates/paged-kv/src/lib.rs | head -80` → `No such file or directory`
3. same command retried with a cosmetic change → same failure
4. same command retried again → same failure
5. `pwd && ls && cat …` → reveals cwd is still `/home/daniel/epr`
6. an **8,473-character** reasoning block relitigating whether the tool description is true

### Fix

Append a note to the result whenever an inferred `cd` was not persisted:

```
note: cd to /home/daniel/minglang was not persisted (outside workspace root);
      cwd remains /home/daniel/epr
```

`infer_cwd` (`bash.rs:405`) already computes the target, so the harness knows exactly when
this happens. Also soften the description: an *in-workspace* cd persists; one that leaves
the workspace does not.

---

## 6. `Write`'s read-before-mutate check fires on files the agent just created

**Severity: medium — 7 wasted three-call round trips.**

Seven identical sequences in the primary trace:

```
Write crates/epr-trace/src/lib.rs  → error: "… — read it with `Read` before mutating it"
Read  crates/epr-trace/src/lib.rs  → "//! stub"          (16 chars)
Write crates/epr-trace/src/lib.rs  → ok
```

Affected: `epr-trace/Cargo.toml`, `epr-trace/src/lib.rs`, `epr-io/src/lib.rs`,
`epr-policy/Cargo.toml`, `epr-policy/src/lib.rs`, `epr-predict/src/lib.rs`,
`epr-layout/src/lib.rs`. In every case **the agent itself had created the file moments
earlier** via a Bash heredoc loop, so the read registry (`util.rs:32 record_read`,
`util.rs:46 require_current_read`) had no entry.

The invariant is right — don't clobber content you haven't seen. The ergonomics aren't.

### Fix

Include the current file contents in the rejection message when the file is small. That
satisfies "the model has seen it" in **one** round trip instead of three. Optionally, have
Bash-created files register in the read registry.

---

## 7. `Grep` and `Glob` are effectively dead tools

**Severity: medium — design signal.**

| Tool | Calls | Share |
|---|---|---|
| Bash | 183 | 48% |
| Read | 85 | 22% |
| Edit | 52 | 14% |
| Write | 52 | 14% |
| Grep | 9 | 2% |
| Glob | 3 | 1% |

Most of those 183 Bash calls are `grep -rn` / `find` shellouts that `Grep`/`Glob` exist to
serve. Every one of them forfeits structured results, path scoping, and permission
granularity — and inherits the exit-code and truncation problems above.

Either make the `Grep`/`Glob` descriptions win the comparison against raw shell, or accept
that this is a Bash-shaped agent and invest the safety work there instead. The current split
gets the costs of both.

---

## 8. Failures are invisible in the traces

**Severity: medium — this is the finding that made finding #1 hard to diagnose.**

The 50 unanswered user messages produced **no record of why nothing happened**. Anything
that fails between submit and first token — transport error, context-length rejection,
cancellation, a dropped queue entry — leaves no trace record at all. The only failure
visible anywhere in 51 sessions is `{"kind":"error"}` inside a `tool_result`.

### Fix

Emit a `{"type":"error", …}` record (and a `{"type":"cancelled"}`) into the session JSONL.
The trace is your debugging surface for exactly this class of bug; right now it goes quiet
precisely when something breaks.

---

## 9. Session-file clutter

**Severity: low.**

- **12 of 51 session files contain only the header line** — zero exchanges.
- ~11 more contain a single `"hi"`.

`~/.sc/sessions` is 4.5 MB of mostly noise; locating the one substantive trace meant sorting
by file size. Don't persist a session file until the first exchange completes.

---

## 10. Answer depth in short sessions

**Severity: low — prompt-quality, not a defect.**

Six near-identical `mode=auto` sessions on 2026-08-17 asked "whats on this machine". The
answer listed:

```
- minglang — (?)
- nightshift — (?)
- tracer — (?)
```

One additional `ls` or `head README.md` would have cost ~200 ms against a 16 ms `ls`. Listing
a directory and shrugging at it isn't an answer; the system prompt should push one more cheap
call before emitting a `(?)`.

---

## What is working well

Worth protecting during any of the above changes:

- **Prompt caching — 97% hit rate across 10.4 M prompt tokens**, 98% on the long session.
- **Context eviction works.** Prompt tokens *dropped* mid-task (42,813 → 39,324 → 39,605)
  as superseded reads were evicted, exactly as `rc-ctx` intends.
- **`Edit` is reliable** — 52 calls, **zero** hard failures. Better than most harnesses manage.
- **Tool latency is not a problem.** Read p50 0 ms, Edit p50 13 ms, Write p50 13 ms,
  Grep p50 3 ms, Bash p50 4 ms. The only long pole is Bash p95 ≈ 3.0 s (cargo builds), with
  one 120 s timeout kill.
- **The model's own judgment was sound.** It correctly diagnosed the missing plan, located it,
  and produced a coherent 9-crate workspace with 12 passing tests — while working through a
  broken input path and a lying exit code.

---

## Recommended order of work

If only three things get done, do these — they are a few lines each, and they are the ones
that make the agent *wrong* rather than merely slow:

1. **`set -o pipefail`** in `crates/rc-tools/src/bash.rs:136` (finding #2)
2. **`busy` guard + prompt queue** on `crates/rc-tui/src/app.rs:512` (finding #1)
3. **Silent-`cd` note** at `crates/rc-tools/src/bash.rs:193` (finding #5)

Then, in descending value: #3 (Bash description), #4 (sandbox consistency), #6 (Write
ergonomics), #8 (error records), #7 (tool split), #9, #10.
