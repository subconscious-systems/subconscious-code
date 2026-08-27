# Getting started

This guide takes a new installation from source checkout to its first verified
model request. The primary development platforms are Linux and macOS. The core
client is portable Rust, but the kernel sandbox and systemd resource controls
are Linux-only.

## 1. Install the prerequisites

Install Git and Rust 1.89 or newer. The recommended Rust installer is
[rustup](https://rustup.rs/):

```sh
rustup toolchain install stable
rustup default stable
```

You also need an API key for an OpenAI-compatible Chat Completions endpoint.
The endpoint must support streaming responses and tool calls for the complete
agent experience.

## 2. Build and install `sc`

```sh
git clone https://github.com/subconscious-systems/subconscious-code.git
cd subconscious-code
cargo install --locked --path crates/rc-cli
sc --version
```

`cargo install` places `sc` in Cargo's binary directory, normally
`~/.cargo/bin`. Ensure that directory is on `PATH` if the final command is not
found.

## 3. Configure a provider

For the default Subconscious endpoint:

```sh
export SC_API_KEY="your-api-key"
```

For another compatible endpoint:

```sh
export SC_API_KEY="your-provider-key"
export SC_BASE_URL="https://provider.example/v1"
export SC_MODEL="provider/model-name"
```

Do not put a literal API key in `settings.json`. Environment variables take
priority, and `/menu` can save a key separately to `~/.sc/key` with mode `0600`.

## 4. Verify the endpoint

Run the built-in compatibility checks before starting work:

```sh
sc doctor
```

Doctor checks resolved configuration, authentication, non-streaming and
streaming chat, and tool-call support. It reports a missing key instead of
failing before diagnostics are printed.

## 5. Start in a project

```sh
cd /path/to/project
sc
```

Useful controls:

| Control | Action |
| --- | --- |
| `Enter` | Submit the composer |
| `@path` | Complete and include a project file |
| `/` | Show available slash commands |
| `/menu` | Open projects, sessions, API key, and settings |
| `Shift+Tab` | Cycle permission mode |
| `Esc` | Interrupt the active turn |
| `Ctrl+O` | Release/capture the mouse for native terminal selection |
| `Ctrl+C` | Quit |

Try a read-only first request:

```text
Explain the project layout and identify the main executable entry point.
```

## Headless operation

`-p` runs one turn and writes the final answer to stdout:

```sh
sc -p "summarize the test strategy"
```

Headless mode has nobody available to approve mutations, so writes and shell
commands are denied unless project rules allow them. See
[Configuration](CONFIGURATION.md#permissions) before automating changes.

## Update or uninstall

From an updated checkout:

```sh
git pull --ff-only
cargo install --locked --force --path crates/rc-cli
```

To uninstall:

```sh
cargo uninstall rc-cli
```

For provider, terminal, or DLR problems, continue with
[Troubleshooting](TROUBLESHOOTING.md).
