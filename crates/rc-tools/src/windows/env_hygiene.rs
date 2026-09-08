//! Windows shell selection. No change to Unix shell discovery or runbooks.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    Bash,
}

pub fn shell_kind() -> Result<ShellKind, String> {
    match std::env::var("SC_WINDOWS_SHELL")
        .unwrap_or_else(|_| "powershell".into())
        .to_lowercase()
        .as_str()
    {
        "powershell" => Ok(ShellKind::PowerShell),
        "bash" => Ok(ShellKind::Bash),
        _ => Err("SC_WINDOWS_SHELL must be powershell (default) or bash".into()),
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(name))
        .find(|p| p.is_file())
}

pub fn resolve_shell(kind: ShellKind) -> Result<PathBuf, String> {
    let override_name = match kind {
        ShellKind::PowerShell => "SC_POWERSHELL_PATH",
        ShellKind::Bash => "SC_GIT_BASH_PATH",
    };
    if let Some(value) = std::env::var_os(override_name) {
        let path = PathBuf::from(value);
        if path.is_absolute()
            && path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Ok(path);
        }
        return Err(format!(
            "{override_name} must be an absolute path to an existing .exe"
        ));
    }
    let mut candidates = Vec::new();
    if kind == ShellKind::PowerShell {
        if let Some(pwsh) = find_in_path("pwsh.exe") {
            return Ok(pwsh);
        }
        if let Some(root) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(root).join("PowerShell/7/pwsh.exe"));
        }
        if let Some(root) = std::env::var_os("SystemRoot") {
            candidates
                .push(PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        }
    } else {
        // Prefer Git for Windows, not the unrelated System32 WSL bash launcher.
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(var) {
                candidates.push(PathBuf::from(&root).join("Git/bin/bash.exe"));
                candidates.push(PathBuf::from(root).join("Programs/Git/bin/bash.exe"));
            }
        }
        if let Some(git) = find_in_path("git.exe") {
            if let Some(root) = git.parent().and_then(Path::parent) {
                candidates.push(root.join("bin/bash.exe"));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| match kind {
        ShellKind::PowerShell => "PowerShell not found; set SC_POWERSHELL_PATH to powershell.exe or pwsh.exe".into(),
        ShellKind::Bash => "Git Bash was selected but is not installed; install Git for Windows or use SC_WINDOWS_SHELL=powershell. A custom installation can use SC_GIT_BASH_PATH.".into(),
    })
}

pub fn rehydrated_path_env() -> OsString {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE") {
        for suffix in [".local/bin", ".cargo/bin"] {
            dirs.push(PathBuf::from(&home).join(suffix));
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(conda) = std::env::var_os("CONDA_PREFIX") {
        dirs.push(PathBuf::from(&conda));
        dirs.push(PathBuf::from(conda).join("Scripts"));
    }
    dirs.retain(|dir| dir.is_dir());
    let current = std::env::var_os("PATH").unwrap_or_default();
    dirs.extend(std::env::split_paths(&current));
    std::env::join_paths(dirs).unwrap_or(current)
}
