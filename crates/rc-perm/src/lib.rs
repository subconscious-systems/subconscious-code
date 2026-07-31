//! rc-perm: the permission engine (§7).
//!
//! deny → allow → ask rule matching with tool-specific matchers (path globs for
//! Read/Edit/Write/Glob/Grep, parsed-command prefix matching for Bash — §7.2,
//! never regex on the raw string); the four modes (§7.3); path containment with
//! symlink-escape guards (§7.5); and a catastrophic-deny / always-ask set.
//!
//! The agent loop calls [`PermissionChecker::check`] before each tool call.
//! [`AllowAllChecker`] is for tests; [`BypassChecker`] is
//! `--dangerously-skip-permissions` (still hard-denies catastrophic commands);
//! [`PermissionEngine`] is the real one, fed by rc-config's `permissions` block.

pub mod bash;
pub mod path;
pub mod rules;

pub use bash::{parse_bash, ParsedBash, Sub};
pub use path::{resolve_within, resolve_within_loose};
pub use rules::{AllowAllChecker, BypassChecker, Decision, Mode, PermissionChecker, PermissionEngine};
