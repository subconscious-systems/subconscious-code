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

The report and trajectory only record the run. They do not change how the
agent behaves. Add `--completion-review` to inject one completion-audit note
after the first stop that follows tool work. Interactive runs never get it, so
leave it off to measure the agent as users run it.

The report intentionally excludes prompt and tool-result content. The ATIF
trajectory includes user-visible messages and tool activity, so treat it as
sensitive. External orchestrators should invoke this CLI contract rather than
requiring an adapter package from this repository.

Headless exit status is part of that contract: `0` means the agent loop ended
with a clean `stop`; `1` means it failed, exhausted a budget, stopped making
progress, was cancelled, or received an incomplete response. Partial stdout,
the final report, and the trajectory remain available on unsuccessful runs.
Startup or artifact-writing failures also return `1`; CLI argument errors
return `2`.

A clean `stop` measures runtime completion, not whether a coding task was
solved. Grade the resulting files and the task's acceptance tests independently.
For example, a run that correctly reports a permission denial can stop normally
without making the requested edit. Record resolved tasks separately from runtime
failures, along with the model, revision, budgets, and multiple trial results.

## Offline harness contracts

Exercise the real binary through a local scripted SSE provider:

```sh
cargo test --locked -p rc-cli --test harness_contract
```

These tests cover clean completion, iteration and time limits, repeated partial
answers, reasoning-only non-progress, content filtering, provider failure,
permission denial, and a read–edit–verify workflow. They inspect terminal
artifacts and the actual modified file instead of accepting an assistant's
claim of success. Each run uses isolated configuration and a dummy credential;
no live model API is used. The workspace CI jobs run this suite automatically.

This suite checks deterministic harness behavior. Live-model task success still
requires independently graded tasks under fixed model and resource budgets;
passing these contracts does not establish a coding benchmark score.

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
