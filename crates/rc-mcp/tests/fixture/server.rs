//! A minimal stdio MCP server for rc-mcp's integration tests. It speaks just
//! enough JSON-RPC to initialize, list tools, and answer calls:
//!
//! - `echo {text}`  returns the text
//! - `fail`         returns an `isError` result
//! - `crash`        exits the process mid-call
//! - `sleep {ms}`   answers after a delay
//! - `env`          lists the variable names it was started with
//! - `family`       starts a long-lived grandchild and returns `<pid> <grandchild pid>`
//!
//! `FIXTURE_STDERR` is written to stderr at startup, and `FIXTURE_EXIT_EARLY=1`
//! exits before initializing, so tests can check startup failures.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--sleep") {
        std::thread::sleep(std::time::Duration::from_secs(600));
        return;
    }
    if let Ok(text) = std::env::var("FIXTURE_STDERR") {
        eprintln!("{text}");
    }
    if std::env::var("FIXTURE_EXIT_EARLY").as_deref() == Ok("1") {
        std::process::exit(3);
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue; // a notification
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rc-mcp-fixture", "version": "0.0.0"},
            }),
            "tools/list" => json!({"tools": [
                {"name": "echo", "description": "Echo the text back.",
                 "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
                {"name": "fail", "description": "Always reports an error.", "inputSchema": {"type": "object"}},
                {"name": "crash", "description": "Exits the server.", "inputSchema": {}},
                {"name": "env", "description": "Lists environment variable names.", "inputSchema": {"type": "object"}},
                {"name": "family", "description": "Starts a grandchild.", "inputSchema": {"type": "object"}},
                {"name": "sleep", "description": "Waits, then answers.",
                 "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}}},
            ]}),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                match name {
                    "echo" => json!({"content": [{"type": "text",
                        "text": format!("echo: {}", args.get("text").and_then(Value::as_str).unwrap_or(""))}]}),
                    "fail" => {
                        json!({"content": [{"type": "text", "text": "it went wrong"}], "isError": true})
                    }
                    "crash" => {
                        eprintln!("fixture crashing on purpose");
                        std::process::exit(7);
                    }
                    "env" => {
                        let mut names: Vec<String> = std::env::vars_os()
                            .map(|(k, _)| k.to_string_lossy().into_owned())
                            .collect();
                        names.sort();
                        json!({"content": [{"type": "text", "text": names.join("\n")}]})
                    }
                    "family" => {
                        let exe = std::env::current_exe().expect("fixture path");
                        let mut grandchild = std::process::Command::new(exe)
                            .arg("--sleep")
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .expect("spawn grandchild");
                        let grandchild_pid = grandchild.id();
                        // Reaped here if it ever exits; the test kills it.
                        std::thread::spawn(move || {
                            let _ = grandchild.wait();
                        });
                        json!({"content": [{"type": "text",
                            "text": format!("{} {grandchild_pid}", std::process::id())}]})
                    }
                    "sleep" => {
                        let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(0);
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        json!({"content": [{"type": "text", "text": format!("slept {ms}")}]})
                    }
                    _ => {
                        reply(
                            &mut stdout,
                            json!({"jsonrpc": "2.0", "id": id,
                            "error": {"code": -32602, "message": format!("unknown tool {name}")}}),
                        );
                        continue;
                    }
                }
            }
            "ping" => json!({}),
            _ => {
                reply(
                    &mut stdout,
                    json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("unknown method {method}")}}),
                );
                continue;
            }
        };
        reply(
            &mut stdout,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        );
    }
}

fn reply(stdout: &mut std::io::Stdout, message: Value) {
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{message}");
    let _ = handle.flush();
}
