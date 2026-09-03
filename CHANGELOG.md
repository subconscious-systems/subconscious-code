# Changelog

All notable changes to Subconscious Code are documented here. This project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/subconscious-systems/subconscious-code/releases/tag/v0.1.0
