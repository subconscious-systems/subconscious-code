# Subconscious Code for Harbor

This package provides native Harbor adapters for the `sc` coding-agent harness
and mini-swe-agent. It is compatible with Harbor 0.13.x and is designed for
Terminal-Bench, SWE-bench, SWE-bench Pro, and other Harbor datasets.

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

The benchmark defaults are fail-closed and share the same offline runtime for
both agents. Setup rejects public or unknown sandbox networking, GitHub and
package-registry hosts are forbidden even in an allowlist, Git remote protocols
are disabled, and package managers are forced into offline mode. The only
permitted network hosts are explicitly declared model/metrics control-plane
endpoints. `sc` additionally denies network syscalls from Bash tools. During
long model/tool phases `sc` prints a
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

## Build the shared offline bundle on EC2

Use a clean EC2 staging host with the same Linux architecture and Python version
as the E2B task images. Network access is allowed only during this staging step.
Populate a directory with pinned agent artifacts and dependency caches; do not
put task repositories, upstream checkouts, patches, issues, or Git metadata in
it. A minimal bundle for both agents looks like this:

```text
offline-payload/
├── agents/sc
└── python/wheels/
    ├── mini_swe_agent-<pinned version>-py3-none-any.whl
    └── <the complete transitive wheel closure>
```

For example, download the pinned artifacts on EC2, not in E2B:

```bash
mkdir -p offline-payload/agents offline-payload/python/wheels

# Copy a pinned, checksummed sc release binary into this path after downloading
# it once from GitHub on the staging host.
install -m 0755 /path/to/verified/sc offline-payload/agents/sc

# Download mini-swe-agent and its complete dependency closure once. Pin the
# version used by the benchmark job.
python3 -m pip download --only-binary=:all: \
  --dest offline-payload/python/wheels 'mini-swe-agent==<version>'

python3 integrations/harbor/build-offline-bundle.py \
  offline-payload /srv/bench-artifacts/deepswe-offline.tar.gz \
  --provenance integrations/harbor/offline-provenance.example.json
```

The builder produces a deterministic gzip archive, prints its SHA-256, records
the size and SHA-256 of every member, allows only agent/package-cache roots, and
rejects symlinks and `.git` directories. Add task dependency wheels/caches only
when a benchmark task genuinely needs them; prefer the dependencies already
pinned and installed in the task image. Git-based build dependencies should be
vendored without Git metadata before bundling.

Harbor can upload the archive from EC2 into each E2B sandbox with
`offline_bundle`. For less transfer overhead, bake the exact archive into the
E2B template and use `offline_bundle_remote_path` plus the required
`offline_bundle_sha256`. Both paths verify the outer archive hash and every
manifest entry before use.

## Install on the benchmark platform

From the `bench-runner` repository:

```bash
uv add --editable ../subconscious-code/integrations/harbor
```

For a smoke run, copy [`example-job.yaml`](example-job.yaml) into
`bench-runner/jobs/`, replace the bundle path and checksum, then run it with
`uv run bench run <job-path>`.

Use the same artifact in both agent definitions. The sandbox must use
`network_mode: allowlist` with only the model/metrics endpoints, or
`network_mode: no-network` when the model proxy is outside the sandbox network
namespace:

```yaml
environment:
  type: e2b
  network_mode: allowlist
  allowed_hosts:
    - api.subconscious.dev

agents:
  - import_path: subconscious_harbor.agent:SubconsciousCode
    model_name: openai/subconscious/glm-5.2
    env:
      OPENAI_API_BASE: https://api.subconscious.dev/v1
      OPENAI_API_KEY: ${SUBCONSCIOUS_API_KEY}
    kwargs:
      offline_bundle: /srv/bench-artifacts/deepswe-offline.tar.gz
      offline_bundle_sha256: REPLACE_WITH_BUNDLE_SHA256
      offline_control_plane_hosts: [api.subconscious.dev]
      # The adapter defaults to 8192. Override only for a model that needs a
      # larger single-response reasoning allowance; repeated no-progress
      # completions stop after one harness recovery.
      max_tokens: 8192
      # Finish before Harbor's 600-second outer kill so the report is complete.
      turn_timeout_ms: 540000
      max_iters: 1000
      max_retries: 2
      request_gzip: false
  - import_path: subconscious_harbor.mini_swe_agent:OfflineMiniSweAgent
    model_name: openai/subconscious/glm-5.2
    version: REPLACE_WITH_PINNED_MINI_SWE_VERSION
    env:
      OPENAI_API_BASE: https://api.subconscious.dev/v1
      OPENAI_API_KEY: ${SUBCONSCIOUS_API_KEY}
    kwargs:
      offline_bundle: /srv/bench-artifacts/deepswe-offline.tar.gz
      offline_bundle_sha256: REPLACE_WITH_BUNDLE_SHA256
      offline_control_plane_hosts: [api.subconscious.dev]
```

For an immediate `sc` run from an unpushed worktree, build the package and use
the resulting binary as `binary_path` while retaining the same network policy:

```bash
integrations/harbor/build-package.sh
```

```yaml
    kwargs:
      # Fastest local/cloud path: upload an already-built static binary.
      binary_path: /absolute/path/to/sc
```

An exact source bundle can also be built during offline agent setup when the
task image already has Rust and the shared bundle contains Cargo's full locked
dependency cache:

```yaml
    kwargs:
      source_archive: /absolute/path/to/subconscious-code/integrations/harbor/dist/subconscious-code-source.tar.gz
```

The adapter uploads that source bundle into each task sandbox and builds it
with `CARGO_NET_OFFLINE=true`.
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

In default offline mode, setup never uses `apt`, `curl`, Git, rustup, PyPI, or
`uv tool install`. It reuses a prebaked agent or installs from the verified
bundle. An absent artifact is an installation error. The legacy network
installer remains available only through the explicit `offline: false` escape
hatch and must not be used for scored benchmark runs.

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
