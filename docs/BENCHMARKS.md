# Benchmarks

The repository includes reproducible measurement instructions and native CLI
output for evaluation harnesses. Generated results, trial summaries, and
trajectories are not part of the source distribution and should remain in
external artifact storage or an ignored local output directory.

## CLI benchmark output

Run the `marathon` binary directly and supply its credential through `SC_API_KEY`.
For a headless task, `--benchmark-report` writes privacy-safe accounting data
and `--benchmark-trajectory` writes an ATIF v1.7 transcript:

```sh
SC_API_KEY="your-api-key" marathon \
  --benchmark-report benchmark-results/report.json \
  --benchmark-trajectory benchmark-results/trajectory.json \
  -p "fix the task"
```

The report intentionally excludes prompt and tool-result content. The ATIF
trajectory includes user-visible messages and tool activity, so treat it as
sensitive. External orchestrators should invoke this CLI contract rather than
requiring an adapter package from this repository.

## DLR and TTFT measurements

[`integrations/dlr`](../integrations/dlr/README.md) contains protocol tests,
microbenchmarks, a network harness, and the sidecar. The TTFT example compares
ordinary JSON with DLR against the same immediate-SSE upstream, isolating
transport cost from model queueing and generation:

```sh
SC_TTFT_JSON_URL=http://gateway.test/v1 \
SC_TTFT_DLR_URL=http://sidecar.test:32180 \
SC_TTFT_DLR_TOKEN="$DLR_INGRESS_TOKEN" \
SC_TTFT_SIZES_MIB=1,10,25,45 \
SC_TTFT_REPEATS=3 \
cargo run --release -p rc-proto --example dlr_ttft
```

Use multiple repetitions, report p50 and p95, and keep client, gateway,
sidecar, and upstream placement fixed. A synthetic immediate-SSE upstream
measures transport overhead; a real model endpoint measures end-to-end TTFT
and includes queueing, prefill, and cache effects.

## Result handling

Write local outputs beneath `benchmark-results/` or `trial-results/`; both are
ignored by Git. Store durable evaluation evidence in the benchmark system's
artifact store rather than this source repository. A trace may contain model
output and task content, so review and sanitize it before sharing it anywhere.
Never include credentials, private source, customer prompts, or unredacted
session files.

When sharing a comparison, record the commit SHA, model, endpoint region,
client region, concurrency, repetitions, corpus characteristics, and whether
caches were warm. Those details are required for a useful result.

## Pinned real-task development suite

`evaluations/tasks.json` defines five real rc-tools bugs from issues #25, #27,
#29, #30, and #33, all starting at source commit
`7445d70454ff68536b68f8c7444d9dd376e498ff`. This small development suite exercises
known defects; it is not a held-out general coding benchmark. It is intended to
catch harness regressions before expanding to unfamiliar projects and tasks.

The optional evaluator uses Python 3.12+, Git, and the Rust toolchain. It invokes
the public `marathon` CLI contract; the product binary needs no Python runtime or
adapter dependency. Source snapshots live in temporary directories outside the
checkout, without Git history, evaluation assets, or inherited project ignore
rules. Agent configuration is isolated. Only Rust source and tests in rc-tools
may change; manifests, lockfiles, and settings are checked for tampering.

Acceptance tests are installed after the agent exits. Each case must pass its
goal test and three independent checks of existing behavior. A normal agent
exit is recorded separately from patch correctness. Candidate source, bounded
redacted logs, accounting, per-trial JSON, and an incremental trial journal are
kept under an ignored artifact directory.

List the cases, then calibrate the graders without calling any model:

```sh
python evaluations/run.py list
cargo fetch --locked
python evaluations/run.py validate --out benchmark-results/grader-controls
```

The checkout must contain the pinned source commit; fetch full Git history when
using a shallow checkout. Validation uses committed, SHA-256-pinned local oracle
patches, so it does not depend on a contributor's fork or future PR branches.
Each unmodified snapshot must pass the existing-behavior checks and fail its
goal; the known repair must pass both. Setup failures and missing/ignored test
receipts are grading errors, not evidence of model failure or success. CI runs
these controls on Linux and the Python runner tests on Linux and Windows.

For a live pilot, set `SC_API_KEY` in the environment, build the binary, and
supply an explicit model ID and API URL:

```sh
cargo build --locked --release --bin marathon
python evaluations/run.py run --binary target/release/marathon \
  --model YOUR_MODEL_ID --base-url https://your-provider.example/v1 \
  --trials 1 --max-tokens 2048 --max-iters 8 --seconds 180
```

On Windows use `target/release/marathon.exe`. The output directory must be new;
omit `--out` to create a unique directory automatically. Use `--task ID` to select
a smaller pilot. Increase `--trials` to at least three for repeated measurements;
task order rotates deterministically between trials. API keys are accepted only
through the environment, omitted from provenance, and redacted from captured logs.

Budgets are explicit: `--max-tokens` is the completion cap per model request;
`--max-iters` limits rounds; `--seconds` is the turn limit. A separate process
deadline allows ten seconds for final artifacts, and `--grader-seconds` limits
each compiler/test invocation. Input and output token usage are reported when
the provider supplies them. This is not a hard aggregate-input-token or dollar
budget. Compiler warm-up is outside agent timing; evaluation runs are serial.
Owned process groups or Windows jobs are cleaned up before grading begins.

`summary.json` separates resolved patches, clean runtime finishes, invalid
patches, and grading errors. A solve rate is published only for a complete run;
partial results remain available in the journal. Evaluator exit `0` means the
evaluation completed, including normally graded unresolved tasks; exit `1`
means a setup, control, or grading error. Scripted-provider tests validate the
orchestration only and must not be reported as live-model success rates.
