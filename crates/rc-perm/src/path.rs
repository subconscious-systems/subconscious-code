//! Path containment (§7.5): canonicalize after join, require the result under
//! an allowed root. Symlink escapes are caught because `canonicalize` resolves
//! the link physically — `./docs → /etc` canonicalizes to `/etc`, which is not
//! under a repo root. (TOCTOU-safe `openat2` RESOLVE_BENEATH on Linux is a later
//! refinement; this is the §7.5 "one function, used everywhere".)

use std::path::{Path, PathBuf};

fn root_canon(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn under_root(canon: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| canon.starts_with(root_canon(r)))
}

/// Resolve a path that must already exist (Read/Edit/Grep/Glob): canonicalize
/// physically and require the result under an allowed root.
pub fn resolve_within(roots: &[PathBuf], cwd: &Path, candidate: &str) -> Result<PathBuf, String> {
    let p = Path::new(candidate);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let canon = std::fs::canonicalize(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
    if under_root(&canon, roots) {
        Ok(canon)
    } else {
        Err(format!("path outside allowed roots: {}", canon.display()))
    }
}

/// Resolve a path that may not exist yet (Write creating a new file):
/// canonicalize the parent, append the file name, then scope-check.
pub fn resolve_within_loose(roots: &[PathBuf], cwd: &Path, candidate: &str) -> Result<PathBuf, String> {
    let p = Path::new(candidate);
    let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    if let Ok(canon) = std::fs::canonicalize(&abs) {
        return if under_root(&canon, roots) {
            Ok(canon)
        } else {
            Err(format!("path outside allowed roots: {}", canon.display()))
        };
    }
    let parent = abs.parent().unwrap_or(Path::new("."));
    let file_name = abs.file_name().ok_or_else(|| "invalid path".to_string())?;
    let parent_canon = std::fs::canonicalize(parent).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "parent directory {} does not exist — create it first (e.g. `Bash` mkdir -p {})",
                parent.display(),
                parent.display()
            )
        } else {
            format!("{}: {e}", parent.display())
        }
    })?;
    let canon = parent_canon.join(file_name);
    if under_root(&canon, roots) {
        Ok(canon)
    } else {
        Err(format!("path outside allowed roots: {}", canon.display()))
    }
}
