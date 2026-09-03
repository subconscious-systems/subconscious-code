# Changelog

All notable changes to Subconscious Code are documented here. This project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.2] - 2026-09-03

### Added

- Precompiled Apple Silicon and Intel macOS release archives, checksums, and
  keyless Sigstore bundles for installer-driven setup without Cargo.

### Changed

- Release automation now uses Node 24-compatible GitHub Actions.

## [0.1.1] - 2026-09-03

### Added

- Interactive follow-up queue: press `Tab` during a turn to queue the current
  draft, or `Esc` to hand it off after the active tool call.

### Changed

- Turn dividers show only elapsed time unless files changed, then add compact
  `+N -N` counts without redundant prose.

### Fixed

- In-app copy now uses native system clipboards locally and tmux's clipboard
  bridge when available, with OSC 52 retained for remote sessions.

## [0.1.0] - 2026-09-01

Initial public release.

### Added

- Native interactive and headless coding agent with resumable sessions.
- Permission-aware file, search, edit, and shell tools.
- OpenAI-compatible streaming with tool calls, retries, request spooling, and
  optional gzip transport.
- Optional DLR sidecar transport for large, repeated conversation contexts.
- Crash-safe benchmark reports and ATIF trajectories emitted directly by the
  `sc` CLI.
- Secure first-launch API-key setup for the interactive CLI.

### Reliability

- Bounded recovery for transport failures that occur before semantic model
  output, without replaying partially emitted responses.
- Benchmark completion review, no-progress handling, and endpoint diagnostics.
- Linux sandboxing and fail-closed headless permissions.

[Unreleased]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/subconscious-systems/subconscious-code/releases/tag/v0.1.0
