//! Shared tool helpers: schema generation, path resolution + scope check,
//! read-registry check/recording, atomic writes, line-ending preservation,
//! output capping, ANSI stripping, and the Bash safety floor.
//!
//! Path containment (resolve_within/loose) is re-exported from rc-perm (§7.5 —
//! one function, used everywhere). The Bash safety floor below is a conservative
//! deny-list that over-refuses; the rc-perm engine (M3) is authoritative.

use rc_core::{ToolCtx, ToolOutcome};
use schemars::JsonSchema;
use serde_json::Value;
use std::path::Path;
use std::time::SystemTime;

/// Generate the JSON Schema `parameters` for a `JsonSchema` type, stripping
/// `$schema`/`title`. Canonical serialization (§4.6) makes the byte form stable.
pub fn params_schema<T: JsonSchema>() -> Value {
    let root = schemars::schema_for!(T);
    let mut v = serde_json::to_value(&root).expect("schema is serializable");
    if let Value::Object(map) = &mut v {
        map.remove("$schema");
        map.remove("title");
    }
    v
}

pub fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Record a read (path -> (mtime, blake3)) in the shared registry.
pub fn record_read(ctx: &ToolCtx, canon: &Path) {
    let mtime = mtime_of(canon).unwrap_or(SystemTime::UNIX_EPOCH);
    let bytes = std::fs::read(canon).unwrap_or_default();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    if let Ok(mut reg) = ctx.read_registry.lock() {
        reg.record(canon.to_path_buf(), mtime, hash);
    }
}

/// Enforce "read before mutate" (§6.2/§6.3): the file must have been read, and
/// must be unchanged since (mtime **and** content hash). Returns `Some(error)`
/// to surface to the model when the check fails.
pub fn require_current_read(ctx: &ToolCtx, canon: &Path) -> Option<ToolOutcome> {
    let recorded = ctx
        .read_registry
        .lock()
        .ok()
        .and_then(|reg| reg.get(canon).cloned());
    let Some((reg_mtime, reg_hash)) = recorded else {
        return Some(ToolOutcome::Error {
            message: format!("{} — read it with `Read` before mutating it", canon.display()),
            retryable: false,
        });
    };
    let cur_mtime = mtime_of(canon);
    let cur_hash = blake3::hash(&std::fs::read(canon).unwrap_or_default())
        .to_hex()
        .to_string();
    if cur_mtime != Some(reg_mtime) || cur_hash != reg_hash {
        return Some(ToolOutcome::Error {
            message: format!("{} changed since the last `Read` — re-read it first", canon.display()),
            retryable: false,
        });
    }
    None
}

// Path containment lives in rc-perm now (§7.5 — one function, used everywhere).
pub use rc_perm::{resolve_within, resolve_within_loose};

/// Atomic write: temp file in the same dir -> fsync -> rename (§6.2).
pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, content.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(std::io::Error::other)?;
    Ok(())
}

/// Preserve the existing file's line-ending style (CRLF/LF) and trailing-newline
/// presence (§6.2). A new file (no `old`) gets a trailing newline.
pub fn preserve_line_endings(old: Option<&str>, new: &str) -> String {
    let Some(old) = old else {
        let mut s = new.to_string();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        return s;
    };
    let crlf = old.contains("\r\n");
    let old_trailing = old.ends_with('\n');
    let mut s = new.replace("\r\n", "\n"); // normalize, then convert
    if crlf {
        s = s.replace("\n", "\r\n");
    }
    let trailing = s.ends_with('\n');
    if old_trailing && !trailing {
        s.push_str(if crlf { "\r\n" } else { "\n" });
    } else if !old_trailing && trailing {
        if crlf && s.ends_with("\r\n") {
            s.truncate(s.len() - 2);
        } else if s.ends_with('\n') {
            s.truncate(s.len() - 1);
        }
    }
    s
}

/// Cap output to `cap` chars: keep the first `head` + last `tail`, noting how
/// many chars were elided. Returns (was_truncated, body).
pub fn cap_output(s: &str, cap: usize, head: usize, tail: usize) -> (bool, String) {
    let n = s.chars().count();
    if n <= cap {
        return (false, s.to_string());
    }
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    (true, format!("{h}\n… [{} chars elided] …\n{t}", n - head - tail))
}

/// Strip ANSI CSI escape sequences (SGR colors, cursors, etc.) — pure token
/// waste in tool output (§6.6).
pub fn strip_ansi(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap());
    re.replace_all(s, "").to_string()
}

/// Conservative catastrophic-command deny-list (M2 safety floor). Over-refuses
/// rather than under-refuses; M3 replaces this with real command parsing + prompts.
pub fn dangerous_command(cmd: &str) -> Option<&'static str> {
    const CATACLYSMIC: &[&str] = &[
        "rm -rf /",
        "rm -rf ~",
        "rm -rf /*",
        "rm -fr /",
        "rm -fr ~",
        "rm -fr /*",
        "rm -rf $HOME",
        "rm -rf $PWD",
        "mkfs",
        "dd of=/dev/",
        "chmod -R 777 /",
        ":(){:|:&};:",
        "shutdown",
        "reboot",
        "halt -p",
        "init 0",
    ];
    for p in CATACLYSMIC {
        if cmd.contains(p) {
            return Some("command refused by the M2 safety floor (destructive); M3 adds the real permission engine");
        }
    }
    let trimmed = cmd.trim_start();
    if trimmed.starts_with("sudo ") || trimmed == "sudo" || trimmed.starts_with("sudo\t") {
        return Some("sudo is not permitted");
    }
    None
}

#[cfg(test)]
pub(crate) fn test_ctx(dir: &Path) -> ToolCtx {
    use rc_core::state::ReadRegistry;
    use rc_core::{ShellState, ChangeJournal};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;
    ToolCtx {
        cwd: dir.to_path_buf(),
        allowed_roots: vec![dir.to_path_buf()],
        cancel: CancellationToken::new(),
        read_registry: Arc::new(Mutex::new(ReadRegistry::new())),
        shell_state: Arc::new(Mutex::new(ShellState::new(dir.to_path_buf()))),
        change_journal: Arc::new(Mutex::new(ChangeJournal::new())),
        sandbox: None,
    }
}
