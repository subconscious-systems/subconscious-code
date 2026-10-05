//! MCP server configuration: the Claude Code / Cursor `mcpServers` shape.
//!
//! ```json
//! { "mcpServers": {
//!     "files":  { "command": "npx", "args": ["-y", "@scope/server"], "env": {"TOKEN": "${MY_TOKEN}"} },
//!     "search": { "type": "http", "url": "https://example.com/mcp",
//!                 "headers": { "Authorization": "Bearer ${SEARCH_TOKEN}" } }
//! } }
//! ```
//!
//! `type` (alias `transport`) is `stdio`, `http` or `streamable-http`; it is
//! inferred from `command`/`url` when omitted. `${VAR}` and `${VAR:-default}`
//! expand from the environment in commands, arguments, env values, URLs and
//! headers, so secrets stay out of settings files.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Wait this long for a server to start and list its tools.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Wait this long for one tool call before reporting it as timed out.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// One server, validated and with every `${VAR}` expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub startup_timeout: Duration,
    pub tool_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl Transport {
    /// A short human description: `stdio: npx -y …` or `http: https://…`.
    pub fn describe(&self) -> String {
        match self {
            Transport::Stdio { command, args, .. } => {
                let mut line = format!("stdio: {command}");
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                line
            }
            Transport::Http { url, .. } => format!("http: {url}"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    #[serde(rename = "type", alias = "transport")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    disabled: bool,
    startup_timeout_ms: Option<u64>,
    tool_timeout_ms: Option<u64>,
}

/// Parse a `mcpServers` map into server configs. Returns the servers to start,
/// in name order, and one error string per entry that could not be used, so a
/// bad entry disables only itself.
pub fn parse_servers(
    raw: &BTreeMap<String, serde_json::Value>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> (Vec<ServerConfig>, Vec<(String, String)>) {
    let mut servers = Vec::new();
    let mut errors = Vec::new();
    for (name, value) in raw {
        match parse_server(name, value, lookup) {
            Ok(Some(server)) => servers.push(server),
            Ok(None) => {}
            Err(error) => errors.push((name.clone(), error)),
        }
    }
    (servers, errors)
}

/// The process environment, for [`parse_servers`].
pub fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn parse_server(
    name: &str,
    value: &serde_json::Value,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ServerConfig>, String> {
    validate_name(name)?;
    let raw: RawServer = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if raw.disabled {
        return Ok(None);
    }
    let expand = |s: &str| expand_vars(s, lookup);
    let kind = match (raw.kind.as_deref(), &raw.command, &raw.url) {
        (Some(kind), _, _) => kind.to_ascii_lowercase(),
        (None, Some(_), None) => "stdio".into(),
        (None, None, Some(_)) => "http".into(),
        (None, Some(_), Some(_)) => return Err("set either `command` or `url`, not both".into()),
        (None, None, None) => return Err("needs a `command` (stdio) or a `url` (http)".into()),
    };
    let transport = match kind.as_str() {
        "stdio" => {
            if raw.url.is_some() || !raw.headers.is_empty() {
                return Err("a stdio server takes `command`, not `url`/`headers`".into());
            }
            let command = raw
                .command
                .as_deref()
                .ok_or("a stdio server needs a `command`")?;
            Transport::Stdio {
                command: expand(command)?,
                args: raw.args.iter().map(|a| expand(a)).collect::<Result<_, _>>()?,
                env: raw
                    .env
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), expand(v)?)))
                    .collect::<Result<_, String>>()?,
                cwd: raw.cwd.as_deref().map(expand).transpose()?.map(PathBuf::from),
            }
        }
        "http" | "streamable-http" | "streamable_http" | "streamablehttp" => {
            if raw.command.is_some() || !raw.args.is_empty() || !raw.env.is_empty() {
                return Err("an http server takes `url`, not `command`/`args`/`env`".into());
            }
            let url = expand(raw.url.as_deref().ok_or("an http server needs a `url`")?)?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!("`url` must start with http:// or https://, got {url:?}"));
            }
            Transport::Http {
                url,
                headers: raw
                    .headers
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), expand(v)?)))
                    .collect::<Result<_, String>>()?,
            }
        }
        "sse" => {
            return Err(
                "the legacy SSE transport is not supported; use the server's streamable HTTP endpoint (`type: http`)"
                    .into(),
            )
        }
        other => return Err(format!("unknown transport {other:?} (use stdio or http)")),
    };
    Ok(Some(ServerConfig {
        name: name.to_string(),
        transport,
        startup_timeout: raw
            .startup_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT),
        tool_timeout: raw
            .tool_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TOOL_TIMEOUT),
    }))
}

