//! Path containment (§7.5): canonicalize after join, require the result under
//! an allowed root. Symlink escapes are caught because `canonicalize` resolves
//! the link physically — `./docs → /etc` canonicalizes to `/etc`, which is not
//! under a repo root. (TOCTOU-safe `openat2` RESOLVE_BENEATH on Linux is a later
//! refinement; this is the §7.5 "one function, used everywhere".)

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

fn root_canon(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn under_root(canon: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| canon.starts_with(root_canon(r)))
}

/// The "path outside allowed roots" error, naming the allowed roots so the
/// model can self-correct in one turn instead of guessing (the rejected path
/// alone tells it nothing about where it *is* allowed to write).
fn outside_err(canon: &Path, roots: &[PathBuf]) -> String {
    let allowed = roots
        .iter()
        .map(|r| root_canon(r).display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "path outside allowed roots: {} (allowed: {})",
        canon.display(),
        allowed
    )
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
        Err(outside_err(&canon, roots))
    }
}

/// Resolve a path that may not exist yet (Write creating a new file): walk up
/// to the nearest existing ancestor, canonicalize it, append the missing path
/// components, then scope-check. This permits `Write` to create nested parent
/// directories without weakening the symlink/root escape guard.
pub fn resolve_within_loose(
    roots: &[PathBuf],
    cwd: &Path,
    candidate: &str,
) -> Result<PathBuf, String> {
    let p = Path::new(candidate);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    if abs.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "path traversal with `..` is not allowed: {}",
            abs.display()
        ));
    }
    if let Ok(canon) = std::fs::canonicalize(&abs) {
        return if under_root(&canon, roots) {
            Ok(canon)
        } else {
            Err(outside_err(&canon, roots))
        };
    }
    let mut ancestor = abs.as_path();
    let mut missing: Vec<OsString> = Vec::new();
    let ancestor_canon = loop {
        match std::fs::canonicalize(ancestor) {
            Ok(canon) => break canon,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| format!("invalid path: {}", abs.display()))?;
                missing.push(name.to_os_string());
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| format!("invalid path: {}", abs.display()))?;
            }
            Err(e) => return Err(format!("{}: {e}", ancestor.display())),
        }
    };
    let mut canon = ancestor_canon;
    for component in missing.into_iter().rev() {
        canon.push(component);
    }
    if under_root(&canon, roots) {
        Ok(canon)
    } else {
        Err(outside_err(&canon, roots))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_resolution_allows_nested_missing_parents_inside_root() {
        let root = tempfile::tempdir().unwrap();
        let resolved = resolve_within_loose(
            &[root.path().to_path_buf()],
            root.path(),
            "new/nested/file.txt",
        )
        .unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(root.path())
                .unwrap()
                .join("new/nested/file.txt")
        );
    }

    #[test]
    fn loose_resolution_rejects_parent_traversal() {
        let root = tempfile::tempdir().unwrap();
        let err = resolve_within_loose(
            &[root.path().to_path_buf()],
            root.path(),
            "new/../../escape.txt",
        )
        .unwrap_err();
        assert!(err.contains("path traversal"), "{err}");
    }
}
