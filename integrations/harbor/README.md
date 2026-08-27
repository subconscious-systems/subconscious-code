# Subconscious Code for Harbor

This package makes the `sc` coding-agent harness a native Harbor agent. It is
compatible with Harbor 0.13.x and is designed for Terminal-Bench, SWE-bench,
SWE-bench Pro, and other Harbor datasets.

The adapter runs `sc` in headless auto-permission mode, leaves repository edits
in the task container for the verifier, writes an ATIF v1.7 `trajectory.json`
for Harbor's trajectory viewer, and records:

- total, cached-input, and output tokens;
- cost when `sc` has pricing configured;
- wall/model/tool time, request count, retry count, and per-request TTFT;
- tool-call, tool-error, and tool-denial counts.

Prompts, reasoning, tool arguments, and tool output are intentionally excluded
from the machine-readable performance report. The separate ATIF trajectory
contains user-visible messages and tool activity for replay and inspection,
while hidden model reasoning remains omitted.

The benchmark defaults are hermetic: Bash confinement is enabled and network
syscalls from tools are disabled unless explicitly overridden. Remote binary
URLs require a SHA-256 checksum. During long model/tool phases `sc` prints a
10-second heartbeat to the captured agent log, and it journals completed turns
incrementally before atomically publishing report/trajectory checkpoints. Each
run deletes the exact prior output paths first, stamps a unique run id, hashes
the installed binary, and rejects a report whose provenance does not match that
run. Release archives and checksum manifests are keylessly signed with Sigstore;
the accompanying `*.sigstore.json` bundle includes the certificate, signature,
and transparency-log proof.

`TimeUp`, `MaxIterations`, `Incomplete`, and other valid harness outcomes are
reported as completed agent runs so Harbor can still invoke the task verifier;
the report records the underlying `sc` process status separately as
`harness_process_exit_code`. Installation failures, missing reports, malformed
reports, and actual process crashes remain non-zero agent failures.

## Install on the benchmark platform

From the `bench-runner` repository:

```bash
uv add --editable ../subconscious-code/integrations/harbor
```

For a smoke run, copy
[`example-job.yaml`](example-job.yaml) into `bench-runner/jobs/`, replace the
absolute source-bundle path, then run it with `uv run bench run <job-path>`.

Then use the import path `subconscious_harbor.agent:SubconsciousCode` in a Harbor
job:

```yaml
agents:
  - import_path: subconscious_harbor.agent:SubconsciousCode
    model_name: openai/subconscious/glm-5.2
    env:
      OPENAI_API_BASE: https://api.subconscious.dev/v1
      OPENAI_API_KEY: ${SUBCONSCIOUS_API_KEY}
    kwargs:
      # Fast path: use a release artifact. Pin the hash for reproducibility.
      binary_url: https://github.com/subconscious-systems/subconscious-code/releases/download/v0.1.0/sc-x86_64-unknown-linux-musl.tar.gz
      binary_sha256: REPLACE_WITH_RELEASE_SHA256
      # The adapter defaults to 8192. Override only for a model that needs a
      # larger single-response reasoning allowance; repeated no-progress
      # completions stop after one harness recovery.
      max_tokens: 8192
      # Finish before Harbor's 600-second outer kill so the report is complete.
      turn_timeout_ms: 540000
      max_iters: 1000
      max_retries: 2
      request_gzip: false
```

For an immediate run from an unpushed worktree, build the package and point the
job at its exact source bundle:

```bash
integrations/harbor/build-package.sh
```

```yaml
    kwargs:
      # Fastest local/cloud path: upload an already-built static binary.
      binary_path: /absolute/path/to/sc
```

Or upload the exact source bundle and build it during agent setup:

```yaml
    kwargs:
      source_archive: /absolute/path/to/subconscious-code/integrations/harbor/dist/subconscious-code-source.tar.gz
```

The adapter uploads that bundle into each task sandbox and builds it there.
Harbor records this as agent setup time, separate from agent execution time.
`build-package.sh` also emits `MANIFEST.json` with the byte length and SHA-256
of every local package artifact.

To verify a release archive, install Cosign and bind verification to this
repository's release workflow identity:

```bash
cosign verify-blob sc-x86_64-unknown-linux-musl.tar.gz \
  --bundle sc-x86_64-unknown-linux-musl.tar.gz.sigstore.json \
  --certificate-identity-regexp \
    'https://github.com/subconscious-systems/subconscious-code/.github/workflows/release.yml@refs/tags/v.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

If `binary_url` is omitted, setup builds the requested git revision. Pin it with
`kwargs.revision`; otherwise the adapter uses `v<agent version>` when a Harbor
agent version is configured, then falls back to `main`. If `sc` is already in
the task image, it is reused and setup skips both paths.

For accurate serving TTFT/decode/cache metrics, point `OPENAI_API_BASE` at the
benchmark platform's timing proxy exactly as other OpenAI-compatible agents do.
The adapter translates either `OPENAI_API_BASE` or `OPENAI_BASE_URL` to
`SC_BASE_URL`.

## Build and test the package

```bash
integrations/harbor/build-package.sh
PYTHONPATH=integrations/harbor/src uv run --project integrations/harbor pytest
```

`sc --benchmark-report <path> --print <prompt>` is the versioned interface
between the Rust harness and this adapter. The JSON schema starts at version 1.
