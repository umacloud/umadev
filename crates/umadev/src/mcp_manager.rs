//! MCP server management — install/list/remove MCP servers for the host.
//!
//! UmaDev manages MCP servers by writing `.mcp.json` in the project root.
//! Claude Code auto-discovers this file on launch and loads all listed
//! servers — so installing an MCP in UmaDev makes it available to the
//! underlying host (claude code / codex) automatically.
//!
//! ## Usage
//! ```bash
//! umadev mcp-manage install github -- npx -y @modelcontextprotocol/server-github
//! umadev mcp-manage list
//! umadev mcp-manage remove github
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One MCP server configuration entry (matches Claude Code's `.mcp.json` format).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpServerEntry {
    /// The executable command (e.g. "npx").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Command arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables for the server process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// URL for SSE/HTTP transport servers (alternative to command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The `.mcp.json` file structure.
///
/// Server entries are stored as raw `serde_json::Value` (not the typed
/// [`McpServerEntry`]) so an entry with a shape UmaDev doesn't model — `args`
/// as a string, a `transport`/`disabled` field, a future schema — is preserved
/// VERBATIM on round-trip instead of failing the whole parse. Unknown TOP-LEVEL
/// keys are preserved via `other`. Together these guarantee install/remove never
/// silently drops the user's existing config.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, serde_json::Value>,
    /// Any other top-level keys the user's `.mcp.json` carries, kept verbatim.
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}

impl McpConfig {
    /// Load `.mcp.json` from the project root.
    ///
    /// # Errors
    /// A missing or empty file yields an empty config (fail-open). A file that
    /// EXISTS but isn't valid JSON returns `Err` — we must NOT treat it as empty
    /// and then overwrite it, which would wipe the user's MCP servers.
    pub fn load(project_root: &Path) -> std::io::Result<Self> {
        let path = project_root.join(".mcp.json");
        match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        ".mcp.json exists but isn't valid JSON ({e}); refusing to overwrite it and \
                         lose your MCP servers. Fix or remove {} and retry.",
                        path.display()
                    ),
                )
            }),
            Err(_) => Ok(Self::default()),
        }
    }

    /// Save `.mcp.json` to the project root (atomic: temp + rename).
    pub fn save(&self, project_root: &Path) -> std::io::Result<PathBuf> {
        let path = project_root.join(".mcp.json");
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json + "\n")?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Install (or replace) a named MCP server.
    pub fn install(&mut self, name: &str, entry: McpServerEntry) {
        let value = serde_json::to_value(entry).unwrap_or(serde_json::Value::Null);
        self.servers.insert(name.to_string(), value);
    }

    /// Remove a named MCP server. Returns true if it was present.
    pub fn remove(&mut self, name: &str) -> bool {
        self.servers.remove(name).is_some()
    }

    /// List all configured servers as raw JSON values.
    pub fn list(&self) -> Vec<(&str, &serde_json::Value)> {
        self.servers.iter().map(|(k, v)| (k.as_str(), v)).collect()
    }
}

/// Parse `-- npx -y @modelcontextprotocol/server-github` into command + args.
/// The `--` separates the name from the server command.
pub fn parse_command(raw: &str) -> McpServerEntry {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.is_empty() {
        return McpServerEntry {
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: None,
        };
    }
    // If the first part looks like a URL, it's an SSE/HTTP server.
    if parts[0].starts_with("http://") || parts[0].starts_with("https://") {
        return McpServerEntry {
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some(parts[0].to_string()),
        };
    }
    McpServerEntry {
        command: Some(parts[0].to_string()),
        args: parts[1..]
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        env: BTreeMap::new(),
        url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_and_list() {
        let mut cfg = McpConfig::default();
        cfg.install(
            "github",
            McpServerEntry {
                command: Some("npx".into()),
                args: vec!["-y".into(), "@modelcontextprotocol/server-github".into()],
                env: BTreeMap::new(),
                url: None,
            },
        );
        assert_eq!(cfg.list().len(), 1);
        assert!(cfg.list().iter().any(|(n, _)| *n == "github"));
    }

    #[test]
    fn remove_server() {
        let mut cfg = McpConfig::default();
        cfg.install(
            "test",
            McpServerEntry {
                command: Some("echo".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            },
        );
        assert!(cfg.remove("test"));
        assert!(!cfg.remove("test"));
        assert!(cfg.list().is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cfg = McpConfig::default();
        cfg.install(
            "db",
            McpServerEntry {
                command: Some("npx".into()),
                args: vec!["-y".into(), "@modelcontextprotocol/server-postgres".into()],
                env: BTreeMap::new(),
                url: None,
            },
        );
        let path = cfg.save(tmp.path()).unwrap();
        assert!(path.ends_with(".mcp.json"));

        let loaded = McpConfig::load(tmp.path()).unwrap();
        assert_eq!(loaded.list().len(), 1);
        let (_, entry) = loaded.list()[0];
        assert_eq!(entry.get("command").and_then(|v| v.as_str()), Some("npx"));
    }

    #[test]
    fn load_missing_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = McpConfig::load(tmp.path()).unwrap();
        assert!(cfg.list().is_empty());
    }

    #[test]
    fn install_preserves_unusual_existing_entries_and_top_level_keys() {
        // A user .mcp.json with an entry shape UmaDev doesn't model (`args` as a
        // string) plus an extra top-level key. Installing a NEW server must keep
        // both verbatim — never wipe them.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            r#"{"mcpServers":{"old":{"command":"node","args":"server.js","disabled":false}},"extra":42}"#,
        )
        .unwrap();
        let mut cfg = McpConfig::load(tmp.path()).unwrap();
        cfg.install(
            "new",
            McpServerEntry {
                command: Some("npx".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            },
        );
        cfg.save(tmp.path()).unwrap();
        let after = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        assert!(after.contains("\"old\""), "existing server preserved");
        assert!(after.contains("server.js"), "string-shaped args preserved");
        assert!(after.contains("\"new\""), "new server added");
        assert!(
            after.contains("\"extra\""),
            "unknown top-level key preserved"
        );
    }

    #[test]
    fn load_refuses_to_treat_unparseable_file_as_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".mcp.json"), "{not valid json,,,").unwrap();
        assert!(
            McpConfig::load(tmp.path()).is_err(),
            "must error, not silently return empty (which would overwrite)"
        );
    }

    #[test]
    fn parse_command_stdio() {
        let entry = parse_command("npx -y @modelcontextprotocol/server-github");
        assert_eq!(entry.command.as_deref(), Some("npx"));
        assert_eq!(
            entry.args,
            vec!["-y", "@modelcontextprotocol/server-github"]
        );
    }

    #[test]
    fn parse_command_url() {
        let entry = parse_command("https://mcp.example.com/sse");
        assert!(entry.command.is_none());
        assert_eq!(entry.url.as_deref(), Some("https://mcp.example.com/sse"));
    }

    #[test]
    fn parse_command_empty() {
        let entry = parse_command("");
        assert!(entry.command.is_none());
        assert!(entry.args.is_empty());
    }
}
