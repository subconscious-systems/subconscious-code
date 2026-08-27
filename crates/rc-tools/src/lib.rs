//! rc-tools: built-in tool implementations (§6).
//!
//! M2: `Read`, `ReadMany`, `Write`, `Edit`, `List`, `Glob`, `Grep`, and a
//! foreground, stateless `Bash`. `Write`/`Edit` enforce "read before mutate"
//! via the shared [`ReadRegistry`] (§6.2/§6.3). The full permission engine is
//! M3; here there's only a basic path-scope check and (for Bash) a conservative
//! destructive-command safety floor.

pub mod append;
pub mod bash;
pub mod edit;
pub mod env_hygiene;
pub mod glob;
pub mod grep;
pub mod grep_many;
pub mod list;
pub mod read;
pub mod read_many;
pub mod util;
pub mod write;

pub use append::Append;
pub use bash::Bash;
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use grep_many::GrepMany;
pub use list::List;
pub use read::Read;
pub use read_many::ReadMany;
pub use write::Write;
