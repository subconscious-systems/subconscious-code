//! rc-tools: built-in tool implementations (§6).
//!
//! M2: `Read`, `Write`, `Edit` (the read/mutate trio) + `Glob`, `Grep`, and a
//! foreground, stateless `Bash`. `Write`/`Edit` enforce "read before mutate"
//! via the shared [`ReadRegistry`] (§6.2/§6.3). The full permission engine is
//! M3; here there's only a basic path-scope check and (for Bash) a conservative
//! destructive-command safety floor.

pub mod bash;
pub mod edit;
pub mod env_hygiene;
pub mod glob;
pub mod grep;
pub mod read;
pub mod util;
pub mod write;

pub use bash::Bash;
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use read::Read;
pub use write::Write;
