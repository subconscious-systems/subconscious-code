# Troubleshooting

Start with:

```sh
sc doctor
```

It prints the resolved endpoint, model, transport, credential presence, and
results of non-streaming, streaming, and tool-call checks.

## `no API key`

Run bare `sc` in a terminal to complete the secure first-launch prompt, or set
the variable named by `provider.api_key_env` (`SC_API_KEY` by default) for
automation. Use `/menu` → Change API key to replace a saved key. Do not add a
literal key to `settings.json`.

```sh
export SC_API_KEY="your-api-key"
```

## 404 or an incorrect request path

`SC_BASE_URL` should normally end in `/v1`; `sc` appends
`/chat/completions`. For example:

```sh
export SC_BASE_URL="http://127.0.0.1:30000/v1"
```

If your proxy already rewrites the version prefix, use the base URL expected by
that proxy and confirm the final path in its access logs.

## Streaming works but tools do not

The endpoint must emit OpenAI-compatible streaming tool-call deltas, including
stable call IDs and JSON arguments. Run `sc doctor`; its tool-call check catches
many endpoints that support plain text but not agent use.

## DLR reports unavailable

In safe mode this is informational: the client remembers the failed capability
probe and uses normal JSON. To disable DLR explicitly:

```sh
export SC_DLR_ENABLED=false
```

For a local sidecar, verify its routes and point the client at its origin:

```sh
curl http://127.0.0.1:32180/healthz
curl http://127.0.0.1:32180/readyz
curl http://127.0.0.1:32180/v1/dlr/capabilities
export SC_DLR_URL=http://127.0.0.1:32180
```

After DLR becomes active, transport errors fail the request instead of silently
resending it through JSON; this prevents duplicate model invocations.

## The first request is rejected as too large

Measure the endpoint's body ceiling deliberately:

```sh
sc doctor --body-ladder
```

This sends 1, 10, 32, 100, and 500 MiB test bodies until one fails. Do not run
it against a metered or production endpoint unless that traffic is acceptable.
Lower context caps in `~/.sc/settings.json` or deploy DLR/gateway infrastructure
that supports the required size.

## A stream appears stuck

The default idle timeout is 120 seconds of no transport activity. Lower it for
faster failure detection:

```sh
export SC_IDLE_TIMEOUT_MS=30000
```

Avoid a small total `SC_TIMEOUT_MS` for large uploads; it can expire while the
request body is still being sent.

## Headless mode will not edit files

This is expected. There is no person to answer a permission prompt. Add narrow
project rules as described in [Configuration](CONFIGURATION.md#permissions), or
use `--dangerously-skip-permissions` only inside an appropriately isolated
environment.

## Mouse selection does not behave like the terminal

The TUI captures the mouse for scrolling and in-app copy. Press `Ctrl+O` to
release it temporarily, or set:

```sh
export SC_MOUSE=false
```

## Linux sandbox denies a required operation

The Bash sandbox denies network by default and writes outside workspace roots.
Use `--sandbox-net` only when network access is required. Add legitimate extra
roots through `permissions.additional_directories`; do not broaden the policy
just to hide an unexpected path.

## Resume cannot find a session

`sc --continue` skips header-only sessions with no history. Inspect
`~/.sc/sessions/` and use `sc --resume /absolute/path/session.jsonl` for a
specific valid file.

If a reproducible problem remains, open a GitHub issue with `sc --version`, OS,
terminal, sanitized `sc doctor` output, and minimal reproduction. Remove keys,
prompts, proprietary source, and session contents first.
