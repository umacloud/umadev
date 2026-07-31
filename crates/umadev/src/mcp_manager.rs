//! MCP server management — install/list/remove MCP servers for the host.
//!
//! The five first-class bases discover MCP servers from their own project
//! config files, so "install one MCP" means writing the
//! right file for the chosen base:
//!
//! | base          | file (project-scoped)   | format                                   |
//! |---------------|-------------------------|------------------------------------------|
//! | `claude-code` | `.mcp.json`             | JSON `mcpServers` map                     |
//! | `codex`       | `.codex/config.toml`    | TOML `[mcp_servers.<name>]` tables        |
//! | `opencode`    | `opencode.json`         | JSON `mcp` map (`type`/`command` array)   |
//! | `grok-build`  | `.grok/config.toml`     | TOML `[mcp_servers.<name>]` tables        |
//! | `kimi-code`   | `.kimi-code/mcp.json`   | JSON `mcpServers` map                     |
//!
//! Every writer **parse-merges**: it loads the user's existing file, preserves
//! unknown servers + unknown top-level keys (and, for TOML, comments and
//! formatting via `toml_edit`), inserts/removes just the named server, and
//! writes back **atomically** (temp + rename). A bug or unmodelled shape never
//! wipes the user's config. `--backend all` additionally snapshots all five
//! files and rolls the whole set back if any target fails.
//!
//! ## Usage
//! ```bash
//! # default base = claude-code
//! umadev mcp-manage install github -- npx -y @modelcontextprotocol/server-github
//! umadev mcp-manage install github --backend codex -- npx -y @mcp/server-github
//! umadev mcp-manage install github --backend all   -- npx -y @mcp/server-github
//! umadev mcp-manage list   --backend opencode
//! umadev mcp-manage remove github --backend all
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use umadev_state::fs::RootedDir;

const MAX_MCP_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MCP_MUTATION_LOCK: &str = ".umadev/mcp-manager.lock";
const MCP_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const MCP_LOCK_POLL: Duration = Duration::from_millis(5);

fn with_mutation_lock<T>(
    project_root: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let root = RootedDir::open_no_follow(project_root)?;
    root.ensure_dir(Path::new(".umadev"), false)?;
    let lock = root.open_private_lock(Path::new(MCP_MUTATION_LOCK), false)?;
    let deadline = Instant::now() + MCP_LOCK_TIMEOUT;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(MCP_LOCK_POLL);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "another MCP configuration update is still running",
                ));
            }
            Err(error) => return Err(error),
        }
    }
    operation()
}

fn read_config(path: &Path) -> std::io::Result<String> {
    if let Some(parent) = path.parent() {
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) if !umadev_state::fs::metadata_is_real_dir(&metadata) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "MCP config parent is not a real directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let bytes = umadev_state::fs::read_bounded(path, MAX_MCP_CONFIG_BYTES)?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Which base CLI a `mcp-manage` operation targets. Mirrors
/// [`umadev_host::BACKEND_IDS`]; each writes a different config file/format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Claude Code — `.mcp.json` (`mcpServers` JSON map).
    ClaudeCode,
    /// Codex — `.codex/config.toml` (`[mcp_servers.<name>]` TOML tables).
    Codex,
    /// OpenCode — `opencode.json` (`mcp` JSON map, `command` as an array).
    OpenCode,
    /// Grok Build — `.grok/config.toml` (`[mcp_servers.<name>]`).
    GrokBuild,
    /// Kimi Code — `.kimi-code/mcp.json` (`mcpServers` JSON map).
    KimiCode,
}

impl Backend {
    /// Parse a backend id (the `--backend` flag value). `all` is handled by the
    /// caller (it fans out over [`Backend::ALL`]); this rejects it so a single
    /// op never silently targets one base.
    ///
    /// # Errors
    /// An unknown id yields a descriptive error listing the valid ids.
    pub fn parse(id: &str) -> std::io::Result<Self> {
        match id {
            "claude-code" | "claude" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            "grok-build" | "grok" => Ok(Self::GrokBuild),
            "kimi-code" | "kimi" => Ok(Self::KimiCode),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "unknown backend `{other}` (use claude-code | codex | opencode | \
                     grok-build | kimi-code | all)"
                ),
            )),
        }
    }

    /// All five bases, in `BACKEND_IDS` order — the fan-out for `--backend all`.
    pub const ALL: [Backend; 5] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::OpenCode,
        Self::GrokBuild,
        Self::KimiCode,
    ];

    /// The base's display id.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::GrokBuild => "grok-build",
            Self::KimiCode => "kimi-code",
        }
    }

    /// The project-relative config file this base discovers MCP servers from.
    #[must_use]
    pub fn config_rel_path(self) -> &'static str {
        match self {
            Self::ClaudeCode => ".mcp.json",
            Self::Codex => ".codex/config.toml",
            Self::OpenCode => "opencode.json",
            Self::GrokBuild => ".grok/config.toml",
            Self::KimiCode => ".kimi-code/mcp.json",
        }
    }
}

#[derive(Debug)]
struct ConfigSnapshot {
    path: PathBuf,
    original: Option<(Vec<u8>, Option<std::fs::Permissions>)>,
    missing_dirs: Vec<PathBuf>,
}

