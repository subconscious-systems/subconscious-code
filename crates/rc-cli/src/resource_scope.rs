//! Linux host-protection for agent sessions.
//!
//! Each `sc` process re-enters itself inside a transient systemd user scope when
//! available. Minimal containers fall back to an inherited `RLIMIT_AS`. Either
//! ceiling covers the editor and every tool descendant, including detached
//! background commands, so two runaway builds cannot push the whole host into
//! global reclaim. This limits local process resources only; model context is
//! untouched.

use std::ffi::OsString;
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const GIB: u64 = 1024 * 1024 * 1024;
static OBSERVED_PEAK_BYTES: AtomicU64 = AtomicU64::new(0);
static PRESSURE_TERMINATED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy)]
pub(crate) struct PressureSnapshot {
    pub current_bytes: u64,
    pub max_bytes: u64,
    pub percent: u64,
}

/// Samples the whole cgroup (or the current process under RLIMIT fallback),
/// emits escalating telemetry, and requests graceful cancellation before the
/// kernel's hard OOM boundary. Three consecutive high samples avoid killing a
/// session for a transient allocation spike.
pub(crate) struct ResourceMonitor {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ResourceMonitor {
    pub(crate) fn start(on_terminate: impl Fn(PressureSnapshot) + Send + 'static) -> Option<Self> {
        let max = std::env::var("SC_RESOURCE_MEMORY_MAX_BYTES")
            .ok()?
            .parse::<u64>()
            .ok()?
            .max(1);
        let threshold = env_u64("SC_RESOURCE_TERMINATE_PERCENT")
            .unwrap_or(90)
            .clamp(50, 99);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let cgroup = cgroup_path();
            let mut consecutive = 0u8;
            let mut warned_at = 0u64;
            while !thread_stop.load(Ordering::Relaxed) {
                std::thread::park_timeout(std::time::Duration::from_secs(1));
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some(current) = memory_current(cgroup.as_deref()) else {
                    continue;
                };
                OBSERVED_PEAK_BYTES.fetch_max(current, Ordering::Relaxed);
                let percent = current.saturating_mul(100) / max;
                for boundary in [75, 85, threshold] {
                    if percent >= boundary && warned_at < boundary {
                        eprintln!(
                            "resource pressure: memory {percent}% ({} / {} MiB)",
                            current / (1024 * 1024),
                            max / (1024 * 1024)
                        );
                        warned_at = boundary;
                    }
                }
                if percent >= threshold {
                    consecutive = consecutive.saturating_add(1);
                } else {
                    consecutive = 0;
                }
                if consecutive >= 3 {
                    PRESSURE_TERMINATED.store(true, Ordering::Relaxed);
                    on_terminate(PressureSnapshot {
                        current_bytes: current,
                        max_bytes: max,
                        percent,
                    });
                    return;
                }
            }
        });
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

pub(crate) fn observed_peak_bytes() -> Option<u64> {
    let peak = OBSERVED_PEAK_BYTES.load(Ordering::Relaxed);
    (peak > 0).then_some(peak)
}

pub(crate) fn pressure_terminated() -> bool {
    PRESSURE_TERMINATED.load(Ordering::Relaxed)
}

fn cgroup_path() -> Option<std::path::PathBuf> {
    let contents = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = contents.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(std::path::Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
}

fn memory_current(cgroup: Option<&std::path::Path>) -> Option<u64> {
    cgroup
        .and_then(|path| std::fs::read_to_string(path.join("memory.current")).ok())
        .and_then(|value| value.trim().parse().ok())
        .or_else(|| {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            let kib = status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })?;
            kib.checked_mul(1024)
        })
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    memory_high: u64,
    memory_max: u64,
    swap_max: u64,
    tasks_max: u64,
    cpu_quota_percent: u64,
}

/// Re-enter the current command inside a transient systemd scope when the host
/// supports user services. `None` means containment is unavailable/disabled
/// and the caller should continue normally. `Some` is the child exit status;
/// the caller must return it rather than running the command twice.
pub(crate) fn maybe_reexec() -> Option<ExitCode> {
    if !cfg!(target_os = "linux")
        || std::env::var_os("SC_RESOURCE_SCOPE").is_some()
        || std::env::var_os("SC_RESOURCE_LIMITS_APPLIED").is_some()
        || env_disabled("SC_RESOURCE_LIMITS")
    {
        return None;
    }

    // Containers and minimal SSH environments commonly have no user manager.
    // Use an inherited address-space ceiling there; full cgroup accounting and
    // CPU/task controls remain available on hosts with a user manager.
    let manager = std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .is_some_and(|status| status.success());
    if !manager {
        apply_fallback(limits());
        return None;
    }

    let limits = limits();
    let unit = format!("sc-{}-{}.scope", std::process::id(), epoch_nanos());
    let executable = std::env::current_exe().ok()?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut command = std::process::Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg(format!("--unit={unit}"))
        .arg(format!("--property=MemoryHigh={}", limits.memory_high))
        .arg(format!("--property=MemoryMax={}", limits.memory_max))
        .arg(format!("--property=MemorySwapMax={}", limits.swap_max))
        .arg(format!("--property=TasksMax={}", limits.tasks_max))
        .arg(format!("--property=CPUQuota={}%", limits.cpu_quota_percent))
        .args(["--property=CPUWeight=50", "--property=IOWeight=50"])
        .arg("--setenv=SC_RESOURCE_SCOPE=1")
        .arg(format!("--setenv=SC_RESOURCE_SCOPE_UNIT={unit}"))
        .arg(format!(
            "--setenv=SC_RESOURCE_MEMORY_MAX_BYTES={}",
            limits.memory_max
        ))
        .arg(executable)
        .args(args);

    let status = match command.status() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("warning: could not start resource scope: {error}");
            apply_fallback(limits);
            return None;
        }
    };
    Some(
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or_else(|| ExitCode::from(1), ExitCode::from),
    )
}

