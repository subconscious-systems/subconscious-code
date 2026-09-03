# Configuration

`sc` uses layered JSON settings plus environment variables and CLI flags.
Configuration is resolved in this order, with later layers winning:

1. Compiled defaults
2. User settings: `~/.sc/settings.json`
3. Project settings: `./.sc/settings.json`
4. `SC_*` environment variables
5. CLI flags

The `/menu` settings page edits the user file. Project settings are useful for
checked-in permission rules and model/context defaults shared by a repository.

## Complete settings example

All keys are optional. Unknown keys are preserved by the menu editor and
ignored by older clients.

```json
{
  "model": "subconscious/glm-5.2",
  "models": ["subconscious/glm-5.2"],
  "small_model": "subconscious/glm-5.2",
  "provider": {
    "base_url": "https://api.subconscious.dev/v1",
    "api_key_env": "SC_API_KEY",
    "timeout_ms": 0,
    "idle_timeout_ms": 120000,
    "max_retries": 0,
    "request_gzip": true,
    "reasoning_effort": "high",
    "dlr_enabled": true,
    "dlr_url": "https://api.subconscious.dev",
    "dlr_ingress_token_env": "SC_DLR_INGRESS_TOKEN",
    "dlr_repair_margin_pct": 5
  },
  "permissions": {
    "default_mode": "default",
    "allow": [],
    "ask": [],
    "deny": [],
    "additional_directories": []
  },
  "sandbox": {
    "enabled": false,
    "allow_net": false
  },
  "context": {
    "inline_file_cap": 0,
    "tool_result_cap": 65536,
    "bash_output_cap": 0,
    "grep_output_cap": 0,
    "read_default_limit": 0,
    "read_max_line_chars": 0,
    "glob_cap": 0,
    "max_iters": 1000
  },
  "ui": {
    "mouse": true
  }
}
```

For size caps, `0` means unlimited. `max_iters` is a separate runaway
backstop, not a byte cap.

## Credentials

The API key is resolved from the variable named by `provider.api_key_env`
(`SC_API_KEY` by default), then from `~/.sc/key`. A bare interactive `sc`
launch prompts securely and creates the key file when neither source is set;
`/menu` can replace it later. Never store a literal key in either settings file;
`sc doctor` warns when it finds a key-shaped value there.

```sh
export SC_API_KEY="your-api-key"
```

The DLR ingress token follows the same pattern: settings contain only the
environment-variable name, while the secret stays in that variable.

## Common environment variables

| Variable | Meaning | Default |
| --- | --- | --- |
| `SC_API_KEY` | Provider credential | unset |
| `SC_BASE_URL` | OpenAI-compatible base URL | `https://api.subconscious.dev/v1` |
| `SC_MODEL` | Model sent to the provider | `subconscious/glm-5.2` |
| `SC_MAX_TOKENS` | Completion allowance; `0` uses provider default | `8192` |
| `SC_TEMPERATURE` | Sampling temperature | provider default |
| `SC_REASONING_EFFORT` | Provider reasoning level; `off` omits it | `high` |
| `SC_TIMEOUT_MS` | Total request timeout; `0` disables it | `0` |
| `SC_IDLE_TIMEOUT_MS` | Maximum silence between stream activity | `120000` |
| `SC_TURN_TIMEOUT_MS` | Whole-turn budget; `0` disables it | `0` |
| `SC_MAX_RETRIES` | Client retry count | `0` |
| `SC_REQUEST_GZIP` | Enable request gzip | automatic for Subconscious |
| `SC_DLR_ENABLED` | Prefer DLR (`true`) or JSON only (`false`) | see DLR below |
| `SC_DLR_URL` | DLR origin; `/v1/dlr/*` is appended | Subconscious origin |
| `SC_MOUSE` | Capture mouse input in the TUI | `true` |
| `SC_SANDBOX` | Enable the Linux Bash sandbox | `false` |
| `SC_SANDBOX_NET` | Permit network inside that sandbox | `false` |

Boolean variables accept `1/0`, `true/false`, `yes/no`, and `on/off`, without
case sensitivity. Context caps also have direct `SC_*` forms such as
`SC_TOOL_RESULT_CAP`, `SC_READ_DEFAULT_LIMIT`, and `SC_MAX_ITERS`.

## Custom providers and DLR

The default Subconscious provider tries DLR first and safely falls back to JSON
if `/v1/dlr/capabilities` is unavailable. A custom `SC_BASE_URL` or
`--base-url` uses ordinary JSON by default. This prevents a custom provider's
request or credential from being sent to the default DLR origin.

To enable DLR for a custom deployment, configure it deliberately:

```sh
export SC_BASE_URL="https://gateway.example/v1"
export SC_DLR_URL="https://gateway.example"
export SC_DLR_ENABLED=true
```

`provider.request_transport` remains available for compatibility and expert
rollouts: `json` disables DLR, `auto` safely prefers it, and `dlr` fails closed.
The simpler `dlr_enabled` setting wins when both are present.

## Permissions

Modes range from most cautious to most permissive:

| Mode | Behavior |
| --- | --- |
| `ask` | Confirm every tool, including reads |
| `default` | Confirm writes and shell commands |
| `acceptEdits` | Apply file edits automatically; confirm Bash |
| `plan` | Deny mutating tools |
| `auto` | Do not prompt; catastrophic commands remain denied |

Rules are evaluated deny, then allow, then ask. A project suitable for a
headless Rust maintenance task might contain:

```json
{
  "permissions": {
    "default_mode": "default",
    "allow": [
      "Read",
      "ReadMany",
      "Glob",
      "Grep",
      "GrepMany",
      "Write",
      "Append",
      "Edit",
      "Bash(cargo:*)",
      "Bash(git:*)"
    ],
    "deny": ["Bash(rm:*)"]
  }
}
```

Keep rules narrow. `--dangerously-skip-permissions` is intended for an already
isolated environment and still enforces the catastrophic-command safety floor.

## Memory files

Project instructions are loaded in this order:

1. `~/.sc/AGENTS.md`
2. `./.sc/AGENTS.md`
3. `./AGENTS.md` when the project-local file does not replace it

Use these files for stable repository guidance, build commands, and conventions.
