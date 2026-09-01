# Benchmarks

The repository includes benchmark adapters and reproducible measurement
instructions. Generated results, trial summaries, and trajectories are not
part of the source distribution and should remain in external artifact storage
or an ignored local output directory.

## Harbor integration

[`integrations/harbor`](../integrations/harbor/README.md) adapts `sc` to Harbor
and documents the exact setup for SWE-bench-style evaluations. The adapter
keeps benchmark orchestration outside the agent runtime.

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
