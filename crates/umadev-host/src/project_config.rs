//! Whether a base may load the project's own vendor configuration.
//!
//! Every base CLI reads configuration from the project it runs in: Claude
//! Code's `.claude/settings*.json` (hooks, permission allow lists, `env`) and
//! `.mcp.json`, Codex's `.codex/config.toml` (hooks, exec policies), OpenCode's
//! `opencode.json` and `.opencode/` (plugins, MCP servers, agents), Kimi Code's
//! `.mcp.json` and `.kimi-code/mcp.json`, Grok Build's project MCP servers,
//! hooks and plugins. In a project the user has not trusted, that is
//! repository content deciding what code runs and what a tier allows, even in
//! Plan. The application layer publishes the project roots the user trusts
//! ([`set_project_trusted`](crate::project_config::set_project_trusted)); every
//! session and one-shot launch asks
//! [`loads_project_config`](crate::project_config::loads_project_config) about
//! its working directory and, when the answer
//! is no, starts the vendor with its project-level configuration switched off:
//!
//! - Claude Code: `--setting-sources user --strict-mcp-config`, so only the
//!   user's own settings load and no MCP server is started (UmaDev passes no
//!   `--mcp-config` of its own). UmaDev's governance hooks, when the project's
//!   `.claude/settings.local.json` registers them, are passed with `--settings`
//!   instead, running this binary
//!   ([`claude_governance_hooks`](crate::project_config::claude_governance_hooks)).
//! - Codex: the working directory and each of its ancestors are marked
//!   `trust_level = "untrusted"` for that launch, which keeps project config,
//!   hooks and exec policies disabled and stops app-server from recording the
//!   project as trusted on a writable `thread/start`.
//! - OpenCode: `OPENCODE_DISABLE_PROJECT_CONFIG=1`.
//! - Grok Build keeps project MCP servers, hooks and plugins behind its own
//!   Folder Trust until the user grants it there; nothing is added here.
//! - Kimi Code has no switch for its project MCP files, so a Kimi session is
//!   refused in an untrusted project that has one (`kimi_refusal`).
//!
//! A directory nobody published is untrusted: launches fail closed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Canonical roots of the projects the user trusts in this process.
static TRUSTED_ROOTS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// The Claude Code flags that load only the user's own settings and no MCP
/// servers from any settings file.
const CLAUDE_USER_SETTINGS_ONLY: [&str; 3] = ["--setting-sources", "user", "--strict-mcp-config"];

/// The `umadev hook` subcommands UmaDev registers with Claude Code.
const HOOK_SUBCOMMANDS: [&str; 3] = ["pre-write", "pre-bash", "tool-audit"];

/// Largest `.claude/settings.local.json` read to find UmaDev's hooks.
const MAX_CLAUDE_SETTINGS_BYTES: u64 = 1024 * 1024;

/// The OpenCode switch that ignores `opencode.json`, `.opencode/` and project
/// instructions.
pub(crate) const OPENCODE_DISABLE_PROJECT_CONFIG: &str = "OPENCODE_DISABLE_PROJECT_CONFIG";

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Publish whether the user trusts the project at `root`. A base started in
/// `root` or below it loads that project's vendor configuration only while it
/// is trusted. Fail-closed: a poisoned lock leaves the previous answer, and an
/// unpublished root was never trusted.
pub fn set_project_trusted(root: &Path, trusted: bool) {
    let root = canonical(root);
    if let Ok(mut roots) = TRUSTED_ROOTS.write() {
        roots.retain(|known| *known != root);
        if trusted {
            roots.push(root);
        }
    }
}

/// Whether a base working in `workspace` may load the project's own vendor
/// configuration: only inside a project root published as trusted.
#[must_use]
pub fn loads_project_config(workspace: &Path) -> bool {
    let workspace = canonical(workspace);
    TRUSTED_ROOTS
        .read()
        .is_ok_and(|roots| roots.iter().any(|root| workspace.starts_with(root)))
}

