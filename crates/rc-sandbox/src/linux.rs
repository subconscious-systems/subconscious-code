//! Linux implementation: Landlock (filesystem) + seccomp-BPF (network).
//!
//! All allocation happens in [`prepare`] (the parent, before fork). [`install`]
//! returns a `pre_exec` closure that runs in the forked child and issues only
//! raw syscalls — `landlock_restrict_self`, `prctl(PR_SET_NO_NEW_PRIVS)`,
//! `seccomp(SECCOMP_SET_MODE_FILTER)` — which are async-signal-safe (no
//! allocation, no libc state touched). `NO_NEW_PRIVS` is set before seccomp so
//! the filter survives `exec` into the shell; Landlock domains are inherited
//! across `exec` by design.
//!
//! The Landlock ruleset fd is created `O_CLOEXEC` by the kernel, so we `dup` it
//! to a non-`CLOEXEC` fd in [`prepare`] and let the crate drop the original.
//! The dup'd fd is inherited by the forked child; the parent closes its copy
//! via the [`SandboxGuard`](super::SandboxGuard) after `spawn`.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use landlock::{Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr};

use super::NETWORK_SYSCALLS;

/// Host paths needed to execute normal developer toolchains without granting
/// them write access. `/opt` is where Harbor/E2B task images install their
/// testbed Conda environment; omitting it made the sandbox reject the Python
/// interpreter in most historical benchmark trajectories.
const READ_ONLY_SYSTEM_DIRS: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc", "/opt"];

#[derive(Debug)]
pub struct Prepared {
    /// Non-`CLOEXEC` dup of the populated Landlock ruleset fd, inherited by the
    /// forked child. `None` if Landlock is unavailable (seccomp-only confinement).
    ruleset_fd: Option<OwnedFd>,
    /// Compiled seccomp BPF program denying the network syscalls. `None` if
    /// `allow_net` is true.
    bpf: Option<Vec<libc::sock_filter>>,
}

/// Build all parent-side state. Fail-closed when nothing can be enforced;
/// graceful-degrade to seccomp-only when Landlock is unavailable.
pub fn prepare(roots: &[PathBuf], allow_net: bool) -> io::Result<Prepared> {
    let ruleset_fd = build_landlock(roots); // None → Landlock unavailable, degrade
    let bpf = if allow_net {
        None
    } else {
        Some(build_seccomp())
    };

    if ruleset_fd.is_none() && bpf.is_none() {
        // --sandbox with --sandbox-net on a Landlock-less kernel: nothing to
        // enforce. Refuse rather than run unsandboxed.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "rc-sandbox: Landlock unavailable and network is allowed; \
             no confinement can be applied",
        ));
    }
    Ok(Prepared { ruleset_fd, bpf })
}

