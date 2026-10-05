//! Starting a stdio server process and stopping its whole process tree.
//!
//! A server is often a launcher (`npx` → `node`, `uvx` → `python`), so killing
//! only the direct child would leave the real server running. Each server gets
//! its own process group (Unix) or job object (Windows), and stopping it kills
//! everything in that group or job.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// Variables a server inherits from Marathon. Everything else, including the
/// model API key and `SC_*` settings, stays out unless the server's own `env`
/// sets it.
#[cfg(not(windows))]
const INHERITED_ENV: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM",
];
#[cfg(windows)]
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "LANG",
];

/// The environment a server starts with: the inherited allowlist from
/// `parent`, then the server's configured `env` on top.
pub(crate) fn server_env(
    parent: impl Iterator<Item = (OsString, OsString)>,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = parent
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            INHERITED_ENV.iter().any(|allowed| {
                if cfg!(windows) {
                    allowed.eq_ignore_ascii_case(&key)
                } else {
                    *allowed == key
                }
            })
        })
        .collect();
    for (key, value) in configured {
        env.insert(key.into(), value.into());
    }
    env
}

/// A running server process and the handle that stops its tree.
pub(crate) struct ServerProcess {
    pub(crate) child: Child,
    pub(crate) stdin: ChildStdin,
    pub(crate) stdout: ChildStdout,
    pub(crate) stderr: Option<ChildStderr>,
    pub(crate) tree: ProcessTree,
}

pub(crate) fn spawn(
    program: OsString,
    args: &[String],
    env: BTreeMap<OsString, OsString>,
    cwd: Option<&Path>,
) -> std::io::Result<ServerProcess> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // stderr must never reach the terminal: it would corrupt the TUI.
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let (mut child, tree) = start_tree(command)?;
    let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
    let stderr = child.stderr.take();
    Ok(ServerProcess {
        child,
        stdin,
        stdout,
        stderr,
        tree,
    })
}

fn missing(what: &str) -> std::io::Error {
    std::io::Error::other(format!("server {what} was not captured"))
}

/// Kills the server's process tree. Dropping it kills too, so the tree is
/// gone even when a session ends on an error path.
pub(crate) struct ProcessTree {
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    job: Option<rc_core::windows_process::Job>,
}

impl ProcessTree {
    pub(crate) fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // SAFETY: the negative id addresses only the process group this
            // server leads (created by `setsid` in `start_tree`).
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            job.kill();
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(unix)]
fn start_tree(mut command: Command) -> std::io::Result<(Child, ProcessTree)> {
    // SAFETY: `setsid` is async-signal-safe and the closure allocates nothing
    // between fork and exec. A new session also detaches the server from the
    // terminal, so it cannot read keystrokes meant for the TUI.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = command.spawn()?;
    let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
    Ok((child, ProcessTree { pgid }))
}

#[cfg(windows)]
fn start_tree(mut command: Command) -> std::io::Result<(Child, ProcessTree)> {
    use rc_core::windows_process::{Job, CREATE_SUSPENDED};
    // The child starts suspended and joins the job before it runs, so nothing
    // it spawns can escape the job.
    command.creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let (Some(handle), Some(pid)) = (child.raw_handle(), child.id()) else {
        let _ = child.start_kill();
        return Err(std::io::Error::other(
            "server exited before it could be contained",
        ));
    };
    // SAFETY: the process was created with CREATE_SUSPENDED and is still owned.
    match unsafe { Job::attach_and_resume(handle, pid) } {
        Ok(job) => Ok((child, ProcessTree { job: Some(job) })),
        Err(error) => {
            let _ = child.start_kill();
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowlisted_and_configured_variables_reach_the_server() {
        let parent = [
            ("PATH", "/bin"),
            ("SC_API_KEY", "secret"),
            ("SUBCONSCIOUS_API_KEY", "secret"),
            ("LANG", "C.UTF-8"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let configured = BTreeMap::from([("TOKEN".to_string(), "abc".to_string())]);
        let env = server_env(parent, &configured);
        let keys: Vec<String> = env
            .keys()
            .map(|k| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(keys, ["LANG", "PATH", "TOKEN"]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_essentials_match_case_insensitively() {
        let parent = [
            ("Path", "C:\\Windows"),
            ("SYSTEMROOT", "C:\\Windows"),
            ("SC_MODEL", "m"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = server_env(parent, &BTreeMap::new());
        assert_eq!(env.len(), 2);
    }
}
