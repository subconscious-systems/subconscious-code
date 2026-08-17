//! Env hygiene for the `Bash` tool (M7 / §6.6).
//!
//! Two concerns:
//! - **Strip rc**: run `bash --noprofile --norc` when bash is available so the
//!   shell doesn't load the user's interactive rc (nvm/conda/pyenv shims in
//!   `.bashrc` would otherwise leak in opaquely).
//! - **Rehydrate toolchains**: prepend the detected toolchain bin dirs (nvm
//!   default, pyenv shims, conda, `~/.local/bin`) to `PATH` so the agent still
//!   finds `node`/`python`/etc. without sourcing rc files.
//!
//! Both are applied via `Command`-level config, never by rewriting the command
//! text — the permission layer parses the verbatim `command` string, so a PATH
//! prefix must not be spliced into it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Resolve the shell to invoke and its non-interactive args.
///
/// Prefers `bash` (with `--noprofile --norc`) since those flags are bash-only;
/// falls back to `$SHELL` (inheriting its rc) and then `/bin/sh`. Cached for the
/// process lifetime — the choice doesn't change mid-run.
pub fn resolve_shell() -> (PathBuf, Vec<String>) {
    static SHELL: OnceLock<(PathBuf, Vec<String>)> = OnceLock::new();
    SHELL
        .get_or_init(|| {
            if let Some(bash) = find_in_path("bash") {
                return (bash, vec!["--noprofile".into(), "--norc".into()]);
            }
            if let Ok(shell) = std::env::var("SHELL") {
                let p = PathBuf::from(&shell);
                if p.exists() {
                    return (p, Vec::new());
                }
            }
            (PathBuf::from("/bin/sh"), Vec::new())
        })
        .clone()
}

/// Look up an executable by name on `PATH` (POSIX semantics: any existing,
/// executable file in a `PATH` dir). Returns the first match.
fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && (meta.permissions().mode() & 0o111) != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

/// The toolchain bin dirs that exist under `home` (plus an optional active conda
/// prefix). Pure over its inputs so it's testable without mutating process env.
///
/// Order is most-specific first: nvm default → pyenv shims → conda → `~/.local/bin`.
/// Only dirs that actually exist are returned (a missing toolchain is silently
/// skipped — best-effort rehydration).
pub fn toolchain_dirs_for(home: &Path, conda_prefix: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };

    // nvm: resolve the default alias to a version's bin dir.
    if let Some(default) = read_trim(home.join(".nvm").join("alias").join("default")) {
        push(
            home.join(".nvm")
                .join("versions")
                .join("node")
                .join(default)
                .join("bin"),
        );
    }
    push(home.join(".pyenv").join("shims"));
    if let Some(cp) = conda_prefix {
        push(cp.join("bin"));
    }
    push(home.join("miniconda3").join("bin"));
    push(home.join("anaconda3").join("bin"));
    push(home.join(".local").join("bin"));

    out
}

fn read_trim(p: PathBuf) -> Option<String> {
    let s = std::fs::read_to_string(&p).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        // nvm aliases can be `lts/iron` or `node` → no version dir; only keep
        // things that plausibly map to a `versions/node/<x>` dir (contain a
        // digit). Skip pure aliases so we don't construct a bogus path.
        if t.chars().any(|c| c.is_ascii_digit()) {
            Some(t.to_string())
        } else {
            None
        }
    }
}

