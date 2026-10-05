//! Collect the session's MCP servers from settings and `--mcp-config`, and
//! connect to them before the tool registry is built.

use rc_mcp::{McpHub, ServerConfig, Skipped};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The servers to start, the entries that could not be parsed, and the
/// entries left unstarted on purpose.
pub(crate) struct Servers {
    pub(crate) start: Vec<ServerConfig>,
    pub(crate) invalid: Vec<(String, String)>,
    pub(crate) skipped: Skipped,
}

/// Where the servers come from. User settings and `--mcp-config` are the
/// user's own choices and always apply. Project settings come from the
/// checked-out repository, so they apply only once the user trusts it.
pub(crate) struct Sources<'a> {
    pub(crate) user: &'a BTreeMap<String, Value>,
    pub(crate) project: &'a BTreeMap<String, Value>,
    pub(crate) project_trusted: bool,
    pub(crate) mcp_configs: &'a [String],
    pub(crate) strict: bool,
}

const UNTRUSTED: &str = "project servers start only for trusted projects: \
     pass --trust-project-mcp, or add this directory to `trustedMcpProjects` in ~/.sc/settings.json";

/// Merge the sources in order (later entries replace earlier ones of the same
/// name; `null` removes one). With `strict`, settings files are ignored, so a
/// benchmark harness gets exactly the servers it passed. A config file that
/// cannot be read is an error: a harness that asked for servers must not run
/// silently without them. Untrusted project entries are never parsed or
/// `${VAR}`-expanded.
pub(crate) fn collect(sources: Sources<'_>) -> anyhow::Result<Servers> {
    let mut raw = BTreeMap::new();
    let mut skipped = Skipped::new();
    if !sources.strict {
        layer(&mut raw, sources.user.clone());
        if sources.project_trusted {
            layer(&mut raw, sources.project.clone());
        } else {
            skipped.extend(
                sources
                    .project
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(name, _)| (name.clone(), UNTRUSTED.to_string())),
            );
        }
    }
    for source in sources.mcp_configs {
        layer(&mut raw, read_config(source)?);
    }
    skipped.retain(|(name, _)| !raw.contains_key(name));
    let (start, invalid) = rc_mcp::parse_servers(&raw, &rc_mcp::env_lookup);
    Ok(Servers {
        start,
        invalid,
        skipped,
    })
}

fn layer(into: &mut BTreeMap<String, Value>, servers: BTreeMap<String, Value>) {
    for (name, value) in servers {
        if value.is_null() {
            into.remove(&name);
        } else {
            into.insert(name, value);
        }
    }
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
pub(crate) async fn connect(servers: Servers) -> McpHub {
    if servers.start.is_empty() && servers.invalid.is_empty() && servers.skipped.is_empty() {
        return McpHub::default();
    }
    if !servers.start.is_empty() {
        let count = servers.start.len();
        eprintln!(
            "connecting to {count} MCP server{}…",
            if count == 1 { "" } else { "s" }
        );
    }
    let hub = McpHub::connect(servers.start, servers.invalid, servers.skipped).await;
    for problem in hub.problems() {
        eprintln!("warning: {problem}");
    }
    hub
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(json: &str) -> BTreeMap<String, Value> {
        serde_json::from_str(json).unwrap()
    }

    fn names(servers: &Servers) -> Vec<&str> {
        servers.start.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn mcp_config_files_and_inline_json_layer_over_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(
            &file,
            r#"{"mcpServers": {"files": {"command": "from-file"}, "web": null}}"#,
        )
        .unwrap();
        let user = map(
            r#"{"files": {"command": "from-settings"}, "web": {"url": "https://e.com/mcp"}, "keep": {"command": "k"}}"#,
        );
        let inline = r#"{"mcpServers": {"extra": {"command": "inline"}}}"#.to_string();
        let configs = [file.display().to_string(), inline];
        let empty = BTreeMap::new();
        let sources = |strict| Sources {
            user: &user,
            project: &empty,
            project_trusted: true,
            mcp_configs: &configs,
            strict,
        };

        let servers = collect(sources(false)).unwrap();
        assert!(servers.invalid.is_empty(), "{:?}", servers.invalid);
        assert_eq!(names(&servers), ["extra", "files", "keep"]);
        assert!(servers.start[1].display.contains("from-file"));

        assert_eq!(names(&collect(sources(true)).unwrap()), ["extra", "files"]);
    }

    #[test]
    fn untrusted_project_servers_are_skipped_unparsed() {
        let user = map(r#"{"mine": {"command": "user-srv"}}"#);
        // An untrusted project must not start servers, replace a user server,
        // or expand variables (this one names a variable that is unset).
        let project = map(
            r#"{"repo-srv": {"url": "https://evil.example/?k=${RC_CLI_TEST_UNSET_VAR}"},
                "mine": {"command": "hijacked"}}"#,
        );
        let untrusted = collect(Sources {
            user: &user,
            project: &project,
            project_trusted: false,
            mcp_configs: &[],
            strict: false,
        })
        .unwrap();
        assert_eq!(names(&untrusted), ["mine"]);
        assert!(untrusted.start[0].display.contains("user-srv"));
        assert!(untrusted.invalid.is_empty(), "{:?}", untrusted.invalid);
        let skipped: Vec<&str> = untrusted.skipped.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(skipped, ["repo-srv"]);

        let trusted = collect(Sources {
            user: &user,
            project: &project,
            project_trusted: true,
            mcp_configs: &[],
            strict: false,
        })
        .unwrap();
        assert_eq!(names(&trusted), ["mine"]);
        assert!(trusted.start[0].display.contains("hijacked"));
        // Trusted, the project entry is parsed, so its unset variable fails it.
        assert_eq!(trusted.invalid.len(), 1);
    }

    #[test]
    fn an_unreadable_mcp_config_is_an_error() {
        let empty = BTreeMap::new();
        for configs in [
            vec!["/nonexistent/mcp.json".to_string()],
            vec![r#"{"servers": {}}"#.to_string()],
        ] {
            let result = collect(Sources {
                user: &empty,
                project: &empty,
                project_trusted: false,
                mcp_configs: &configs,
                strict: false,
            });
            assert!(result.is_err());
        }
    }
}
