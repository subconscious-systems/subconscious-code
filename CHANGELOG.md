# Changelog

All notable changes to Subconscious Code are documented here. This project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-09-01

Initial public release.

### Added

- Native interactive and headless coding agent with resumable sessions.
- Permission-aware file, search, edit, and shell tools.
- OpenAI-compatible streaming with tool calls, retries, request spooling, and
  optional gzip transport.
- Optional DLR sidecar transport for large, repeated conversation contexts.
- Crash-safe benchmark reports and ATIF trajectories for Harbor.
- Reproducible offline Harbor adapters for Subconscious Code and mini-swe-agent.

### Reliability

- Bounded recovery for transport failures that occur before semantic model
  output, without replaying partially emitted responses.
- Benchmark completion review, no-progress handling, and endpoint diagnostics.
- Linux sandboxing, fail-closed headless permissions, and offline benchmark
  network policy.

[0.1.0]: https://github.com/subconscious-systems/subconscious-code/releases/tag/v0.1.0