/// Extra `claude` arguments for a launch in `workspace`.
pub(crate) fn claude_args(workspace: &Path) -> Vec<String> {
    if loads_project_config(workspace) {
        return Vec::new();
    }
    let mut args: Vec<String> = CLAUDE_USER_SETTINGS_ONLY
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
    if let Some(settings) = claude_governance_settings(workspace) {
        args.extend(["--settings".to_string(), settings]);
    }
    args
}

/// UmaDev's Claude Code governance hooks running `bin`, as the `PreToolUse`
/// and `PostToolUse` matcher lists: the pre-write and pre-bash guards and the
/// post-tool audit. The one definition `umadev install --host claude-code`
/// writes and an untrusted launch passes.
#[must_use]
pub fn claude_governance_hooks(bin: &str) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let handler = |subcommand: &str| serde_json::json!([{"type": "command", "command": bin, "args": ["hook", subcommand]}]);
    // The write matchers MUST stay a superset of the hook's own write set
    // (Write / Edit / MultiEdit / NotebookEdit): a tool the hook can govern but
    // the matcher omits never fires the hook, so its writes (a secret leaked into
    // an .ipynb via NotebookEdit, say) would bypass the irreversible floor.
    let pre = vec![
        serde_json::json!({
            "matcher": "Write|Edit|MultiEdit|NotebookEdit",
            "hooks": handler("pre-write"),
        }),
        // The Bash guard (UD-SEC-002): command executions, not only file writes.
        serde_json::json!({"matcher": "Bash", "hooks": handler("pre-bash")}),
    ];
    // The audit records every executed write and command to the tool-call
    // JSONL. It is a pure evidence write and never blocks.
    let post = vec![serde_json::json!({
        "matcher": "Write|Edit|MultiEdit|NotebookEdit|Bash",
        "hooks": handler("tool-audit"),
    })];
    (pre, post)
}