fn ioerr(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Build and populate a Landlock ruleset: deny all FS access by default; allow
/// full read/write/execute/make/remove beneath each writable root; allow
/// read+execute beneath the system dirs (so `/usr/bin` etc. keep working).
/// Returns a non-`CLOEXEC` dup of the ruleset fd, or `None` if Landlock is not
/// supported by this kernel (graceful degrade to seccomp-only).
fn build_landlock(roots: &[PathBuf]) -> Option<OwnedFd> {
    build_landlock_inner(roots)
        .map_err(|e| {
            tracing::debug!("rc-sandbox: Landlock unavailable, degrading to seccomp-only: {e}");
        })
        .ok()
}

fn build_landlock_inner(roots: &[PathBuf]) -> io::Result<OwnedFd> {
    // ABI::V1 (Linux 5.13+) is the broadest baseline with write confinement.
    let abi = landlock::ABI::V1;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);

    let mut created = Ruleset::default()
        .handle_access(access_all)
        .map_err(ioerr)?
        .create()
        .map_err(ioerr)?;

    // Writable roots: full access (read/write/execute/make/remove).
    for root in roots {
        if !root.is_dir() {
            continue; // a missing root contributes no allow-rule
        }
        let fd = PathFd::new(root).map_err(ioerr)?;
        created = created
            .add_rule(PathBeneath::new(fd, access_all))
            .map_err(ioerr)?;
    }
    // Read-only system dirs: exec + reads (binaries, libs, configs, benchmark
    // toolchains). They deliberately do not receive write/make/remove rights.
    for dir in READ_ONLY_SYSTEM_DIRS {
        if Path::new(dir).is_dir() {
            if let Ok(fd) = PathFd::new(dir) {
                created = created
                    .add_rule(PathBeneath::new(fd, access_read))
                    .map_err(ioerr)?;
            }
        }
    }

    // Shells, git, compilers, and test runners routinely open `/dev/null`
    // themselves (including for redirects such as `2>/dev/null`). Opening the
    // child's stdio before Landlock is installed is not enough. Grant only the
    // two file-content rights on this one device; allowing all of `/dev` would
    // expose unrelated terminals and devices to sandboxed commands.
    let dev_null = Path::new("/dev/null");
    if dev_null.exists() {
        let fd = PathFd::new(dev_null).map_err(ioerr)?;
        created = created
            .add_rule(PathBeneath::new(
                fd,
                AccessFs::ReadFile | AccessFs::WriteFile,
            ))
            .map_err(ioerr)?;
    }

    // The crate's fd is O_CLOEXEC (kernel default); take it out of the
    // RulesetCreated, dup to an inheritable (non-CLOEXEC) fd, then let the
    // original OwnedFd drop close the CLOEXEC copy. The dup'd fd is inherited
    // by the forked child.
    let created_fd: Option<OwnedFd> = created.into();
    let Some(ruleset) = created_fd else {
        return Err(io::Error::other("landlock: ruleset has no fd"));
    };
    let raw = ruleset.as_raw_fd();
    let dup = unsafe { libc::dup(raw) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

/// Compile (no syscall) a seccomp BPF program that denies the network syscalls
/// with `EPERM` and allows everything else. Hand-rolled in `libc` types so the
/// same `sock_filter`/`sock_fprog` feed the raw `seccomp()` syscall.
///
/// Program shape (the classic deny-list):
/// ```text
///   LD  seccomp_data.nr          // load the syscall number
///   for each denied nr N:
///     JEQ N  → match: next (RET ERRNO) ; no-match: skip 1 (next JEQ / ALLOW)
///     RET ERRNO|EPERM
///   RET ALLOW                    // default
/// ```
fn build_seccomp() -> Vec<libc::sock_filter> {
    let denied: Vec<i64> = NETWORK_SYSCALLS.iter().filter_map(|n| sysno(n)).collect();

    let mut prog: Vec<libc::sock_filter> = Vec::with_capacity(2 + 2 * denied.len());
    // Load seccomp_data.nr (offset 0 within `struct seccomp_data`).
    prog.push(bpf_stmt(
        (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
        0,
    ));
    for nr in denied {
        // match → fall through to the RET ERRNO on the next line;
        // no match → skip that RET (jf=1) to the next JEQ / the final ALLOW.
        prog.push(bpf_jump(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            nr as u32,
            0,
            1,
        ));
        prog.push(bpf_stmt(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        ));
    }
    prog.push(bpf_stmt(
        (libc::BPF_RET | libc::BPF_K) as u16,
        libc::SECCOMP_RET_ALLOW,
    ));
    prog
}

#[inline]
fn bpf_stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

#[inline]
fn bpf_jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// Map a network syscall name to its per-arch `libc::SYS_*` number.
fn sysno(name: &str) -> Option<i64> {
    // `libc::SYS_*` are `c_long` (= i64 on 64-bit Linux, the only target this
    // file compiles for), so no cast is needed.
    Some(match name {
        "socket" => libc::SYS_socket,
        "socketpair" => libc::SYS_socketpair,
        "connect" => libc::SYS_connect,
        "bind" => libc::SYS_bind,
        "listen" => libc::SYS_listen,
        "accept" => libc::SYS_accept,
        "accept4" => libc::SYS_accept4,
        "sendto" => libc::SYS_sendto,
        "recvfrom" => libc::SYS_recvfrom,
        "sendmsg" => libc::SYS_sendmsg,
        "recvmsg" => libc::SYS_recvmsg,
        "getsockopt" => libc::SYS_getsockopt,
        "setsockopt" => libc::SYS_setsockopt,
        "getpeername" => libc::SYS_getpeername,
        "getsockname" => libc::SYS_getsockname,
        _ => return None,
    })
}

/// Build the `pre_exec` closure (child-side, syscalls only) and the parent-side
/// fd guard.
pub fn install(
    prepared: Option<Prepared>,
) -> (
    Box<dyn FnMut() -> io::Result<()> + Send + Sync + 'static>,
    super::SandboxGuard,
) {
    let Some(p) = prepared else {
        return (Box::new(|| Ok(())), super::SandboxGuard(None));
    };
    // Raw fd number captured by the closure (Copy). The guard keeps the
    // parent's OwnedFd (and thus the OS fd) open across spawn.
    let fd = p.ruleset_fd.as_ref().map(|f| f.as_raw_fd());
    let bpf = p.bpf;
    let guard = super::SandboxGuard(p.ruleset_fd);

    let closure: Box<dyn FnMut() -> io::Result<()> + Send + Sync + 'static> = Box::new(move || {
        // SAFETY: runs in the forked child before exec. Only raw syscalls; no
        // allocation. `fd` is a raw i32 captured by value; `bpf` is an owned Vec
        // moved in (reading it does not allocate).
        unsafe {
            // NO_NEW_PRIVS must be set BEFORE both `landlock_restrict_self` and
            // `seccomp(SECCOMP_SET_MODE_FILTER)`: each requires it (or
            // CAP_SYS_ADMIN, which an unprivileged caller doesn't have) and
            // returns EPERM otherwise. It also survives exec, so the filter
            // and Landlock domain stick to the exec'd shell.
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(fd) = fd {
                // Apply the Landlock domain to this process (inherited by the
                // exec'd shell). 0 = no flags.
                if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u64) != 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(fd);
            }
            if let Some(bpf) = bpf.as_ref() {
                let prog = libc::sock_fprog {
                    len: bpf.len() as u16,
                    filter: bpf.as_ptr() as *mut libc::sock_filter,
                };
                if libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    0u32,
                    &prog as *const _ as *const libc::c_void,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    });
    (closure, guard)
}

// ---- Linux-only integration tests (run by the user on a real Linux box) ----
// Compiled only on Linux; absent on macOS so the workspace stays green here.
// `cargo test --workspace` on Linux exercises the denial.
#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use crate::Sandbox;
    use std::process::Command;
    use tempfile::tempdir;

    fn sandbox_cmd(roots: &[PathBuf], allow_net: bool, shell_cmd: &str) -> std::process::Output {
        let sandbox = Sandbox::new(roots.to_vec(), allow_net);
        let prepared = sandbox.prepare().expect("prepare");
        let (pre_exec, _guard) = prepared.install();
        let mut cmd = Command::new("/bin/sh");
        // Run inside the first root so relative paths in the test command resolve
        // beneath an allowed Landlock root, not the test process's cwd.
        if let Some(root) = roots.first() {
            cmd.current_dir(root);
        }
        cmd.arg("-c").arg(shell_cmd);
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(pre_exec);
        }
        cmd.output().expect("spawn")
    }

    #[test]
    fn build_seccomp_program_shape() {
        let prog = build_seccomp();
        // 1 (LD) + 2 per denied syscall (JEQ + RET) + 1 (ALLOW).
        let denied = NETWORK_SYSCALLS.iter().filter_map(|n| sysno(n)).count();
        assert_eq!(prog.len(), 1 + 2 * denied + 1);
        // First instruction loads the syscall number (BPF_LD|BPF_W|BPF_ABS).
        assert_eq!(prog[0].code & 0x07, libc::BPF_LD as u16 & 0x07);
    }

    #[test]
    fn write_inside_root_is_allowed() {
        let dir = tempdir().unwrap();
        let out = sandbox_cmd(&[dir.path().to_path_buf()], false, "echo hi > inside.txt");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.path().join("inside.txt").exists());
    }

    #[test]
    fn dev_null_is_readable_and_writable() {
        let dir = tempdir().unwrap();
        let out = sandbox_cmd(
            &[dir.path().to_path_buf()],
            false,
            "printf discarded >/dev/null && cat /dev/null",
        );
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn opt_is_read_only_when_present() {
        if !Path::new("/opt").is_dir() {
            return;
        }
        let dir = tempdir().unwrap();
        let out = sandbox_cmd(
            &[dir.path().to_path_buf()],
            false,
            "test -r /opt && test -x /opt && ! touch /opt/sc-sandbox-must-not-write 2>/dev/null",
        );
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn write_outside_root_is_denied() {
        let dir = tempdir().unwrap();
        // The "outside" dir must NOT live under `/tmp`: `Sandbox::new` always
        // adds `/tmp` as a writable root, so a tempdir() there is still inside
        // the sandbox. Put it under `$HOME`, which is never an allowed root.
        let home = std::env::var_os("HOME").expect("$HOME set");
        let outside = tempfile::tempdir_in(&home).expect("tempdir in $HOME");
        let target = outside.path().join("escaped.txt");
        let out = sandbox_cmd(
            &[dir.path().to_path_buf()],
            false,
            &format!("echo x > {}", target.display()),
        );
        // Landlock denies open(O_WRONLY) → the shell command fails.
        assert!(
            !out.status.success(),
            "write outside root unexpectedly succeeded\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(!target.exists(), "file escaped the sandbox");
    }

    #[test]
    fn network_is_denied_without_allow_net() {
        // socket() returns EPERM under the seccomp filter. Probe with a shell
        // `/dev/tcp` redirection; the connection attempt must fail.
        let dir = tempdir().unwrap();
        let out = sandbox_cmd(
            &[dir.path().to_path_buf()],
            false,
            "sh -c 'echo > /dev/tcp/1.1.1.1/80' 2>/dev/null; test $? -ne 0",
        );
        assert!(out.status.success(), "network was not denied by seccomp");
    }
}
