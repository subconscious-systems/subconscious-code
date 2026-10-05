//! SIGTERM to a headless `marathon -p` stops its MCP servers and their
//! children, even a server that ignores stdin EOF and SIGTERM.
#![cfg(unix)]

use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A minimal MCP server in POSIX sh: it records its pid and a grandchild's,
/// answers `initialize` and `tools/list`, ignores SIGTERM, and keeps running
/// after stdin closes, the cases that leak servers without explicit cleanup.
const SERVER: &str = r#"#!/bin/sh
trap '' TERM HUP INT
sleep 600 &
echo "$$ $!" > "$PIDFILE"
while :; do
  if IFS= read -r line; then
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$line" in
      *'"initialize"'*)
        pv=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{"tools":{}},"serverInfo":{"name":"sh","version":"0"}}}\n' "$id" "$pv" ;;
      *'"tools/list"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"noop","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    esac
  else
    sleep 600
  fi
done
"#;

fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    out.status.success() && !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

fn wait_for(path: &Path, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.split_whitespace().count() == 2 {
                return text;
            }
        }
        assert!(Instant::now() < deadline, "server never started");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sigterm_stops_mcp_servers_and_their_children() {
    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().join("server.sh");
    std::fs::write(&server, SERVER).unwrap();
    let pidfile = dir.path().join("pids");
    let config = serde_json::json!({"mcpServers": {"sh": {
        "command": "/bin/sh",
        "args": [server.display().to_string()],
        "env": {"PIDFILE": pidfile.display().to_string()},
    }}});

    // A model endpoint that accepts and never answers keeps the run going.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });

    let mut marathon = Command::new(env!("CARGO_BIN_EXE_marathon"))
        .args(["-p", "hello", "--strict-mcp-config", "--mcp-config"])
        .arg(config.to_string())
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("SC_API_KEY", "test-key")
        .env("SC_BASE_URL", &base_url)
        .env("SC_RESOURCE_LIMITS", "0")
        .env_remove("SC_RESOURCE_SCOPE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let pids: Vec<u32> = wait_for(&pidfile, Duration::from_secs(20))
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(pids.iter().all(|&p| alive(p)), "{pids:?} should be running");
    // Let Marathon finish connecting and install its signal handler.
    std::thread::sleep(Duration::from_millis(1500));

    // SAFETY: plain kill(2) on the child we spawned.
    unsafe {
        libc::kill(marathon.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = marathon.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "marathon did not exit on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    marathon
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{stderr}");

    let deadline = Instant::now() + Duration::from_secs(10);
    while pids.iter().any(|&p| alive(p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        pids.iter().all(|&p| !alive(p)),
        "{pids:?} survived SIGTERM: {stderr}"
    );
}