/// `--settings` for an untrusted Claude launch in `workspace`: UmaDev's own
/// governance hooks, when the project's `.claude/settings.local.json` registers
/// them, running this binary. The file only says whether the user installed
/// them; the program it names is never run.
fn claude_governance_settings(workspace: &Path) -> Option<String> {
    let path = workspace.join(".claude").join("settings.local.json");
    // A regular file only: a FIFO or device would block or never end.
    if !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(&path)
        .ok()?
        .take(MAX_CLAUDE_SETTINGS_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let settings: serde_json::Value = serde_json::from_str(&text).ok()?;
    let registers_umadev_hook = ["PreToolUse", "PostToolUse"]
        .iter()
        .filter_map(|event| settings["hooks"][event].as_array())
        .flatten()
        .filter_map(|matcher| matcher["hooks"].as_array())
        .flatten()
        .any(|handler| {
            handler["args"][0] == "hook"
                && handler["args"][1]
                    .as_str()
                    .is_some_and(|sub| HOOK_SUBCOMMANDS.contains(&sub))
        });
    if !registers_umadev_hook {
        return None;
    }
    let bin = std::env::current_exe().ok()?;
    let (pre, post) = claude_governance_hooks(&bin.to_string_lossy());
    Some(serde_json::json!({"hooks": {"PreToolUse": pre, "PostToolUse": post}}).to_string())
}

/// The spellings Codex may look `dir` up by in its `projects` table: the path
/// as given and its canonical form (without the Windows verbatim prefix, which
/// Codex strips too).
fn codex_trust_keys(dir: &Path) -> Vec<String> {
    let mut keys = vec![dir.to_string_lossy().into_owned()];
    if let Ok(real) = std::fs::canonicalize(dir) {
        let real = real.to_string_lossy().into_owned();
        keys.push(
            real.strip_prefix(r"\\?\")
                .map_or(real.clone(), str::to_string),
        );
    }
    keys
}

/// Codex `projects` entries marking `workspace` and every ancestor untrusted,
/// or `None` when the project is trusted. Codex decides per directory that has
/// a `.codex/`, then by project and repository root, all of which are the
/// working directory or above it; an exact entry for each wins over any trust
/// the user's own config grants a broader path.
fn codex_untrusted_projects(workspace: &Path) -> Option<BTreeMap<String, &'static str>> {
    if loads_project_config(workspace) {
        return None;
    }
    let mut projects = BTreeMap::new();
    for start in [workspace.to_path_buf(), canonical(workspace)] {
        for dir in start.ancestors() {
            for key in codex_trust_keys(dir) {
                projects.insert(key, "untrusted");
            }
        }
    }
    Some(projects)
}

/// The `config` override for a Codex app-server `thread/start` or
/// `thread/resume` in `workspace`, or `None` when the project is trusted.
pub(crate) fn codex_thread_config(workspace: &Path) -> Option<serde_json::Value> {
    let projects: serde_json::Map<String, serde_json::Value> = codex_untrusted_projects(workspace)?
        .into_iter()
        .map(|(key, level)| (key, serde_json::json!({ "trust_level": level })))
        .collect();
    Some(serde_json::json!({ "projects": projects }))
}

/// The same override as `codex exec` arguments. The value is one TOML inline
/// table: a dotted `projects."<path>"` key would be split at every `.` of the
/// path.
pub(crate) fn codex_exec_args(workspace: &Path) -> Vec<String> {
    let Some(projects) = codex_untrusted_projects(workspace) else {
        return Vec::new();
    };
    let entries: Vec<String> = projects
        .iter()
        .map(|(key, level)| {
            format!(
                "{}={{trust_level={}}}",
                toml_string(key),
                toml_string(level)
            )
        })
        .collect();
    vec![
        "--config".to_string(),
        format!("projects={{{}}}", entries.join(",")),
    ]
}

/// `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Environment for an OpenCode launch in `workspace`.
pub(crate) fn opencode_env(workspace: &Path) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    if loads_project_config(workspace) {
        Vec::new()
    } else {
        vec![(OPENCODE_DISABLE_PROJECT_CONFIG.into(), "1".into())]
    }
}