fn snapshot_config(project_root: &Path, backend: Backend) -> std::io::Result<ConfigSnapshot> {
    let path = project_root.join(backend.config_rel_path());
    match umadev_state::fs::read_bounded(&path, MAX_MCP_CONFIG_BYTES) {
        Ok(bytes) => {
            let permissions = std::fs::symlink_metadata(&path)
                .ok()
                .filter(umadev_state::fs::metadata_is_real_file)
                .map(|metadata| metadata.permissions());
            Ok(ConfigSnapshot {
                path,
                original: Some((bytes, permissions)),
                missing_dirs: Vec::new(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut missing_dirs = Vec::new();
            let mut current = path.parent();
            while let Some(directory) = current {
                if !directory.starts_with(project_root) {
                    break;
                }
                match std::fs::symlink_metadata(directory) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing_dirs.push(directory.to_path_buf());
                    }
                    _ => break,
                }
                if directory == project_root {
                    break;
                }
                current = directory.parent();
            }
            Ok(ConfigSnapshot {
                path,
                original: None,
                missing_dirs,
            })
        }
        Err(error) => Err(error),
    }
}

fn restore_config(snapshot: &ConfigSnapshot) -> std::io::Result<()> {
    if let Some((original, permissions)) = &snapshot.original {
        let current = umadev_state::fs::read_bounded(&snapshot.path, MAX_MCP_CONFIG_BYTES).ok();
        if current.as_deref() != Some(original.as_slice()) {
            umadev_state::fs::atomic_write(&snapshot.path, original)?;
        }
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&snapshot.path, permissions.clone())?;
        }
        return Ok(());
    }

    umadev_state::fs::remove_regular_file(&snapshot.path)?;
    for directory in &snapshot.missing_dirs {
        match umadev_state::fs::remove_empty_dir(directory) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn mutate_all<T>(
    project_root: &Path,
    mut operation: impl FnMut(Backend) -> std::io::Result<T>,
) -> std::io::Result<Vec<(Backend, T)>> {
    let snapshots = Backend::ALL
        .into_iter()
        .map(|backend| {
            snapshot_config(project_root, backend).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("could not snapshot {} MCP config: {error}", backend.id()),
                )
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut completed = Vec::with_capacity(Backend::ALL.len());

    for (index, backend) in Backend::ALL.into_iter().enumerate() {
        match operation(backend) {
            Ok(value) => completed.push((backend, value)),
            Err(error) => {
                let mut rollback_errors = Vec::new();
                for snapshot in snapshots[..=index].iter().rev() {
                    if let Err(rollback_error) = restore_config(snapshot) {
                        rollback_errors
                            .push(format!("{}: {rollback_error}", snapshot.path.display()));
                    }
                }
                let rollback = if rollback_errors.is_empty() {
                    "all target configs were rolled back".to_string()
                } else {
                    format!("rollback was incomplete ({})", rollback_errors.join("; "))
                };
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("{} MCP operation failed: {error}; {rollback}", backend.id()),
                ));
            }
        }
    }
    Ok(completed)
}

/// Atomically write `bytes` to `path` (temp file in the same dir + rename).
/// A same-filesystem rename is atomic on POSIX, so a crash mid-write never
/// leaves a truncated config that a reader's parser would choke on. The temp
/// name carries the pid so concurrent writers don't share + clobber it.
///
fn atomic_write(path: &Path, bytes: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    umadev_state::fs::atomic_write(path, bytes.as_bytes())
}

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
        Self::load_path(&path)
    }

    /// Load a standard `mcpServers` JSON object from an explicit base path.
    fn load_path(path: &Path) -> std::io::Result<Self> {
        match read_config(path) {
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} exists but isn't valid JSON ({e}); refusing to overwrite it and lose \
                         your MCP servers. Fix or remove it and retry.",
                        path.display()
                    ),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Save `.mcp.json` to the project root (atomic: temp + rename).
    pub fn save(&self, project_root: &Path) -> std::io::Result<PathBuf> {
        let path = project_root.join(".mcp.json");
        self.save_path(&path)
    }

    /// Save a standard `mcpServers` JSON object to an explicit base path.
    fn save_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        atomic_write(path, &(json + "\n"))?;
        Ok(path.to_path_buf())
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
///
/// This whitespace-splits a raw string; a caller that already holds the
/// shell-tokenized argv (clap's post-`--` `Vec<String>`) must use
/// [`parse_command_parts`] instead, so a quoted multi-word arg is not re-split.
pub fn parse_command(raw: &str) -> McpServerEntry {
    let parts: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
    parse_command_parts(&parts)
}

/// Build an [`McpServerEntry`] from an ALREADY-TOKENIZED command vector — the
/// `-- <command> <args…>` argv clap captured after `--`. Unlike
/// [`parse_command`] (which whitespace-splits a raw string, destroying quoting),
/// each token is kept VERBATIM, so a quoted multi-word arg (`-- node "my
/// server.js"`) stays a single arg instead of collapsing into `my` + `server.js`
/// and writing the wrong MCP server entry to the base config.
pub fn parse_command_parts(parts: &[String]) -> McpServerEntry {
    let Some(first) = parts.first() else {
        return McpServerEntry {
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: None,
        };
    };
    // If the first token looks like a URL, it's an SSE/HTTP server.
    if first.starts_with("http://") || first.starts_with("https://") {
        return McpServerEntry {
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some(first.clone()),
        };
    }
    McpServerEntry {
        command: Some(first.clone()),
        args: parts[1..].to_vec(),
        env: BTreeMap::new(),
        url: None,
    }
}

