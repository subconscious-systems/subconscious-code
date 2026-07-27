//! rc-core: the agent loop, turn state, and orchestration (§4).
//!
//! Not yet implemented — lands in M1. Per §16 "First 200 lines", this crate
//! gets the `Tool` trait, `Turn` (the source of truth), and `project()` with
//! the tool-answer invariant assertion (§4.2 / §3.1 trap 3): every assistant
//! `tool_calls` id must be answered by exactly one *contiguous* Tool message.
//! The agent loop, concurrency classes (§4.3), cancellation, and retries
//! follow.
