//! rc-tui: the ratatui frontend (§12).
//!
//! Not yet implemented — lands in M4. Observes rc-core through a broadcast
//! channel of `AgentEvent`s and pushes `UserAction`s down an mpsc — never
//! calls into core synchronously. Incremental markdown parse (O(n²) full
//! re-parse per delta is the trap), grapheme-cluster wrapping, word-level
//! diff highlighting. `rc-core` must run headless with zero TUI deps, so this
//! crate sits *above* core in the dependency direction.
