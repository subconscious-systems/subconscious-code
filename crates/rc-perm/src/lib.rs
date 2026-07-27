//! rc-perm: the permission engine (§7).
//!
//! Not yet implemented — lands in M3. deny→allow→ask rule matching with tool-
//! specific matchers; Bash matching parses the command (§7.2), never regex on
//! the raw string; modes (default/acceptEdits/plan/bypass); path containment
//! with symlink-escape guards (§7.5).
