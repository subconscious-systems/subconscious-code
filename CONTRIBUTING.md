# Contributing

Thanks for helping improve Subconscious Code. Bug reports, documentation fixes,
tests, benchmarks, and focused code changes are welcome.

By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
For questions and usage help, see [Support](SUPPORT.md). Report security issues
privately according to [SECURITY.md](SECURITY.md).

## Development setup

Install Git and stable Rust 1.89 or newer, then clone the repository:

```sh
git clone https://github.com/subconscious-systems/subconscious-code.git
cd subconscious-code
cargo test --locked --workspace
```

The DLR protocol is a separate Cargo workspace and must be checked separately:

```sh
cargo test --locked --manifest-path integrations/dlr/Cargo.toml --workspace
```

## Before opening a pull request

Run the checks that apply to your change:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo fmt --manifest-path integrations/dlr/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path integrations/dlr/Cargo.toml \
  --workspace --all-targets -- -D warnings
cargo test --locked --manifest-path integrations/dlr/Cargo.toml --workspace
```

Keep commits reviewable and avoid unrelated formatting or generated artifacts.
Add tests for behavior changes. Update the README or focused guide when a CLI,
setting, protocol, or deployment contract changes.

## Pull requests

- Explain the user-visible problem and the chosen solution.
- List the checks you ran and any checks you could not run.
- Call out compatibility, security, persistence, or performance implications.
- Include measurements for performance claims and reproduction instructions.
- Do not commit API keys, credentials, private prompts, or proprietary code.

Small pull requests are easier to review. Large architectural changes benefit
from a proposal issue before implementation, especially when they change the
session format, DLR wire protocol, permissions, or provider behavior.

## Architecture expectations

Respect the crate boundaries described in [Architecture](docs/ARCHITECTURE.md).
Provider wire details belong in `rc-proto`; the provider-independent loop lives
in `rc-core`; terminal presentation belongs in `rc-tui`. Preserve fail-closed
permission behavior and do not expose secrets through logs, settings, tests, or
diagnostic output.

Contributions are accepted under the repository's dual MIT OR Apache-2.0
license unless explicitly stated otherwise.
