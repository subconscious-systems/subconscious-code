//! A minimal MCP server for rc-mcp's integration tests. It speaks just enough
//! JSON-RPC to initialize, list tools, and answer calls:
//!
//! - `echo {text}`  returns the text
//! - `fail`         returns an `isError` result
//! - `crash`        exits the process mid-call
//! - `sleep {ms}`   answers after a delay
//! - `env`          lists the variable names it was started with
//! - `family`       starts a long-lived grandchild and returns `<pid> <grandchild pid>`
//! - `whoami`       (HTTP only) returns the request's `Authorization` header
//!
//! By default it serves stdio. `--http` serves streamable HTTP on a free
//! localhost port instead (stateless, JSON responses) and prints
//! `listening <url>` on stdout. `FIXTURE_STDERR` is written to stderr at
//! startup, and `FIXTURE_EXIT_EARLY=1` exits before initializing.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--sleep") => {
            std::thread::sleep(std::time::Duration::from_secs(600));
            return;
        }
        Some("--http") => return serve_http(),
        _ => {}
    }
    if let Ok(text) = std::env::var("FIXTURE_STDERR") {
        eprintln!("{text}");
    }
    if std::env::var("FIXTURE_EXIT_EARLY").as_deref() == Ok("1") {
        std::process::exit(3);
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(response) = respond(&message, None) {
            let mut handle = stdout.lock();
            let _ = writeln!(handle, "{response}");
            let _ = handle.flush();
        }
    }
}

/// The JSON-RPC response to `message`, or `None` for a notification.
/// `authorization` is the HTTP request's header, when served over HTTP.
fn respond(message: &Value, authorization: Option<&str>) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let error = |code: i64, text: String| {
        Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}}))
    };
    let text = |text: String| json!({"content": [{"type": "text", "text": text}]});
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "rc-mcp-fixture", "version": "0.0.0"},
        }),
        "tools/list" => {
            let mut tools = vec![
                json!({"name": "echo", "description": "Echo the text back.",
                 "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
                json!({"name": "fail", "description": "Always reports an error.", "inputSchema": {"type": "object"}}),
            ];
            if authorization.is_some() {
                tools.push(json!({"name": "whoami", "description": "Returns the Authorization header.", "inputSchema": {"type": "object"}}));
            } else {
                tools.extend([
                    json!({"name": "crash", "description": "Exits the server.", "inputSchema": {}}),
                    json!({"name": "env", "description": "Lists environment variable names.", "inputSchema": {"type": "object"}}),
                    json!({"name": "family", "description": "Starts a grandchild.", "inputSchema": {"type": "object"}}),
                    json!({"name": "sleep", "description": "Waits, then answers.",
                     "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}}}),
                ]);
            }
            json!({ "tools": tools })
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            match name {
                "echo" => text(format!(
                    "echo: {}",
                    args.get("text").and_then(Value::as_str).unwrap_or("")
                )),
                "fail" => {
                    json!({"content": [{"type": "text", "text": "it went wrong"}], "isError": true})
                }
                "whoami" => text(authorization.unwrap_or("").to_string()),
                "crash" => {
                    eprintln!("fixture crashing on purpose");
                    std::process::exit(7);
                }
                "env" => {
                    let mut names: Vec<String> = std::env::vars_os()
                        .map(|(k, _)| k.to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    text(names.join("\n"))
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
                    text(format!("{} {grandchild_pid}", std::process::id()))
                }
                "sleep" => {
                    let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(0);
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    text(format!("slept {ms}"))
                }
                _ => return error(-32602, format!("unknown tool {name}")),
            }
        }
        "ping" => json!({}),
        _ => return error(-32601, format!("unknown method {method}")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Stateless streamable HTTP: each POST carries one JSON-RPC message and gets
/// a JSON reply (202 for notifications); GET has no event stream (405).
fn serve_http() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    println!("listening {url}");
    let _ = std::io::stdout().flush();
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || serve_connection(stream));
    }
}

fn serve_connection(stream: std::net::TcpStream) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let method = request_line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let mut length = 0usize;
        let mut authorization = String::new();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "content-length" => length = value.trim().parse().unwrap_or(0),
                    "authorization" => authorization = value.trim().to_string(),
                    _ => {}
                }
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let (status, payload) = match method.as_str() {
            "POST" => match serde_json::from_slice::<Value>(&body) {
                Ok(message) => match respond(&message, Some(&authorization)) {
                    Some(response) => ("200 OK", Some(response.to_string())),
                    None => ("202 Accepted", None),
                },
                Err(_) => ("400 Bad Request", None),
            },
            "DELETE" => ("200 OK", None),
            _ => ("405 Method Not Allowed", None),
        };
        let body = payload.unwrap_or_default();
        let content_type = if body.is_empty() {
            ""
        } else {
            "Content-Type: application/json\r\n"
        };
        let response = format!(
            "HTTP/1.1 {status}\r\n{content_type}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        if writer.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}