/// One server as surfaced by a `list` across any backend: a name + a
/// human-readable command/url summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The registered server name.
    pub name: String,
    /// A `command args` or `url` summary for display.
    pub detail: String,
}

/// Install (parse-merge) a named MCP server into the chosen base's config file.
/// Returns the path written. Never clobbers the user's existing config: unknown
/// servers, unknown top-level keys, and (for TOML) comments/formatting survive.
///
/// # Errors
/// Propagates I/O errors, and — like [`McpConfig::load`] — refuses to overwrite
/// a config file that exists but cannot be parsed (so a transient parse bug
/// can't wipe the user's servers).
pub fn install(
    backend: Backend,
    project_root: &Path,
    name: &str,
    entry: &McpServerEntry,
) -> std::io::Result<PathBuf> {
    with_mutation_lock(project_root, || {
        install_unlocked(backend, project_root, name, entry)
    })
}

fn install_unlocked(
    backend: Backend,
    project_root: &Path,
    name: &str,
    entry: &McpServerEntry,
) -> std::io::Result<PathBuf> {
    match backend {
        Backend::ClaudeCode => {
            let mut cfg = McpConfig::load(project_root)?;
            cfg.install(name, entry.clone());
            cfg.save(project_root)
        }
        Backend::Codex => codex::install(project_root, name, entry),
        Backend::OpenCode => opencode::install(project_root, name, entry),
        Backend::GrokBuild => grok::install(project_root, name, entry),
        Backend::KimiCode => kimi::install(project_root, name, entry),
    }
}

/// Install one server for every first-class base as one logical operation.
/// If any target fails, every earlier target is restored to its exact original
/// bytes (and files/directories created by this operation are removed).
pub fn install_all(
    project_root: &Path,
    name: &str,
    entry: &McpServerEntry,
) -> std::io::Result<Vec<(Backend, PathBuf)>> {
    with_mutation_lock(project_root, || {
        mutate_all(project_root, |backend| {
            install_unlocked(backend, project_root, name, entry)
        })
    })
}

/// Remove a named MCP server from the chosen base's config file.
/// Returns `(path, was_present)`.
///
/// # Errors
/// Propagates I/O and parse errors (refuses to touch an unparseable file).
pub fn remove(
    backend: Backend,
    project_root: &Path,
    name: &str,
) -> std::io::Result<(PathBuf, bool)> {
    with_mutation_lock(project_root, || {
        remove_unlocked(backend, project_root, name)
    })
}

fn remove_unlocked(
    backend: Backend,
    project_root: &Path,
    name: &str,
) -> std::io::Result<(PathBuf, bool)> {
    match backend {
        Backend::ClaudeCode => {
            let mut cfg = McpConfig::load(project_root)?;
            let removed = cfg.remove(name);
            let path = if removed {
                cfg.save(project_root)?
            } else {
                project_root.join(Backend::ClaudeCode.config_rel_path())
            };
            Ok((path, removed))
        }
        Backend::Codex => codex::remove(project_root, name),
        Backend::OpenCode => opencode::remove(project_root, name),
        Backend::GrokBuild => grok::remove(project_root, name),
        Backend::KimiCode => kimi::remove(project_root, name),
    }
}

/// Remove one server from every first-class base as one logical operation.
/// A failure in any target restores every earlier removal before returning.
pub fn remove_all(
    project_root: &Path,
    name: &str,
) -> std::io::Result<Vec<(Backend, (PathBuf, bool))>> {
    with_mutation_lock(project_root, || {
        mutate_all(project_root, |backend| {
            remove_unlocked(backend, project_root, name)
        })
    })
}

/// List all MCP servers the chosen base would discover from its config file.
///
/// # Errors
/// Propagates parse errors (a corrupt config is surfaced, not hidden as empty).
pub fn list(backend: Backend, project_root: &Path) -> std::io::Result<Vec<ServerInfo>> {
    match backend {
        Backend::ClaudeCode => {
            let cfg = McpConfig::load(project_root)?;
            Ok(cfg
                .list()
                .into_iter()
                .map(|(name, entry)| ServerInfo {
                    name: name.to_string(),
                    detail: summarize_json_entry(entry),
                })
                .collect())
        }
        Backend::Codex => codex::list(project_root),
        Backend::OpenCode => opencode::list(project_root),
        Backend::GrokBuild => grok::list(project_root),
        Backend::KimiCode => kimi::list(project_root),
    }
}

/// Summarize a `.mcp.json`/opencode JSON entry as `command args` or `url`.
fn summarize_json_entry(entry: &serde_json::Value) -> String {
    if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
        // claude shape: command string + args array.
        let args = entry
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        format!("{cmd} {args}").trim().to_string()
    } else if let Some(arr) = entry.get("command").and_then(|v| v.as_array()) {
        // opencode shape: command is an array including the launcher.
        arr.iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    } else if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
        url.to_string()
    } else {
        "(configured)".to_string()
    }
}

mod grok {
    use super::{codex, Backend, McpServerEntry, ServerInfo};
    use std::path::{Path, PathBuf};

    fn path(project_root: &Path) -> PathBuf {
        project_root.join(Backend::GrokBuild.config_rel_path())
    }

    pub(super) fn install(
        project_root: &Path,
        name: &str,
        entry: &McpServerEntry,
    ) -> std::io::Result<PathBuf> {
        codex::install_at(&path(project_root), name, entry)
    }

    pub(super) fn remove(project_root: &Path, name: &str) -> std::io::Result<(PathBuf, bool)> {
        codex::remove_at(&path(project_root), name)
    }