/// Why a Kimi Code session may not start in `workspace`, if it may not. Kimi
/// starts every MCP server in the project-root `.mcp.json` and the working
/// directory's `.kimi-code/mcp.json` when a session opens, and has no switch
/// to skip them, so an untrusted project that ships one is refused.
pub(crate) fn kimi_refusal(workspace: &Path) -> Option<String> {
    if loads_project_config(workspace) {
        return None;
    }
    // Kimi's project root is the nearest ancestor holding `.git`.
    let project_root = workspace
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(workspace);
    let shipped = [
        project_root.join(".mcp.json"),
        workspace.join(".kimi-code").join("mcp.json"),
    ]
    .into_iter()
    .find(|file| std::fs::symlink_metadata(file).is_ok())?;
    Some(format!(
        "Kimi Code would start the MCP servers in {} from a project you have not trusted, and it \
         cannot be told to skip them. Trust the project (`/trust`, or `umadev trust`) to use Kimi \
         Code here, or pick another base.",
        shipped.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpublished_workspace_is_launched_without_project_config() {
        let project = tempfile::TempDir::new().unwrap();
        assert!(!loads_project_config(project.path()));
        assert_eq!(
            claude_args(project.path()),
            ["--setting-sources", "user", "--strict-mcp-config"]
        );
        assert_eq!(
            opencode_env(project.path()),
            [(
                std::ffi::OsString::from("OPENCODE_DISABLE_PROJECT_CONFIG"),
                std::ffi::OsString::from("1")
            )]
        );
        assert!(codex_thread_config(project.path()).is_some());
        assert_eq!(codex_exec_args(project.path())[0], "--config");
    }

    #[test]
    fn a_trusted_project_and_its_subdirectories_load_their_config() {
        let project = tempfile::TempDir::new().unwrap();
        let nested = project.path().join("web");
        std::fs::create_dir_all(&nested).unwrap();
        set_project_trusted(project.path(), true);
        for dir in [project.path(), nested.as_path()] {
            assert!(loads_project_config(dir));
            assert!(claude_args(dir).is_empty());
            assert!(opencode_env(dir).is_empty());
            assert!(codex_thread_config(dir).is_none());
            assert!(codex_exec_args(dir).is_empty());
        }

        set_project_trusted(project.path(), false);
        assert!(!loads_project_config(nested.as_path()));
    }

    #[test]
    fn codex_marks_the_workspace_and_every_ancestor_untrusted() {
        let project = tempfile::TempDir::new().unwrap();
        let workspace = project.path().join("my.app");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = codex_thread_config(&workspace).unwrap();
        let projects = config["projects"].as_object().unwrap();
        for dir in workspace.ancestors() {
            let key = dir.to_string_lossy();
            assert_eq!(
                projects[key.as_ref()]["trust_level"],
                "untrusted",
                "{key} is not marked untrusted"
            );
        }

        // The exec form keeps a dotted path in one quoted key.
        let args = codex_exec_args(&workspace);
        let key = toml_string(&workspace.to_string_lossy());
        assert!(
            args[1].contains(&format!("{key}={{trust_level=\"untrusted\"}}")),
            "{}",
            args[1]
        );
        assert!(args[1].starts_with("projects={") && args[1].ends_with('}'));
    }

    #[test]
    fn an_untrusted_launch_keeps_umadevs_governance_hooks_but_not_the_programs_they_name() {
        let project = tempfile::TempDir::new().unwrap();
        let claude = project.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        // Without UmaDev's hooks installed, nothing is added.
        std::fs::write(
            claude.join("settings.local.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"./evil"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(claude_args(project.path()).len(), 3);

        // Installed hooks are passed again, running this binary, not the file's.
        let (pre, post) = claude_governance_hooks("./evil/umadev");
        let installed = serde_json::json!({"hooks": {"PreToolUse": pre, "PostToolUse": post}});
        std::fs::write(claude.join("settings.local.json"), installed.to_string()).unwrap();
        let args = claude_args(project.path());
        assert_eq!(args[3], "--settings");
        let passed: serde_json::Value = serde_json::from_str(&args[4]).unwrap();
        let bin = std::env::current_exe().unwrap();
        let (pre, post) = claude_governance_hooks(&bin.to_string_lossy());
        assert_eq!(
            passed,
            serde_json::json!({"hooks": {"PreToolUse": pre, "PostToolUse": post}})
        );
        assert!(!args[4].contains("evil"));

        // A trusted project loads the file itself.
        set_project_trusted(project.path(), true);
        assert!(claude_args(project.path()).is_empty());
        set_project_trusted(project.path(), false);
    }

    #[test]
    fn toml_strings_escape_quotes_backslashes_and_controls() {
        assert_eq!(toml_string(r#"C:\a "b""#), r#""C:\\a \"b\"""#);
        assert_eq!(toml_string("a\u{7f}b\n"), r#""a\u007Fb\u000A""#);
    }

    #[test]
    fn kimi_is_refused_only_in_an_untrusted_project_that_ships_mcp_servers() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".git")).unwrap();
        let workspace = project.path().join("app");
        std::fs::create_dir_all(workspace.join(".kimi-code")).unwrap();
        assert_eq!(kimi_refusal(&workspace), None);

        std::fs::write(project.path().join(".mcp.json"), "{}").unwrap();
        assert!(kimi_refusal(&workspace).is_some());
        std::fs::remove_file(project.path().join(".mcp.json")).unwrap();
        std::fs::write(workspace.join(".kimi-code").join("mcp.json"), "{}").unwrap();
        assert!(kimi_refusal(&workspace).is_some());

        set_project_trusted(project.path(), true);
        assert_eq!(kimi_refusal(&workspace), None);
        set_project_trusted(project.path(), false);
    }
}
