//! rc-tools: built-in tool implementations (§6).
//!
//! M1: `Read` + the read registry. The registry is shared via rc-core's
//! [`ToolCtx`](rc_core::ToolCtx); `Read` populates it so `Write`/`Edit` (M2)
//! can enforce "read before mutate" (§6.2/§6.3). The full permission engine is
//! M3 — `Read` does only a basic path-scope check here (deny-read globs and
//! symlink-escape guards land in M3).

pub mod read;

pub use read::Read;