    pub(super) fn list(project_root: &Path) -> std::io::Result<Vec<ServerInfo>> {
        codex::list_at(&path(project_root))
    }
}

/// Kimi Code's project-scoped MCP file uses the same lossless
/// `mcpServers` JSON envelope as Claude Code, but lives at the vendor's
/// authoritative `.kimi-code/mcp.json` path. Keeping an explicit adapter
/// prevents one base's config from ever being written into another's file.
mod kimi {
    use super::{summarize_json_entry, Backend, McpConfig, McpServerEntry, ServerInfo};
    use std::path::{Path, PathBuf};

    fn path(project_root: &Path) -> PathBuf {
        project_root.join(Backend::KimiCode.config_rel_path())
    }

    pub(super) fn install(
        project_root: &Path,
        name: &str,
        entry: &McpServerEntry,
    ) -> std::io::Result<PathBuf> {
        let path = path(project_root);
        let mut config = McpConfig::load_path(&path)?;
        config.install(name, entry.clone());
        config.save_path(&path)
    }

    pub(super) fn remove(project_root: &Path, name: &str) -> std::io::Result<(PathBuf, bool)> {
        let path = path(project_root);
        if !path.exists() {
            return Ok((path, false));
        }
        let mut config = McpConfig::load_path(&path)?;
        let removed = config.remove(name);
        if removed {
            config.save_path(&path)?;
        }
        Ok((path, removed))
    }

    pub(super) fn list(project_root: &Path) -> std::io::Result<Vec<ServerInfo>> {
        let path = path(project_root);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let config = McpConfig::load_path(&path)?;
        Ok(config
            .list()
            .into_iter()
            .map(|(name, entry)| ServerInfo {
                name: name.to_string(),
                detail: summarize_json_entry(entry),
            })
            .collect())
    }
}

/// Codex MCP config: `[mcp_servers.<name>]` tables in `.codex/config.toml`.
/// Parsed/written with `toml_edit` so the user's comments, formatting, and
/// every unrelated key survive a round-trip verbatim.
mod codex {
    use super::{atomic_write, McpServerEntry, ServerInfo};
    use std::path::{Path, PathBuf};
    use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

    /// The project-scoped codex config file.
    fn config_path(project_root: &Path) -> PathBuf {
        project_root.join(".codex").join("config.toml")
    }

    /// Load the existing document, or an empty one if the file is missing/empty.
    ///
    /// # Errors
    /// A file that EXISTS but isn't valid TOML returns `Err` — we must not treat
    /// it as empty and then overwrite it, wiping the user's codex config.
    fn load_doc(path: &Path) -> std::io::Result<DocumentMut> {
        match super::read_config(path) {
            Ok(text) if text.trim().is_empty() => Ok(DocumentMut::new()),
            Ok(text) => text.parse::<DocumentMut>().map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} exists but isn't valid TOML ({e}); refusing to overwrite it and \
                         lose your codex config. Fix or remove it and retry.",
                        path.display()
                    ),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
            Err(e) => Err(e),
        }
    }

    pub(super) fn install(
        project_root: &Path,
        name: &str,
        entry: &McpServerEntry,
    ) -> std::io::Result<PathBuf> {
        let path = config_path(project_root);
        install_at(&path, name, entry)
    }

    pub(super) fn install_at(
        path: &Path,
        name: &str,
        entry: &McpServerEntry,
    ) -> std::io::Result<PathBuf> {
        let mut doc = load_doc(path)?;

        // Ensure `[mcp_servers]` is a table (create it implicit-of-dotted so the
        // serialized form is the idiomatic `[mcp_servers.<name>]`).
        match doc.get("mcp_servers") {
            None => {
                let mut table = Table::new();
                table.set_implicit(true);
                doc["mcp_servers"] = Item::Table(table);
            }
            Some(item) if item.is_table_like() => {}
            Some(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mcp_servers exists but is not table-like; refusing to replace it",
                ));
            }
        }
        let Some(servers) = doc["mcp_servers"].as_table_like_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "mcp_servers configuration is not table-like",
            ));
        };

        let mut srv = Table::new();
        if let Some(url) = &entry.url {
            srv["url"] = value(url.clone());
        } else if let Some(cmd) = &entry.command {
            srv["command"] = value(cmd.clone());
            if !entry.args.is_empty() {
                let mut arr = Array::new();
                for a in &entry.args {
                    arr.push(Value::from(a.clone()));
                }
                srv["args"] = value(arr);
            }
        }
        if !entry.env.is_empty() {
            let mut env_tbl = toml_edit::InlineTable::new();
            for (k, v) in &entry.env {
                env_tbl.insert(k, Value::from(v.clone()));
            }
            srv["env"] = value(env_tbl);
        }
        servers.insert(name, Item::Table(srv));

        atomic_write(path, &doc.to_string())?;
        Ok(path.to_path_buf())
    }

    pub(super) fn remove(project_root: &Path, name: &str) -> std::io::Result<(PathBuf, bool)> {
        let path = config_path(project_root);
        remove_at(&path, name)
    }

    pub(super) fn remove_at(path: &Path, name: &str) -> std::io::Result<(PathBuf, bool)> {
        if !path.exists() {
            return Ok((path.to_path_buf(), false));
        }
        let mut doc = load_doc(path)?;
        if doc
            .get("mcp_servers")
            .is_some_and(|item| !item.is_table_like())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "mcp_servers exists but is not table-like",
            ));
        }
        let removed = doc
            .get_mut("mcp_servers")
            .and_then(Item::as_table_like_mut)
            .is_some_and(|t| t.remove(name).is_some());
        if removed {
            atomic_write(path, &doc.to_string())?;
        }
        Ok((path.to_path_buf(), removed))
    }

    pub(super) fn list(project_root: &Path) -> std::io::Result<Vec<ServerInfo>> {
        let path = config_path(project_root);
        list_at(&path)
    }

    pub(super) fn list_at(path: &Path) -> std::io::Result<Vec<ServerInfo>> {
        if !path.exists() {
            return Ok(vec![]);
        }
        let doc = load_doc(path)?;
        let servers = match doc.get("mcp_servers") {
            None => return Ok(vec![]),
            Some(item) => item.as_table_like().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mcp_servers exists but is not table-like",
                )
            })?,
        };
        let mut out = Vec::new();
        for (name, item) in servers.iter() {
            let detail = item.as_table().map_or_else(
                || "(configured)".to_string(),
                |t| {
                    if let Some(url) = t.get("url").and_then(Item::as_str) {
                        url.to_string()
                    } else {
                        let cmd = t.get("command").and_then(Item::as_str).unwrap_or("");
                        let args =
                            t.get("args")
                                .and_then(Item::as_array)
                                .map_or_else(String::new, |a| {
                                    a.iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                });
                        format!("{cmd} {args}").trim().to_string()
                    }
                },
            );
            out.push(ServerInfo {
                name: name.to_string(),
                detail,
            });
        }
        Ok(out)
    }
}

