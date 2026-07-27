//! rc-ctx: context assembly, memory files, compaction (§8).
//!
//! Not yet implemented — lands in M6. Hierarchical memory files, `@` mentions,
//! token estimation + calibration, tool-output truncation, microcompaction
//! (superseded-Read eviction), and full compaction. Note: microcompaction
//! mutates tool-result *bodies* below the prefix cut — the stable prefix is
//! "above the deepest mutation point", which is why `project()` re-serializes
//! each request (§4.1).
