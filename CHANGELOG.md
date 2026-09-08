# Changelog

All notable changes to Subconscious Code are documented here. This project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.4] - 2026-09-08

### Added

- Native Windows x64 support on the stable release channel: a self-contained
  `sc.exe`, ZIP archive, SHA-256 checksum, and Sigstore verification bundles.
- A separate PowerShell backend, exact-script approvals, process-tree cleanup,
  Windows profile discovery, and an explicit optional Git Bash backend.

### Compatibility

- Promotes the Windows preview implementation without changing Unix shell
  behavior. Windows ARM64 and kernel sandboxing remain unsupported.
- Automated Windows and Unix checks and public-release installation passed;
  browser login and real model inference still require user-device verification.

## [0.1.4-windows.0] - 2026-09-08

### Added

- Native Windows x64 preview with a self-contained `sc.exe`, release ZIP,
  SHA-256 checksum, and Sigstore verification bundles.
- A separate PowerShell shell backend with exact-script approvals, UTF-8
  output, persistent workspace directories, bounded background logs, and
  Windows Job Object process-tree cleanup. Git Bash remains an explicit option.
- Windows profile discovery, CI coverage, and executable mock-gateway smoke tests.

### Compatibility

- Unix shell implementations are unchanged. Windows kernel sandboxing and
  ARM64 binaries are not supported in this preview. Browser login and real
  inference still need verification on a normal user device.

## [0.1.3] - 2026-09-04

### Added

- `sc update` checks the running binary against the newest GitHub release and
  reports the `subc sc install` command when an update is available. A `--json`
  mode provides the same status for scripts.

### Changed

- Mouse-wheel history navigation now moves one transcript row per event, and
  transcript selections remain anchored while scrolling across viewports.
- The TUI stops repainting on idle poll timeouts, allowing terminal tabs to
  become quiescent while no turn or input is active.

### Fixed

- Copying a transcript selection now includes the complete range between its
  endpoints, including history rows outside the current viewport.
- The incremental-session durability test now waits for the asynchronous
  writer's flush instead of racing its filesystem thread in fast CI runners.

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

[Unreleased]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.3...v0.1.4
[0.1.4-windows.0]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.3...v0.1.4-windows.0
[0.1.3]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/subconscious-systems/subconscious-code/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/subconscious-systems/subconscious-code/releases/tag/v0.1.0
