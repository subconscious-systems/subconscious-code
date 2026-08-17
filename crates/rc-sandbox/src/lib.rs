//! rc-sandbox: opt-in OS confinement for the `Bash` tool (M7 / §7.6).
//!
//! Defense-in-depth *behind* the permission engine: an already-approved Bash
//! command is confined at the kernel level so it can't write outside the
//! workspace roots or (by default) open network sockets. **Linux only** —
//! Landlock (filesystem) + seccomp-BPF (network). On other platforms the crate
//! compiles to a documented no-op, so the Bash call site never branches on OS.
//!
//! Opt-in: see `ToolCtx::sandbox` / the `--sandbox` CLI flag. Off by default so
//! `cargo`/`npm`/`git` (which need network + writes outside the workspace) keep
//! working.
//!
//! The confinement is split into a parent-build / child-apply seam so the
//! `pre_exec` hook — which runs in the forked child and must be
//! async-signal-safe (no allocation) — only issues raw syscalls. See
//! [`Sandbox::prepare`] and [`PreparedSandbox::install`].

use std::io;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;

/// Network syscalls denied when `allow_net` is false, by name. The Linux
/// module resolves these to per-arch `libc::SYS_*` numbers. Single source of
/// truth (testable on all platforms); the name↔number mapping lives in
/// `linux.rs` and must stay in sync with this list.
#[allow(dead_code)] // only consumed by the Linux module + tests
pub(crate) const NETWORK_SYSCALLS: &[&str] = &[
    "socket",
    "socketpair",
    "connect",
    "bind",
    "listen",
    "accept",
    "accept4",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "getsockopt",
    "setsockopt",
    "getpeername",
    "getsockname",
];

/// A Bash confinement policy. Cheap to clone; the expensive work is in
/// [`Self::prepare`].
#[derive(Debug, Clone)]
pub struct Sandbox {
    roots: Vec<PathBuf>,
    allow_net: bool,
}

impl Sandbox {
    /// Build a policy from the workspace allowed roots. `/tmp` is always added
    /// (and de-duplicated) so shell temp files keep working.
    pub fn new(mut roots: Vec<PathBuf>, allow_net: bool) -> Self {
        let tmp = PathBuf::from("/tmp");
        if !roots.iter().any(|r| r == &tmp) {
            roots.push(tmp);
        }
        Self { roots, allow_net }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn allow_net(&self) -> bool {
        self.allow_net
    }

    /// Build all state that must be allocated before fork. On non-Linux this is
    /// a no-op — the returned [`PreparedSandbox`] installs nothing.
    ///
    /// **Fail-closed:** if the kernel supports neither Landlock nor seccomp
    /// (or `--sandbox` is on with `--sandbox-net` on a Landlock-less kernel,
    /// leaving nothing enforceable), this returns `Err` so the caller refuses to
    /// run the command unsandboxed. Partial support is accepted: Landlock-only
    /// or seccomp-only confinement is applied when the other is unavailable.
    pub fn prepare(&self) -> io::Result<PreparedSandbox> {
        #[cfg(target_os = "linux")]
        {
            linux::prepare(&self.roots, self.allow_net).map(|p| PreparedSandbox(Some(p)))
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Confinement is Linux-only; on other platforms the prepared
            // sandbox installs nothing (a no-op closure). The opt-in flag is
            // still honored in the sense that `prepare` succeeds, but no kernel
            // confinement is applied.
            let _ = (&self.roots, self.allow_net);
            Ok(PreparedSandbox(()))
        }
    }
}

/// Parent-built confinement state, applied in the child via `pre_exec`. Opaque
/// so callers can't depend on platform internals.
#[derive(Debug)]
pub struct PreparedSandbox(
    #[cfg(target_os = "linux")] Option<linux::Prepared>,
    #[cfg(not(target_os = "linux"))] (),
);

/// Closes the parent-side ruleset fd after `spawn`. The child inherits a copy
/// at fork; the parent must close its own to avoid leaking a fd per Bash call.
/// Hold it across `Command::spawn`, then drop.
#[derive(Debug)]
#[allow(dead_code)] // the fd is kept alive for its Drop (closes the parent copy); never read
pub struct SandboxGuard(
    #[cfg(target_os = "linux")] Option<std::os::unix::io::OwnedFd>,
    #[cfg(not(target_os = "linux"))] (),
);

impl PreparedSandbox {
    /// Produce the `pre_exec` closure (run in the forked child; issues only raw
    /// syscalls) plus a [`SandboxGuard`] that keeps the parent-side ruleset fd
    /// open across `spawn` and closes it when dropped.
    ///
    /// Call site: pass the closure to `CommandExt::pre_exec`, spawn, then let
    /// the guard drop.
    pub fn install(
        self,
    ) -> (
        Box<dyn FnMut() -> io::Result<()> + Send + Sync + 'static>,
        SandboxGuard,
    ) {
        #[cfg(target_os = "linux")]
        {
            linux::install(self.0)
        }
        #[cfg(not(target_os = "linux"))]
        {
            (Box::new(|| Ok(())), SandboxGuard(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn new_always_includes_tmp() {
        let s = Sandbox::new(vec![PathBuf::from("/repo")], false);
        assert!(s.roots().iter().any(|r| r.as_path() == Path::new("/tmp")));
        assert_eq!(s.roots().len(), 2);
        assert!(!s.allow_net());
    }

    #[test]
    fn new_dedups_tmp() {
        let s = Sandbox::new(vec![PathBuf::from("/repo"), PathBuf::from("/tmp")], true);
        assert_eq!(
            s.roots()
                .iter()
                .filter(|r| r.as_path() == Path::new("/tmp"))
                .count(),
            1
        );
        assert!(s.allow_net());
    }

    #[test]
    fn new_tmp_only_is_fine() {
        let s = Sandbox::new(vec![], false);
        assert_eq!(s.roots().len(), 1);
        assert_eq!(s.roots()[0], PathBuf::from("/tmp"));
    }

    #[test]
    fn network_syscall_set_is_the_expected_network_surface() {
        // Document the intent: every entry is a networking syscall name that
        // the Linux module maps to a libc::SYS_* constant. If you add a network
        // syscall to deny, add it here too (and a mapping in linux.rs).
        let expected = [
            "socket",
            "socketpair",
            "connect",
            "bind",
            "listen",
            "accept",
            "accept4",
            "sendto",
            "recvfrom",
            "sendmsg",
            "recvmsg",
            "getsockopt",
            "setsockopt",
            "getpeername",
            "getsockname",
        ];
        for name in expected {
            assert!(NETWORK_SYSCALLS.contains(&name), "missing {name}");
        }
        assert!(NETWORK_SYSCALLS.contains(&"socket"));
        // Non-network syscalls must not be in the deny list.
        assert!(!NETWORK_SYSCALLS.contains(&"write"));
        assert!(!NETWORK_SYSCALLS.contains(&"read"));
    }
}
