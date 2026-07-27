//! rc-session: transcript persistence, checkpoints, rewind (§9).
//!
//! Not yet implemented — lands in M5. Append-only JSONL per session (flush at
//! every turn boundary for crash recovery), `--continue`/`--resume`. M10 adds
//! the CAS checkpoints and `/rewind`. Note: rewind restores only files the
//! agent touched — Bash side effects (build artifacts, clones) are outside
//! the CAS and not rolled back.