#[cfg(target_os = "linux")]
fn apply_fallback(limits: Limits) {
    // Minimal containers frequently lack a user systemd manager. RLIMIT_AS is
    // inherited by every tool descendant and gives those environments a real
    // memory backstop instead of silently running uncontained. CPU/tasks remain
    // the responsibility of the container runtime because RLIMIT_NPROC is
    // user-global and RLIMIT_CPU is cumulative CPU time, not a quota.
    let mut existing = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: both calls operate on the current process with an initialized
    // `rlimit`; descendants inherit the resulting ceiling.
    let applied_max = unsafe {
        if libc::getrlimit(libc::RLIMIT_AS, &mut existing) != 0 {
            None
        } else {
            let requested = limits.memory_max as libc::rlim_t;
            let hard = if existing.rlim_max == libc::RLIM_INFINITY {
                requested
            } else {
                requested.min(existing.rlim_max)
            };
            let soft = if existing.rlim_cur == libc::RLIM_INFINITY {
                hard
            } else {
                hard.min(existing.rlim_cur)
            };
            if libc::setrlimit(
                libc::RLIMIT_AS,
                &libc::rlimit {
                    rlim_cur: soft,
                    rlim_max: hard,
                },
            ) == 0
            {
                Some(soft as u64)
            } else {
                None
            }
        }
    };
    if let Some(memory_max) = applied_max {
        std::env::set_var("SC_RESOURCE_LIMITS_APPLIED", "rlimit-as");
        std::env::set_var("SC_RESOURCE_SCOPE_UNIT", "rlimit-as");
        std::env::set_var("SC_RESOURCE_MEMORY_MAX_BYTES", memory_max.to_string());
    } else {
        eprintln!("warning: systemd scope unavailable and RLIMIT_AS could not be applied");
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_fallback(_limits: Limits) {}

fn limits() -> Limits {
    let total = total_memory_bytes().unwrap_or(16 * GIB);
    // The scope includes compilers and test runners launched by the editor, but
    // it should still leave ample headroom for concurrent sessions and the
    // model gateway. Large hosts do not justify an equally large editor budget.
    let default_max = (total / 8).clamp(4 * GIB, 12 * GIB);
    let memory_max = env_mebibytes("SC_RESOURCE_MEMORY_MAX_MB").unwrap_or(default_max);
    let memory_high = env_mebibytes("SC_RESOURCE_MEMORY_HIGH_MB")
        .unwrap_or(memory_max.saturating_mul(3) / 4)
        .min(memory_max);
    let swap_max = env_mebibytes("SC_RESOURCE_SWAP_MAX_MB").unwrap_or(2 * GIB);
    let tasks_max = env_u64("SC_RESOURCE_TASKS_MAX").unwrap_or(512).max(16);
    let default_cpus = std::thread::available_parallelism()
        .map(|cpus| cpus.get() as u64)
        .unwrap_or(2)
        .div_ceil(2)
        .clamp(1, 8);
    let cpu_quota_percent = env_u64("SC_RESOURCE_CPU_QUOTA_PERCENT")
        .unwrap_or(default_cpus * 100)
        .clamp(100, 6400);
    Limits {
        memory_high,
        memory_max,
        swap_max,
        tasks_max,
        cpu_quota_percent,
    }
}

fn total_memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    kib.checked_mul(1024)
}

fn env_mebibytes(name: &str) -> Option<u64> {
    env_u64(name)?.checked_mul(1024 * 1024)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_disabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| matches!(value.trim(), "0" | "false" | "off" | "no"))
}

fn epoch_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_default_never_exceeds_12_gib() {
        let max = (121 * GIB / 8).clamp(4 * GIB, 12 * GIB);
        assert_eq!(max, 12 * GIB);
    }

    #[test]
    fn small_hosts_retain_a_host_reserve() {
        let max = (16 * GIB / 8).clamp(4 * GIB, 12 * GIB);
        assert_eq!(max, 4 * GIB);
    }

    #[test]
    fn medium_hosts_scale_below_the_cap() {
        let max = (64 * GIB / 8).clamp(4 * GIB, 12 * GIB);
        assert_eq!(max, 8 * GIB);
    }
}