/// Server names become part of wire tool names (`mcp__<server>__<tool>`), which
/// providers restrict to `[a-zA-Z0-9_-]`. A `__` inside a name would make the
/// server/tool split ambiguous.
fn validate_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !name.contains("__");
    if valid {
        Ok(())
    } else {
        Err(format!(
            "server name {name:?} must be 1-32 characters of [a-zA-Z0-9_-] without `__`"
        ))
    }
}

/// Expand `${VAR}` and `${VAR:-default}`. A `$` not followed by `{` is literal.
/// An unset variable without a default is an error, so a missing secret fails
/// the server loudly instead of sending an empty credential.
pub fn expand_vars(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unclosed `${{` in {input:?}"))?;
        let expr = &after[..end];
        let (var, default) = match expr.split_once(":-") {
            Some((var, default)) => (var, Some(default)),
            None => (expr, None),
        };
        match (lookup(var).filter(|v| !v.is_empty()), default) {
            (Some(value), _) => out.push_str(&value),
            (None, Some(default)) => out.push_str(default),
            (None, None) => return Err(format!("environment variable {var} is not set")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(name: &str) -> Option<String> {
        match name {
            "TOKEN" => Some("t0k".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    fn parse(value: serde_json::Value) -> (Vec<ServerConfig>, Vec<(String, String)>) {
        let raw: BTreeMap<String, serde_json::Value> = serde_json::from_value(value).unwrap();
        parse_servers(&raw, &env)
    }

    #[test]
    fn infers_transport_and_expands_variables() {
        let (servers, errors) = parse(json!({
            "files": {"command": "fs-${TOKEN}", "args": ["--key", "${TOKEN}"], "env": {"K": "${MISSING:-dflt}"}},
            "search": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${TOKEN}"}},
        }));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            servers[0].transport,
            Transport::Stdio {
                command: "fs-t0k".into(),
                args: vec!["--key".into(), "t0k".into()],
                env: BTreeMap::from([("K".into(), "dflt".into())]),
                cwd: None,
            }
        );
        assert_eq!(
            servers[1].transport,
            Transport::Http {
                url: "https://example.com/mcp".into(),
                headers: BTreeMap::from([("Authorization".into(), "Bearer t0k".into())]),
            }
        );
        assert_eq!(servers[0].tool_timeout, DEFAULT_TOOL_TIMEOUT);
    }

    #[test]
    fn harbor_transport_key_and_timeouts_are_accepted() {
        let (servers, errors) = parse(json!({
            "smoke": {"transport": "streamable-http", "url": "http://mcp:8000/mcp", "tool_timeout_ms": 5000},
        }));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(matches!(servers[0].transport, Transport::Http { .. }));
        assert_eq!(servers[0].tool_timeout, Duration::from_millis(5000));
    }

    #[test]
    fn a_bad_entry_fails_only_itself() {
        let (servers, errors) = parse(json!({
            "good": {"command": "ok"},
            "needs-secret": {"command": "x", "env": {"K": "${UNSET_SECRET}"}},
            "empty-secret": {"url": "https://e.com/mcp", "headers": {"A": "${EMPTY}"}},
            "legacy": {"type": "sse", "url": "https://e.com/sse"},
            "both": {"command": "x", "url": "https://e.com"},
            "bad__name": {"command": "x"},
            "typo": {"comand": "x"},
            "off": {"command": "x", "disabled": true},
        }));
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["good"]
        );
        let failed: Vec<&str> = errors.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            failed,
            [
                "bad__name",
                "both",
                "empty-secret",
                "legacy",
                "needs-secret",
                "typo"
            ]
        );
        assert!(errors.iter().any(|(_, e)| e.contains("UNSET_SECRET")));
    }

    #[test]
    fn dollar_without_brace_is_literal() {
        assert_eq!(
            expand_vars("cost $5 ${TOKEN}", &env).unwrap(),
            "cost $5 t0k"
        );
        assert!(expand_vars("${TOKEN", &env).is_err());
    }
}
