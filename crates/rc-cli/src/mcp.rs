//! Collect the session's MCP servers from settings and `--mcp-config`, and
//! connect to them before the tool registry is built.

use rc_mcp::{McpHub, ServerConfig};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The servers to start, plus one error per entry that could not be used.
pub(crate) type Servers = (Vec<ServerConfig>, Vec<(String, String)>);

/// Merge the settings block with each `--mcp-config` (later entries replace
/// earlier ones of the same name). With `strict`, settings are ignored, so a
/// benchmark harness gets exactly the servers it passed. A config file that
/// cannot be read is an error: a harness that asked for servers must not run
/// silently without them.
pub(crate) fn collect(
    settings_servers: &BTreeMap<String, Value>,
    mcp_configs: &[String],
    strict: bool,
) -> anyhow::Result<Servers> {
    let mut raw = if strict {
        BTreeMap::new()
    } else {
        settings_servers.clone()
    };
    for source in mcp_configs {
        for (name, value) in read_config(source)? {
            if value.is_null() {
                raw.remove(&name);
            } else {
                raw.insert(name, value);
            }
        }
    }
    Ok(rc_mcp::parse_servers(&raw, &rc_mcp::env_lookup))
}

/// A `--mcp-config` value: a JSON file path, or inline JSON. Either form holds
/// `{"mcpServers": {...}}`, the Claude Code shape.
fn read_config(source: &str) -> anyhow::Result<BTreeMap<String, Value>> {
    let text = if source.trim_start().starts_with('{') {
        source.to_string()
    } else {
        std::fs::read_to_string(Path::new(source))
            .map_err(|e| anyhow::anyhow!("--mcp-config {source}: {e}"))?
    };
    let doc: Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("--mcp-config {source}: {e}"))?;
    let servers = doc
        .get("mcpServers")
        .or_else(|| doc.get("mcp_servers"))
        .ok_or_else(|| anyhow::anyhow!("--mcp-config {source}: no `mcpServers` object"))?;
    serde_json::from_value(servers.clone())
        .map_err(|e| anyhow::anyhow!("--mcp-config {source}: `mcpServers` must be an object: {e}"))
}

/// Connect to every server. Problems print to stderr and never stop the run;
/// with none configured this returns an empty hub without spawning anything.
pub(crate) async fn connect((servers, invalid): Servers) -> McpHub {
    if servers.is_empty() && invalid.is_empty() {
        return McpHub::default();
    }
    let hub = McpHub::connect(servers, invalid).await;
    for problem in hub.problems() {
        eprintln!("warning: {problem}");
    }
    hub
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_files_and_inline_json_layer_over_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(
            &file,
            r#"{"mcpServers": {"files": {"command": "from-file"}, "web": null}}"#,
        )
        .unwrap();
        let settings: BTreeMap<String, Value> = serde_json::from_str(
            r#"{"files": {"command": "from-settings"}, "web": {"url": "https://e.com/mcp"}, "keep": {"command": "k"}}"#,
        )
        .unwrap();
        let inline = r#"{"mcpServers": {"extra": {"command": "inline"}}}"#.to_string();
        let sources = [file.display().to_string(), inline];

        let (servers, errors) = collect(&settings, &sources, false).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["extra", "files", "keep"]);
        assert!(servers[1].transport.describe().contains("from-file"));

        let (strict, _) = collect(&settings, &sources, true).unwrap();
        let names: Vec<&str> = strict.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["extra", "files"]);
    }

    #[test]
    fn an_unreadable_mcp_config_is_an_error() {
        let missing = ["/nonexistent/mcp.json".to_string()];
        assert!(collect(&BTreeMap::new(), &missing, false).is_err());
        let shapeless = [r#"{"servers": {}}"#.to_string()];
        assert!(collect(&BTreeMap::new(), &shapeless, false).is_err());
    }
}
