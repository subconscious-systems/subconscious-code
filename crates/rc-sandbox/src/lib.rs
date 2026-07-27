//! rc-sandbox: OS sandboxing (§7.6).
//!
//! Not yet implemented — lands in M7. Landlock + seccomp-BPF (Linux),
//! `sandbox-exec` (macOS), Job Objects + restricted token (Windows). A leaf
//! crate used by Bash, never calling upward.
