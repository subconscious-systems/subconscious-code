use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SC_BUILD_ID");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let explicit = std::env::var("SC_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let build_id = explicit.unwrap_or_else(detect_build_id);
    println!("cargo:rustc-env=SC_BUILD_ID={build_id}");
}

fn detect_build_id() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let revision = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "source".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(&root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty());
    if dirty {
        format!("{revision}-dirty-{}", source_fingerprint(&root))
    } else if revision == "source" {
        format!("source-{}", source_fingerprint(&root))
    } else {
        revision
    }
}

/// Reproducible identity for the inputs that can change the compiled harness.
/// Source archives intentionally omit `.git`, so a revision-only build id
/// would collapse every local benchmark bundle to the ambiguous word
/// `source`. FNV-1a is sufficient here: this is provenance, not authentication.
fn source_fingerprint(root: &std::path::Path) -> String {
    let mut files = Vec::new();
    collect_build_inputs(root, &mut files);
    files.sort();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for path in files {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        for byte in relative.to_string_lossy().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        if let Ok(contents) = std::fs::read(&path) {
            for byte in contents {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    format!("{hash:016x}")
}

fn collect_build_inputs(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if matches!(
                path.file_name().and_then(|part| part.to_str()),
                Some(".git" | "target" | ".venv" | "dist")
            ) {
                continue;
            }
            collect_build_inputs(&path, files);
        } else {
            let name = path.file_name().and_then(|name| name.to_str());
            let rust = path.extension().is_some_and(|extension| extension == "rs");
            if rust || matches!(name, Some("Cargo.toml" | "Cargo.lock")) {
                files.push(path);
            }
        }
    }
}