/// Build the rehydrated `PATH` by prepending the detected toolchain dirs to the
/// current `PATH`. Returns the new value for `Command::env("PATH", …)`.
pub fn rehydrate_path(cur: &OsStr, dirs: &[PathBuf]) -> OsString {
    if dirs.is_empty() {
        return cur.to_os_string();
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut s = OsString::new();
    for d in dirs {
        if !s.is_empty() {
            s.push(sep);
        }
        s.push(d.as_os_str());
    }
    s.push(sep);
    s.push(cur);
    s
}

/// Convenience: rehydrated PATH from the process env (for `Bash::call`).
pub fn rehydrated_path_env() -> OsString {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let conda_prefix = std::env::var_os("CONDA_PREFIX").map(PathBuf::from);
    let dirs = home
        .as_deref()
        .map(|h| toolchain_dirs_for(h, conda_prefix.as_deref()))
        .unwrap_or_default();
    let cur = std::env::var_os("PATH").unwrap_or_default();
    rehydrate_path(&cur, &dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn mkdir(p: &Path) {
        fs::create_dir_all(p).unwrap();
    }
    fn mkexec(p: &Path) {
        fs::write(p, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(p).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn toolchain_dirs_returns_only_existing_bins_in_order() {
        let home = tempdir().unwrap();
        let h = home.path();
        // nvm default -> versions/node/20.1.0/bin
        mkdir(&h.join(".nvm/alias"));
        fs::write(h.join(".nvm/alias/default"), "20.1.0\n").unwrap();
        mkdir(&h.join(".nvm/versions/node/20.1.0/bin"));
        // pyenv shims
        mkdir(&h.join(".pyenv/shims"));
        // ~/.local/bin
        mkdir(&h.join(".local/bin"));

        let dirs = toolchain_dirs_for(h, None);
        assert_eq!(
            dirs,
            vec![
                h.join(".nvm/versions/node/20.1.0/bin"),
                h.join(".pyenv/shims"),
                h.join(".local/bin"),
            ]
        );
    }

    #[test]
    fn toolchain_dirs_skips_missing_and_non_version_nvm_aliases() {
        let home = tempdir().unwrap();
        let h = home.path();
        // An alias that doesn't resolve to a version dir (e.g. "lts/iron") is
        // skipped; only a digit-bearing alias maps to a version bin.
        fs::create_dir_all(h.join(".nvm/alias")).unwrap();
        fs::write(h.join(".nvm/alias/default"), "lts/iron\n").unwrap();
        // No version dirs exist; nothing from nvm.
        mkdir(&h.join(".local/bin"));
        let dirs = toolchain_dirs_for(h, None);
        assert_eq!(dirs, vec![h.join(".local/bin")]);
    }

    #[test]
    fn toolchain_dirs_includes_conda_prefix() {
        let home = tempdir().unwrap();
        let h = home.path();
        let conda = tempdir().unwrap();
        mkdir(&conda.path().join("bin"));
        mkdir(&h.join(".local/bin"));
        let dirs = toolchain_dirs_for(h, Some(conda.path()));
        assert_eq!(dirs, vec![conda.path().join("bin"), h.join(".local/bin")]);
    }

    #[test]
    fn rehydrate_path_prefixes_dirs() {
        let dirs = vec![PathBuf::from("/nvm/bin"), PathBuf::from("/pyenv/shims")];
        let got = rehydrate_path(OsStr::new("/usr/bin:/bin"), &dirs);
        assert_eq!(got.to_string_lossy(), "/nvm/bin:/pyenv/shims:/usr/bin:/bin");
    }

    #[test]
    fn rehydrate_path_empty_dirs_passes_through() {
        let got = rehydrate_path(OsStr::new("/usr/bin:/bin"), &[]);
        assert_eq!(got.to_string_lossy(), "/usr/bin:/bin");
    }

    #[test]
    fn find_in_path_locates_an_executable() {
        // `sh` is present on every Unix CI host. Verify the lookup honors
        // executability (a non-exec file with the same name would be skipped).
        let dir = tempdir().unwrap();
        let bin = dir.path().join("bin");
        mkdir(&bin);
        // executable
        mkexec(&bin.join("mytool"));
        // non-executable same name in a later dir — must not win over the exec
        let bin2 = dir.path().join("bin2");
        mkdir(&bin2);
        fs::write(bin2.join("mytool"), b"nope").unwrap();

        let saved = std::env::var_os("PATH");
        let new_path = format!("{}:{}", bin.display(), bin2.display());
        std::env::set_var("PATH", &new_path);
        let found = find_in_path("mytool");
        // Restore ASAP — env vars are process-global.
        if let Some(s) = saved {
            std::env::set_var("PATH", s);
        } else {
            std::env::remove_var("PATH");
        }
        assert_eq!(found, Some(bin.join("mytool")));
    }
}