/// OpenCode MCP config: the `mcp` JSON map in `opencode.json` (project root).
/// `command` is an ARRAY whose first element is the launcher; `type` is
/// `local` (stdio) or `remote` (url). Parse-merged via `serde_json::Value` so
/// unknown servers and unknown top-level keys survive.
mod opencode {
    use super::{atomic_write, McpServerEntry, ServerInfo};
    use serde_json::{json, Map, Value};
    use std::path::{Path, PathBuf};

    /// The project-scoped opencode config file.
    fn config_path(project_root: &Path) -> PathBuf {
        project_root.join("opencode.json")
    }

    /// Load the root JSON object, or an empty one if the file is missing/empty.
    ///
    /// # Errors
    /// A file that EXISTS but isn't valid JSON returns `Err` (don't overwrite).
    fn load_root(path: &Path) -> std::io::Result<Map<String, Value>> {
        match super::read_config(path) {
            Ok(text) if text.trim().is_empty() => Ok(Map::new()),
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(map)) => Ok(map),
                Ok(_) | Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} exists but isn't a valid JSON object; refusing to overwrite it and \
                         lose your opencode config. Fix or remove it and retry.",
                        path.display()
                    ),
                )),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(e) => Err(e),
        }
    }

    pub(super) fn install(
        project_root: &Path,
        name: &str,
        entry: &McpServerEntry,
    ) -> std::io::Result<PathBuf> {
        let path = config_path(project_root);
        let mut root = load_root(&path)?;

        let server = if let Some(url) = &entry.url {
            json!({ "type": "remote", "url": url, "enabled": true })
        } else {
            // opencode wants the launcher as the first element of `command`.
            let mut command: Vec<Value> = Vec::new();
            if let Some(cmd) = &entry.command {
                command.push(Value::from(cmd.clone()));
            }
            command.extend(entry.args.iter().map(|a| Value::from(a.clone())));
            let mut obj = Map::new();
            obj.insert("type".into(), Value::from("local"));
            obj.insert("command".into(), Value::Array(command));
            obj.insert("enabled".into(), Value::Bool(true));
            if !entry.env.is_empty() {
                let env: Map<String, Value> = entry
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.clone())))
                    .collect();
                obj.insert("environment".into(), Value::Object(env));
            }
            Value::Object(obj)
        };

        let mcp = root
            .entry("mcp")
            .or_insert_with(|| Value::Object(Map::new()));
        let map = mcp.as_object_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "mcp exists but is not an object; refusing to replace it",
            )
        })?;
        map.insert(name.to_string(), server);

        let json = serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default();
        atomic_write(&path, &(json + "\n"))?;
        Ok(path)
    }

    pub(super) fn remove(project_root: &Path, name: &str) -> std::io::Result<(PathBuf, bool)> {
        let path = config_path(project_root);
        if !path.exists() {
            return Ok((path, false));
        }
        let mut root = load_root(&path)?;
        if root.get("mcp").is_some_and(|mcp| !mcp.is_object()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "mcp exists but is not an object",
            ));
        }
        let removed = root
            .get_mut("mcp")
            .and_then(Value::as_object_mut)
            .is_some_and(|m| m.remove(name).is_some());
        if removed {
            let json = serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default();
            atomic_write(&path, &(json + "\n"))?;
        }
        Ok((path, removed))
    }

    pub(super) fn list(project_root: &Path) -> std::io::Result<Vec<ServerInfo>> {
        let path = config_path(project_root);
        if !path.exists() {
            return Ok(vec![]);
        }
        let root = load_root(&path)?;
        let mcp = match root.get("mcp") {
            None => return Ok(vec![]),
            Some(value) => value.as_object().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mcp exists but is not an object",
                )
            })?,
        };
        let mut out = Vec::new();
        for (name, server) in mcp {
            let detail = if let Some(url) = server.get("url").and_then(Value::as_str) {
                url.to_string()
            } else if let Some(arr) = server.get("command").and_then(Value::as_array) {
                arr.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                "(configured)".to_string()
            };
            out.push(ServerInfo {
                name: name.clone(),
                detail,
            });
        }
        Ok(out)
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
    fn load_rejects_oversized_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            vec![b' '; usize::try_from(MAX_MCP_CONFIG_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(McpConfig::load(tmp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_fifo_without_blocking() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(".mcp.json");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(McpConfig::load(tmp.path()).is_err());
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
    fn parse_command_parts_preserves_quoted_multiword_arg() {
        // clap's post-`--` argv: `-- node "my server.js"` tokenizes to two
        // elements; the space-bearing arg must survive as ONE arg, not be
        // re-split into `my` + `server.js` (the old join+split bug).
        let argv = vec!["node".to_string(), "my server.js".to_string()];
        let entry = parse_command_parts(&argv);
        assert_eq!(entry.command.as_deref(), Some("node"));
        assert_eq!(entry.args, vec!["my server.js".to_string()]);
    }

    #[test]
    fn parse_command_parts_url_and_empty() {
        let url = parse_command_parts(&["https://mcp.example.com/sse".to_string()]);
        assert!(url.command.is_none());
        assert_eq!(url.url.as_deref(), Some("https://mcp.example.com/sse"));

        let empty = parse_command_parts(&[]);
        assert!(empty.command.is_none());
        assert!(empty.args.is_empty());
    }

    #[test]
    fn parse_command_empty() {
        let entry = parse_command("");
        assert!(entry.command.is_none());
        assert!(entry.args.is_empty());
    }

    fn npx_entry() -> McpServerEntry {
        McpServerEntry {
            command: Some("npx".into()),
            args: vec!["-y".into(), "@mcp/server-github".into()],
            env: BTreeMap::new(),
            url: None,
        }
    }

    #[test]
    fn backend_parse_and_ids() {
        assert_eq!(Backend::parse("claude-code").unwrap(), Backend::ClaudeCode);
        assert_eq!(Backend::parse("codex").unwrap(), Backend::Codex);
        assert_eq!(Backend::parse("opencode").unwrap(), Backend::OpenCode);
        assert_eq!(Backend::parse("grok").unwrap(), Backend::GrokBuild);
        assert_eq!(Backend::parse("kimi").unwrap(), Backend::KimiCode);
        for retired in ["cursor", "codebuddy", "droid", "qwen-code"] {
            assert!(
                Backend::parse(retired).is_err(),
                "{retired} must stay retired"
            );
        }
        assert!(Backend::parse("all").is_err()); // caller fans out, not parse()
        assert!(Backend::parse("bogus").is_err());
        assert_eq!(Backend::ALL.len(), 5);
        assert_eq!(Backend::ALL.map(Backend::id), umadev_host::BACKEND_IDS);
        assert_eq!(Backend::Codex.config_rel_path(), ".codex/config.toml");
        assert_eq!(Backend::OpenCode.config_rel_path(), "opencode.json");
        assert_eq!(Backend::GrokBuild.config_rel_path(), ".grok/config.toml");
        assert_eq!(Backend::KimiCode.config_rel_path(), ".kimi-code/mcp.json");
    }

    #[test]
    fn new_base_mcp_configs_roundtrip_without_clobbering() {
        for backend in [Backend::GrokBuild, Backend::KimiCode] {
            let tmp = tempfile::TempDir::new().unwrap();
            let path = install(backend, tmp.path(), "github", &npx_entry()).unwrap();
            assert_eq!(path, tmp.path().join(backend.config_rel_path()));
            let servers = list(backend, tmp.path()).unwrap();
            assert_eq!(servers.len(), 1, "{backend:?}");
            assert_eq!(servers[0].name, "github");
            assert!(servers[0].detail.contains("npx"));
            let (_, removed) = remove(backend, tmp.path(), "github").unwrap();
            assert!(removed, "{backend:?}");
            assert!(list(backend, tmp.path()).unwrap().is_empty());
        }
    }

    #[test]
    fn grok_emits_its_native_schema() {
        let tmp = tempfile::TempDir::new().unwrap();
        install(Backend::GrokBuild, tmp.path(), "gh", &npx_entry()).unwrap();
        let grok = std::fs::read_to_string(tmp.path().join(".grok/config.toml")).unwrap();
        assert!(grok.contains("[mcp_servers.gh]"));
        assert!(grok.contains("command = \"npx\""));
    }

    #[test]
    fn kimi_emits_its_native_project_schema_and_preserves_unknown_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(".kimi-code/mcp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"mcpServers":{"existing":{"url":"https://example.test/mcp","enabled":false}},"future":42}"#,
        )
        .unwrap();

        install(Backend::KimiCode, tmp.path(), "gh", &npx_entry()).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["future"], 42);
        assert_eq!(value["mcpServers"]["existing"]["enabled"], false);
        assert_eq!(value["mcpServers"]["gh"]["command"], "npx");
    }

    #[test]
    fn codex_install_writes_mcp_servers_table() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = install(Backend::Codex, tmp.path(), "github", &npx_entry()).unwrap();
        assert!(path.ends_with("config.toml"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[mcp_servers.github]"));
        assert!(text.contains("command = \"npx\""));
        assert!(text.contains("@mcp/server-github"));

        let listed = list(Backend::Codex, tmp.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "github");
        assert!(listed[0].detail.contains("npx"));
    }

    #[test]
    fn codex_install_preserves_existing_keys_and_comments() {
        // A user's config.toml with an unrelated top-level key, a comment, AND
        // an existing mcp server. Installing a new one must keep all of them.
        let tmp = tempfile::TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            "# my codex config\nmodel = \"o3\"\n\n[mcp_servers.old]\ncommand = \"node\"\nargs = [\"old.js\"]\n",
        )
        .unwrap();

        install(Backend::Codex, tmp.path(), "new", &npx_entry()).unwrap();
        let text = std::fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        assert!(text.contains("# my codex config"), "comment preserved");
        assert!(text.contains("model = \"o3\""), "unrelated key preserved");
        assert!(
            text.contains("[mcp_servers.old]"),
            "existing server preserved"
        );
        assert!(text.contains("old.js"), "existing args preserved");
        assert!(text.contains("[mcp_servers.new]"), "new server added");

        let names: Vec<_> = list(Backend::Codex, tmp.path())
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"old".to_string()));
        assert!(names.contains(&"new".to_string()));
    }

    #[test]
    fn codex_remove_drops_only_named_server() {
        let tmp = tempfile::TempDir::new().unwrap();
        install(Backend::Codex, tmp.path(), "a", &npx_entry()).unwrap();
        install(Backend::Codex, tmp.path(), "b", &npx_entry()).unwrap();
        let (_, removed) = remove(Backend::Codex, tmp.path(), "a").unwrap();
        assert!(removed);
        let names: Vec<_> = list(Backend::Codex, tmp.path())
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["b".to_string()]);
        // Removing a missing one is not an error, just `false`.
        let (_, removed2) = remove(Backend::Codex, tmp.path(), "nope").unwrap();
        assert!(!removed2);
    }

    #[test]
    fn codex_install_refuses_to_clobber_unparseable_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(codex_dir.join("config.toml"), "this is = = not toml [[[").unwrap();
        assert!(
            install(Backend::Codex, tmp.path(), "x", &npx_entry()).is_err(),
            "must refuse rather than overwrite a config it can't parse"
        );
    }

    #[test]
    fn codex_rejects_a_non_table_mcp_servers_value_without_clobbering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "mcp_servers = \"user-value\"\n";
        std::fs::write(&path, original).unwrap();

        assert!(install(Backend::Codex, tmp.path(), "x", &npx_entry()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn codex_url_server_writes_url_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let entry = parse_command("https://mcp.example.com/sse");
        install(Backend::Codex, tmp.path(), "remote", &entry).unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".codex").join("config.toml")).unwrap();
        assert!(text.contains("url = \"https://mcp.example.com/sse\""));
    }

    #[test]
    fn opencode_install_writes_mcp_object_with_command_array() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = install(Backend::OpenCode, tmp.path(), "github", &npx_entry()).unwrap();
        assert!(path.ends_with("opencode.json"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let srv = &v["mcp"]["github"];
        assert_eq!(srv["type"], "local");
        assert_eq!(srv["enabled"], true);
        // command is an ARRAY whose first element is the launcher.
        let cmd = srv["command"].as_array().unwrap();
        assert_eq!(cmd[0], "npx");
        assert_eq!(cmd[1], "-y");

        let listed = list(Backend::OpenCode, tmp.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].detail.contains("npx -y"));
    }

    #[test]
    fn opencode_install_preserves_unrelated_top_level_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("opencode.json"),
            r#"{"$schema":"https://opencode.ai/config.json","model":"x","mcp":{"old":{"type":"local","command":["node"]}}}"#,
        )
        .unwrap();
        install(Backend::OpenCode, tmp.path(), "new", &npx_entry()).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["model"], "x");
        assert!(v["mcp"]["old"].is_object(), "existing server preserved");
        assert!(v["mcp"]["new"].is_object(), "new server added");
    }

    #[test]
    fn opencode_remote_server_uses_url() {
        let tmp = tempfile::TempDir::new().unwrap();
        let entry = parse_command("https://mcp.example.com/sse");
        install(Backend::OpenCode, tmp.path(), "remote", &entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["mcp"]["remote"]["type"], "remote");
        assert_eq!(v["mcp"]["remote"]["url"], "https://mcp.example.com/sse");
    }

    #[test]
    fn opencode_install_refuses_to_clobber_unparseable_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("opencode.json"), "{not json,,,").unwrap();
        assert!(install(Backend::OpenCode, tmp.path(), "x", &npx_entry()).is_err());
    }

    #[test]
    fn opencode_rejects_a_non_object_mcp_value_without_clobbering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("opencode.json");
        let original = r#"{"mcp":"user-value","future":true}"#;
        std::fs::write(&path, original).unwrap();

        assert!(install(Backend::OpenCode, tmp.path(), "x", &npx_entry()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn list_missing_config_is_empty_not_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(list(Backend::Codex, tmp.path()).unwrap().is_empty());
        assert!(list(Backend::OpenCode, tmp.path()).unwrap().is_empty());
        assert!(list(Backend::ClaudeCode, tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn claude_backend_routes_through_unified_api() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = install(Backend::ClaudeCode, tmp.path(), "gh", &npx_entry()).unwrap();
        assert!(path.ends_with(".mcp.json"));
        assert_eq!(list(Backend::ClaudeCode, tmp.path()).unwrap().len(), 1);
        let (_, removed) = remove(Backend::ClaudeCode, tmp.path(), "gh").unwrap();
        assert!(removed);
        assert!(list(Backend::ClaudeCode, tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn all_backend_install_and_remove_succeed_as_one_operation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let installed = install_all(tmp.path(), "gh", &npx_entry()).unwrap();
        assert_eq!(installed.len(), Backend::ALL.len());
        for backend in Backend::ALL {
            assert_eq!(list(backend, tmp.path()).unwrap().len(), 1, "{backend:?}");
        }

        let removed = remove_all(tmp.path(), "gh").unwrap();
        assert_eq!(removed.len(), Backend::ALL.len());
        assert!(removed.iter().all(|(_, (_, was_present))| *was_present));
        for backend in Backend::ALL {
            assert!(list(backend, tmp.path()).unwrap().is_empty(), "{backend:?}");
        }
    }

    #[test]
    fn all_backend_install_restores_exact_prior_bytes_after_late_parse_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude_path = tmp.path().join(".mcp.json");
        let codex_path = tmp.path().join(".codex/config.toml");
        let opencode_path = tmp.path().join("opencode.json");
        std::fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
        std::fs::write(
            &claude_path,
            b"{\"mcpServers\":{\"old\":{\"command\":\"old\"}},\"keep\":1}\n",
        )
        .unwrap();
        std::fs::write(
            &codex_path,
            b"# keep this byte-for-byte\nmodel = \"o3\"\n\n[mcp_servers.old]\ncommand = \"old\"\n",
        )
        .unwrap();
        std::fs::write(&opencode_path, b"{not valid json,,,\n").unwrap();
        let before = [
            std::fs::read(&claude_path).unwrap(),
            std::fs::read(&codex_path).unwrap(),
            std::fs::read(&opencode_path).unwrap(),
        ];

        let error = install_all(tmp.path(), "new", &npx_entry()).unwrap_err();
        assert!(error.to_string().contains("opencode"), "{error}");
        assert!(error.to_string().contains("rolled back"), "{error}");
        assert_eq!(std::fs::read(claude_path).unwrap(), before[0]);
        assert_eq!(std::fs::read(codex_path).unwrap(), before[1]);
        assert_eq!(std::fs::read(opencode_path).unwrap(), before[2]);
    }

    #[test]
    fn all_backend_install_removes_new_files_and_dirs_when_a_later_target_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let invalid = b"{\"mcp\":false,\"keep\":42}\n";
        std::fs::write(tmp.path().join("opencode.json"), invalid).unwrap();

        assert!(install_all(tmp.path(), "new", &npx_entry()).is_err());
        assert!(!tmp.path().join(".mcp.json").exists());
        assert!(!tmp.path().join(".codex").exists());
        assert_eq!(
            std::fs::read(tmp.path().join("opencode.json")).unwrap(),
            invalid
        );
    }

    #[test]
    fn all_backend_transaction_rolls_back_an_injected_post_write_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let error = mutate_all(tmp.path(), |backend| {
            let path = install(backend, tmp.path(), "new", &npx_entry())?;
            if backend == Backend::GrokBuild {
                return Err(std::io::Error::other(
                    "injected failure after the target write",
                ));
            }
            Ok(path)
        })
        .unwrap_err();

        assert!(error.to_string().contains("grok-build"), "{error}");
        assert!(error.to_string().contains("rolled back"), "{error}");
        for backend in Backend::ALL {
            assert!(
                !tmp.path().join(backend.config_rel_path()).exists(),
                "{backend:?} retained a partial write"
            );
        }
        assert!(!tmp.path().join(".codex").exists());
        assert!(!tmp.path().join(".grok").exists());
    }

    #[test]
    fn all_backend_remove_restores_earlier_removals_after_late_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        for backend in Backend::ALL {
            install(backend, tmp.path(), "gh", &npx_entry()).unwrap();
        }
        let grok_path = tmp.path().join(Backend::GrokBuild.config_rel_path());
        std::fs::write(&grok_path, b"not = [valid toml\n").unwrap();
        let before = Backend::ALL
            .map(|backend| std::fs::read(tmp.path().join(backend.config_rel_path())).unwrap());

        let error = remove_all(tmp.path(), "gh").unwrap_err();
        assert!(error.to_string().contains("grok-build"), "{error}");
        assert!(error.to_string().contains("rolled back"), "{error}");
        for (index, backend) in Backend::ALL.into_iter().enumerate() {
            assert_eq!(
                std::fs::read(tmp.path().join(backend.config_rel_path())).unwrap(),
                before[index],
                "{backend:?} was not restored"
            );
        }
        for backend in [
            Backend::ClaudeCode,
            Backend::Codex,
            Backend::OpenCode,
            Backend::KimiCode,
        ] {
            assert_eq!(list(backend, tmp.path()).unwrap().len(), 1, "{backend:?}");
        }
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        install(Backend::OpenCode, tmp.path(), "gh", &npx_entry()).unwrap();
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftover.is_empty(), "atomic write left a temp file");
    }

    #[test]
    fn concurrent_installs_do_not_lose_existing_servers() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::TempDir::new().unwrap();
        let root = Arc::new(tmp.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for index in 0..8 {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                install(
                    Backend::ClaudeCode,
                    &root,
                    &format!("server-{index}"),
                    &npx_entry(),
                )
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }

        let servers = list(Backend::ClaudeCode, &root).unwrap();
        assert_eq!(servers.len(), 8, "a concurrent read-modify-write was lost");
    }
}
