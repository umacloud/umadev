//! `umadev` — the single binary entrypoint.
//!
//! The recommended entry is to run the binary with **no subcommand**, which
//! launches the chat TUI. From there every feature is reachable via slash
//! commands (`/run`, `/continue`, `/backend`, `/status`, …).
//!
//! Subcommands exist for scripting / CI and for the architectural non-negotiable
//! real-time governance hook (called *by* a base CLI, not a human). The visible
//! surface is intentionally tiny:
//!
//! - (none)                  launch the chat TUI (the recommended entry)
//! - `init`                  write the `umadev.yaml` spec manifest
//! - `install --host <id>`   wire the real-time governance hook into a base CLI
//!
//! The rest (`run` / `continue` / `revise` / `spec` / `verify` / `report` /
//! `doctor` / `examples` / `guide` / `rollback` / `history`) are hidden from
//! `--help` but still work for scripts, and mirror TUI slash commands.
//! `hook` / `uninstall` are hidden internals.
//!
//! Anything outside this surface is intentionally absent.

#![forbid(unsafe_code)]
#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::doc_markdown,
    clippy::unused_async,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::vec_init_then_push,
    clippy::format_push_string,
    clippy::uninlined_format_args,
    dead_code
)]

mod ci;
mod doctor;
mod hook;
mod knowledge_manager;
mod mcp;
mod mcp_manager;
mod skill_manager;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

use umadev_agent::ChannelSink;
use umadev_agent::{
    classify_reply, list_snapshots, read_workflow_state, restore_snapshot, AgentRunner, Gate,
    GateOutcome, RunOptions, RunReport, WorkflowState,
};
use umadev_governance::{compliance::write_compliance_mapping, record_tool_call};
use umadev_runtime::{OfflineRuntime, RuntimeKind};
use umadev_spec::{CLAUSES, PHASE_CHAIN, SPEC_VERSION};

/// UmaDev — a coach for AI coding hosts.
#[derive(Debug, Parser)]
#[command(
    name = "umadev",
    version,
    about = "AI 编码的项目总监 Agent — 9-phase commercial delivery pipeline. Run `umadev` (no args) for the TUI. Drive your logged-in Claude Code / Codex / OpenCode CLI. No API key of its own — your existing base login is the brain.",
    long_about = None,
)]
struct Cli {
    /// Subcommand. **Omit to launch the TUI** — that is the recommended entry.
    /// `init` bootstraps a workspace; `install` wires the governance hook.
    /// All other verbs are hidden but still work for scripts/CI.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bootstrap a workspace: write the `umadev.yaml` spec manifest.
    #[command(
        long_about = "Bootstrap a workspace by writing a `umadev.yaml` spec manifest\n\
                      (UD-META-001). This declares the project to UmaDev so future\n\
                      `umadev run` / `umadev` (TUI) invocations have a stable slug,\n\
                      conformance level, and quality threshold.\n\
                      \n\
                      Idempotent — re-running on an initialised workspace is a no-op\n\
                      unless `--force` is passed.",
        after_help = "EXAMPLES:\n  \
                      umadev init                       # init current dir\n  \
                      umadev init --slug my-app          # explicit slug\n  \
                      umadev init --project-root ./app   # initialise a sub-directory\n  \
                      umadev init --force                # overwrite a hand-edited manifest"
    )]
    Init {
        /// Project slug used in artifact filenames; defaults to the
        /// workspace directory name.
        #[arg(long)]
        slug: Option<String>,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Overwrite an existing `umadev.yaml` that differs from the
        /// shipped template.
        #[arg(long)]
        force: bool,
    },
    /// Drive the pipeline from `research` to the first gate (`docs_confirm`).
    #[command(
        hide = true,
        long_about = "Run the pipeline non-interactively from `research` to the first\n\
                      gate (`docs_confirm`). Workers:\n\
                      \n  \
                      --backend claude-code    Anthropic Claude Code\n  \
                      --backend codex          OpenAI Codex\n  \
                      --backend opencode       OpenCode\n  \
                      (default)                offline deterministic templates\n\
                      \n\
                      All base backends drive the user's already-installed,\n\
                      already-logged-in CLI — no API key needed.\n\
                      \n\
                      After `run`, the pipeline pauses at the `docs_confirm` gate.\n\
                      Review `output/*-prd.md` etc., then run `umadev continue` to\n\
                      proceed, or `umadev revise \"...\"` to ask for changes.",
        after_help = "EXAMPLES:\n  \
                      umadev run \"做一个登录系统\"                       # offline\n  \
                      umadev run \"...\" --backend claude-code              # Claude Code\n  \
                      umadev run \"...\" --backend codex --slug my-mvp      # explicit slug\n  \
                      umadev run \"...\" --backend opencode                 # OpenCode"
    )]
    Run {
        /// Plain-text requirement, e.g. "做一个登录页".
        requirement: String,
        /// Drive an already-logged-in base CLI as the worker. When
        /// omitted, the pipeline runs offline with deterministic templates.
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Optional model override. Empty by default so the base CLI runs on its
        /// OWN configured model (login default / connected third-party / local) —
        /// UmaDev never imposes a model. Pass e.g. `--model opus` to override.
        #[arg(long, default_value = "")]
        model: String,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Project slug used in artifact filenames.
        #[arg(long, default_value = "")]
        slug: String,
    },
    /// Approve the active gate and continue the pipeline.
    #[command(
        hide = true,
        long_about = "Approve the currently-active gate and drive the pipeline to its\n\
                      next pause point. Call after reviewing the artifacts a `run` or\n\
                      `continue` previously produced.\n\
                      \n\
                      Gate progression:\n  \
                      docs_confirm       (after research + docs)      → spec / frontend\n  \
                      preview_confirm    (after spec + frontend)       → backend / quality / delivery\n\
                      \n\
                      `continue` is a no-op if no gate is active.",
        after_help = "EXAMPLES:\n  \
                      umadev continue\n  \
                      umadev continue --backend claude-code\n  \
                      umadev continue --project-root ./app"
    )]
    Continue {
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Drive the next block via an already-logged-in base CLI. When
        /// omitted, falls back to whatever the original `run` recorded —
        /// or offline templates if no backend was tracked.
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
    },
    /// Stay in the active gate and record a revision request.
    #[command(
        hide = true,
        long_about = "Record a revision request against the active gate. The pipeline\n\
                      does NOT advance — it stays at the same gate so you can iterate\n\
                      on the artifacts. The revision text is appended to\n\
                      `.umadev/decisions/<gate>.md` so the audit chain captures\n\
                      every change request.",
        after_help = "EXAMPLES:\n  \
                      umadev revise \"把 OAuth 那段去掉,先做邮箱+密码 MVP\"\n  \
                      umadev revise \"前端改成 Vue 而不是 React\""
    )]
    Revise {
        /// What needs to change. Free-form text.
        text: String,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Roll the pipeline back to a previous state snapshot.
    #[command(
        hide = true,
        long_about = "Restore workflow-state.json from a previous transition snapshot.\n\n                      Every phase transition is snapshotted to .umadev/history/. Use\n                      `umadev history` to list available snapshots, then `umadev rollback\n                      <timestamp>` to restore one. This undoes the most recent transition(s)\n                      without losing the generated artifacts on disk.",
        after_help = "EXAMPLES:\n                        umadev history                   # list snapshots\n                        umadev rollback 20260614T120000  # restore by timestamp\n                        umadev rollback latest           # undo the last transition"
    )]
    Rollback {
        /// Snapshot timestamp (from `umadev history`), or `latest`.
        timestamp: String,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// List available rollback snapshots.
    History {
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Print the `UMADEV_HOST_SPEC_V1` specification.
    #[command(
        hide = true,
        long_about = "Print the UMADEV_HOST_SPEC_V1 specification — the normative\n\
                      contract UmaDev enforces (25 clauses across 4 layers + 9\n\
                      phases + 2 gates).",
        after_help = "EXAMPLES:\n  \
                      umadev spec               # full markdown\n  \
                      umadev spec --clauses     # clause table only\n  \
                      umadev spec | less        # paged"
    )]
    Spec {
        /// Print only the clause table.
        #[arg(long)]
        clauses: bool,
    },
    /// Verify spec conformance of a workspace.
    #[command(
        hide = true,
        long_about = "Print a structured conformance report for the workspace:\n\
                      spec manifest health, workflow state, evidence chain row counts,\n\
                      latest quality-gate score, and proof-pack zips.",
        after_help = "EXAMPLES:\n  \
                      umadev verify\n  \
                      umadev verify --project-root ./app"
    )]
    Verify {
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Emit the UD-EVID-004 compliance mapping document.
    #[command(
        hide = true,
        long_about = "Generate the UD-EVID-004 compliance mapping document. Takes the\n\
                      in-workspace evidence files (audit JSONLs + quality report) and\n\
                      maps every clause that fired to the corresponding controls in\n\
                      SOC 2 (2017 TSC), ISO/IEC 27001:2022 Annex A, and the EU AI Act.\n\
                      \n\
                      Output: `output/<slug>-compliance-mapping.json`.",
        after_help = "EXAMPLES:\n  \
                      umadev report\n  \
                      umadev report --slug my-app"
    )]
    Report {
        /// Project slug used in artifact filenames.
        #[arg(long)]
        slug: Option<String>,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Self-test: binary integrity, workspace permissions, manifest.
    #[command(
        hide = true,
        long_about = "Run a self-test that diagnoses common 'installed but not working'\n\
                      situations: binary identity, embedded spec validity, workspace\n\
                      writability, UD-META-001 manifest health.\n\
                      \n\
                      Exit code is non-zero if any check FAILs (suitable for CI smoke).",
        after_help = "EXAMPLES:\n  \
                      umadev doctor\n  \
                      umadev doctor --project-root ./app"
    )]
    Doctor {
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Show common-workflow examples for new users.
    #[command(
        hide = true,
        long_about = "Print a curated set of common-workflow examples. Useful as a\n\
                      cheat-sheet — every important command is shown with a real\n\
                      invocation."
    )]
    Examples,
    /// 60-second guided walkthrough for new users (interactive).
    #[command(
        hide = true,
        long_about = "Run a short, mode-by-mode walkthrough of UmaDev. Prints what\n\
                      each command does, how the pipeline progresses, and what\n\
                      artifacts you can expect. No side effects."
    )]
    Guide,
    /// Pre-write governance hook (called by Claude Code's PreToolUse).
    ///
    /// Reads a PreToolUse JSON payload from stdin, runs the governance rules
    /// (emoji / color / AI-slop), and prints a permission decision. This is
    /// NOT meant to be called by humans — Claude Code invokes it via the
    /// hook registered by `umadev install`.
    #[command(hide = true)]
    Hook {
        /// Which governance check to run: `pre-write` (all code rules) or
        /// `check-emoji` / `check-color` / `check-slop` (individual).
        check: String,
    },
    /// Install the UmaDev pre-write governance hook into a base CLI.
    ///
    /// Currently supports Claude Code (writes `.claude/settings.json` with a
    /// PreToolUse hook pointing at this binary). Codex is honestly
    /// reported as unsupported — they rely on the quality-gate hard block
    /// instead of real-time interception.
    #[command(
        long_about = "Install the UmaDev pre-write governance hook into a base CLI.\n\
                      \n\
                      Supported bases:\n  \
                      claude-code   writes .claude/settings.json PreToolUse hook\n\
                      \n\
                      The hook intercepts every Write/Edit tool call and refuses\n\
                      emoji-as-icon / hardcoded-color / AI-slop code in real time\n\
                      (UD-CODE-001/002/005). Codex lacks\n\
                      a PreToolUse hook surface — they rely on the quality-gate\n\
                      hard block instead."
    )]
    Install {
        /// Base to install into: `claude-code` (default) or `pre-commit`.
        /// (The legacy `--host` spelling still works as an alias.)
        ///
        /// `claude-code` writes the PreToolUse hook into `.claude/settings.json`.
        /// `pre-commit` writes a git `pre-commit` hook into `.git/hooks/` that
        /// runs `umadev ci --changed-only` before every commit.
        #[arg(
            long = "base",
            visible_alias = "host",
            value_name = "BASE",
            default_value = "claude-code"
        )]
        host: String,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Uninstall UmaDev cleanly.
    ///
    /// With NO `--base`: a full clean uninstall — removes `~/.umadev` (global
    /// config + data), this project's governance hooks, and the `umadev` binary
    /// itself (asks for confirmation first).
    ///
    /// With `--base <claude-code|pre-commit>`: removes ONLY that base's
    /// governance hook and leaves everything else in place.
    Uninstall {
        /// Remove only this base's governance hook instead of a full uninstall.
        /// (The legacy `--host` spelling still works.)
        #[arg(long = "base", visible_alias = "host", value_name = "BASE")]
        base: Option<String>,
        /// Skip the confirmation prompt (for scripts).
        #[arg(long)]
        yes: bool,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Upgrade UmaDev to the latest version (re-installs via npm).
    Update {
        /// Skip the confirmation prompt (for scripts).
        #[arg(long)]
        yes: bool,
    },
    /// Run UmaDev as an MCP (Model Context Protocol) server over stdio.
    ///
    /// Exposes `govern_file` and `govern_command` tools so any MCP-compatible
    /// host (Claude Desktop, Cursor, Continue, etc.) can call UmaDev's
    /// governance layer before writing files or running commands. Register
    /// this binary as an MCP server in your client's config with the command
    /// `umadev mcp serve`.
    #[command(name = "mcp")]
    Mcp {
        /// The MCP transport. Only `serve` (stdio) is supported.
        #[arg(default_value = "serve")]
        transport: String,
    },
    /// Run governance on every source file in the workspace (CI/CD mode).
    ///
    /// Scans all source files under the project root and exits non-zero if any
    /// file violates a governance rule. Use in a GitHub Action or pre-commit
    /// hook so violations are caught before code is pushed.
    #[command(name = "ci")]
    Ci {
        /// Report violations but always exit 0 (for non-blocking dashboards).
        #[arg(long)]
        report_only: bool,
        /// Only scan git-changed files (vs the whole workspace).
        #[arg(long)]
        changed_only: bool,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Manage MCP servers — install/list/remove MCP servers for the host.
    ///
    /// Installed servers are written to `.mcp.json` so Claude Code / Codex / OpenCode
    /// auto-discover them on launch.
    #[command(name = "mcp-manage")]
    McpManage {
        /// Action: `install`, `list`, or `remove`.
        action: String,
        /// Server name (for install/remove).
        name: Option<String>,
        /// Server command (everything after `--`).
        /// e.g. `umadev mcp-manage install github -- npx -y @mcp/server-github`
        command: Vec<String>,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Manage Skills — install/list/remove knowledge + rule + prompt packages.
    #[command(name = "skill")]
    Skill {
        /// Action: `install`, `list`, or `remove`.
        action: String,
        /// Skill name or source directory (for install/remove).
        target: Option<String>,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Manage custom knowledge — add/list/search/remove documents.
    #[command(name = "knowledge-manage")]
    KnowledgeManage {
        /// Action: `add`, `list`, `search`, or `remove`.
        action: String,
        /// Source path (add), name (remove), or query (search).
        target: Option<String>,
        /// Optional name for `add`.
        #[arg(long)]
        name: Option<String>,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
}

/// Host CLI backend selector for `umadev run --backend`.
///
/// UmaDev drives exactly three base CLI bases. A unit test
/// asserts [`BACKEND_ARG_IDS`] stays equal to [`umadev_host::BACKEND_IDS`].
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
enum BackendArg {
    /// Drive Claude Code CLI.
    ClaudeCode,
    /// Drive Codex CLI.
    Codex,
    /// Drive OpenCode CLI.
    Opencode,
}

impl BackendArg {
    fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        match id {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::Opencode),
            _ => None,
        }
    }

    /// Every id this enum can produce. Kept in sync with
    /// [`umadev_host::BACKEND_IDS`] by the `backend_arg_ids_match_host` test.
    fn all_ids() -> &'static [&'static str] {
        &["claude-code", "codex", "opencode"]
    }
}

/// Re-export so the sync test and help text can reference the canonical list.
const BACKEND_ARG_IDS: &[&str] = &["claude-code", "codex", "opencode"];

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    // No subcommand → launch the TUI (the recommended interactive entry).
    // In a non-TTY environment (piped output, CI, docker), fall back to
    // printing help instead of crashing on terminal setup.
    let Some(command) = cli.command else {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return cmd_tui().await;
        }
        eprintln!(
            "umadev: no terminal detected — showing help.
"
        );
        Cli::command().print_help()?;
        return Ok(());
    };

    match command {
        Command::Init {
            slug,
            project_root,
            force,
        } => cmd_init(slug, project_root, force),
        Command::Run {
            requirement,
            backend,
            model,
            project_root,
            slug,
        } => {
            cmd_run(RunArgs {
                requirement,
                backend,
                model,
                project_root,
                slug,
            })
            .await
        }
        Command::Continue {
            project_root,
            backend,
        } => cmd_continue(project_root, backend).await,
        Command::Revise { text, project_root } => cmd_revise(text, project_root).await,
        Command::Rollback {
            timestamp,
            project_root,
        } => cmd_rollback(timestamp, project_root),
        Command::History { project_root } => cmd_history(project_root),
        Command::Spec { clauses } => cmd_spec(clauses),
        Command::Verify { project_root } => cmd_verify(project_root),
        Command::Report { slug, project_root } => cmd_report(slug, project_root),
        Command::Doctor { project_root } => cmd_doctor(project_root),
        Command::Examples => cmd_examples(),
        Command::Guide => cmd_guide(),
        Command::Hook { check } => cmd_hook(check),
        Command::Install { host, project_root } => cmd_install(host, project_root),
        Command::Uninstall {
            base,
            yes,
            project_root,
        } => cmd_uninstall(base, yes, project_root),
        Command::Update { yes } => cmd_update(yes),
        Command::Mcp { transport } => cmd_mcp(transport),
        Command::Ci {
            report_only,
            changed_only,
            project_root,
        } => cmd_ci(report_only, changed_only, project_root),
        Command::McpManage {
            action,
            name,
            command,
            project_root,
        } => cmd_mcp_manage(action, name, command, project_root),
        Command::Skill {
            action,
            target,
            project_root,
        } => cmd_skill(action, target, project_root),
        Command::KnowledgeManage {
            action,
            target,
            name,
            project_root,
        } => cmd_knowledge(action, target, name, project_root),
    }
}

fn cmd_examples() -> Result<()> {
    println!("{}", include_str!("../templates/examples.txt"));
    Ok(())
}

fn cmd_guide() -> Result<()> {
    println!("{}", include_str!("../templates/guide.txt"));
    Ok(())
}

fn cmd_hook(check: String) -> Result<()> {
    // Read the PreToolUse payload from stdin.
    use std::io::Read;
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    // Load the per-project policy from .umadev/rules.toml (fail-open default).
    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let policy = umadev_governance::Policy::load(&project_root);
    let decision = match check.as_str() {
        "pre-write" | "check-emoji" | "check-color" | "check-slop" => {
            hook::run_pre_write_with(&stdin, &policy)
        }
        "pre-bash" => hook::run_pre_bash(&stdin),
        _ => {
            // Fail-open, like every other path in the hook layer: an unknown
            // check name (a misconfigured matcher, or a future host passing a
            // check this build doesn't recognise) must still emit an `allow`
            // decision — NOT exit non-zero with empty stdout, which a host may
            // treat as a hard block. Warn on stderr for diagnosability.
            eprintln!(
                "umadev: unknown hook check `{check}` — passing through (expected: pre-write, pre-bash)"
            );
            umadev_governance::Decision::pass()
        }
    };
    // Record the decision to the audit log (UD-EVID-002). The hook runs in
    // the workspace CWD, so that's the project root. Best-effort: a write
    // failure never blocks the host.
    if decision.block {
        let tool = if check == "pre-bash" { "Bash" } else { "Write" };
        let _ = umadev_governance::record_tool_call(
            &project_root,
            tool,
            "",
            "block",
            &decision.clause,
            &decision.reason,
            "",
            None,
        );
    }
    hook::print_decision(&decision);
    Ok(())
}

fn cmd_install(host: String, project_root: Option<PathBuf>) -> Result<()> {
    let root = project_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    match host.as_str() {
        "claude-code" => {
            let path = hook::install_claude_hook(&root)?;
            println!("[ok] Installed UmaDev PreToolUse hook for Claude Code.");
            println!("  → {}", path.display());
            println!();
            println!("Every Write/Edit tool call will now be checked for:");
            println!("  • emoji-as-functional-icons (UD-CODE-001)");
            println!("  • hardcoded color literals   (UD-CODE-002)");
            println!("  • AI-slop / placeholders     (UD-CODE-002)");
            println!("  • sensitive-path writes      (UD-SEC-001) — .git/.env/.ssh bypass-immune");
            println!();
            println!("To remove: umadev uninstall --host claude-code");
        }
        "pre-commit" => {
            let path = install_pre_commit_hook(&root)?;
            println!("[ok] Installed UmaDev pre-commit git hook.");
            println!("  → {}", path.display());
            println!();
            println!("Every `git commit` will now run `umadev ci --changed-only`");
            println!("on the staged files. A violation aborts the commit.");
            println!();
            println!("To remove: umadev uninstall --host pre-commit");
        }
        other => {
            anyhow::bail!(
                "Unknown install host '{other}'. Supported: `claude-code`, `pre-commit`."
            );
        }
    }
    Ok(())
}

fn cmd_uninstall(base: Option<String>, yes: bool, project_root: Option<PathBuf>) -> Result<()> {
    let root = project_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    // Hook-only mode: `umadev uninstall --base <x>` — unchanged behaviour.
    if let Some(host) = base {
        match host.as_str() {
            "claude-code" => {
                hook::uninstall_claude_hook(&root)?;
                println!("[ok] Removed UmaDev PreToolUse hook from Claude Code settings.");
            }
            "pre-commit" => {
                uninstall_pre_commit_hook(&root)?;
                println!("[ok] Removed UmaDev pre-commit git hook.");
            }
            other => anyhow::bail!("uninstall not applicable for base '{other}'"),
        }
        return Ok(());
    }

    // Full clean uninstall: global state + this project's hooks + the binary.
    let state_dirs: Vec<PathBuf> = umadev_state_dirs()
        .into_iter()
        .filter(|d| d.exists())
        .collect();
    let exe = std::env::current_exe().ok();
    println!("This will completely uninstall UmaDev and remove:");
    for d in &state_dirs {
        println!("  - global config + data:  {}", d.display());
    }
    println!("  - this project's governance hooks (.claude/settings.json, .git/hooks/pre-commit)");
    if let Some(e) = &exe {
        println!("  - the umadev binary:     {}", e.display());
    }
    if !yes && !confirm("Continue?") {
        println!("Aborted. Nothing was removed.");
        return Ok(());
    }
    // 1. Global config + data (~/.umadev and/or $XDG_CONFIG_HOME/umadev).
    for d in &state_dirs {
        match std::fs::remove_dir_all(d) {
            Ok(()) => println!("[ok] Removed {}", d.display()),
            Err(e) => println!("[!] Could not remove {} ({e}).", d.display()),
        }
    }
    // 2. This project's governance hooks (best-effort; fail-open).
    let _ = hook::uninstall_claude_hook(&root);
    let _ = uninstall_pre_commit_hook(&root);
    println!("[ok] Removed this project's governance hooks (if present).");
    // 3. The binary LAST (so the steps above ran on a live binary). An npm
    //    install is removed via npm so the package metadata is cleaned too; a
    //    manual/dev binary is unlinked directly (safe while running on Unix).
    let npm_managed = exe
        .as_ref()
        .is_some_and(|p| p.to_string_lossy().contains("node_modules"));
    if npm_managed {
        match umadev_host::std_command("npm")
            .args(["uninstall", "-g", "umadev"])
            .status()
        {
            Ok(s) if s.success() => println!("[ok] npm uninstall -g umadev"),
            _ => println!("[!] Run `npm uninstall -g umadev` to remove the binary."),
        }
    } else if let Some(e) = exe {
        match std::fs::remove_file(&e) {
            Ok(()) => println!("[ok] Removed {}", e.display()),
            Err(err) => println!("[!] Delete the binary manually: {} ({err})", e.display()),
        }
    }
    println!("\nUmaDev uninstalled. Thanks for trying it.");
    Ok(())
}

/// Candidate directories holding UmaDev's global config + data — `~/.umadev`
/// and, when set, `$XDG_CONFIG_HOME/umadev`. Mirrors `config::default_path`.
fn umadev_state_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            dirs.push(PathBuf::from(xdg).join("umadev"));
        }
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let d = PathBuf::from(home).join(".umadev");
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

/// Ask a `y/N` question on the terminal. Returns false on any read error so an
/// unattended/piped run never destroys data without an explicit `--yes`.
fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// `umadev update` — re-install the latest published version via npm. A manual /
/// dev build (not under npm) just prints the upgrade instructions.
fn cmd_update(yes: bool) -> Result<()> {
    let exe = std::env::current_exe().ok();
    let npm_managed = exe
        .as_ref()
        .is_some_and(|p| p.to_string_lossy().contains("node_modules"));
    println!("UmaDev {} is installed.", env!("CARGO_PKG_VERSION"));
    if !npm_managed {
        println!(
            "This is a manual / dev build (not an npm install).\n  \
             upgrade:  npm install -g umadev@latest\n  \
             releases: https://github.com/umacloud/umadev/releases"
        );
        return Ok(());
    }
    if !yes && !confirm("Upgrade now via `npm install -g umadev@latest`?") {
        println!("Aborted.");
        return Ok(());
    }
    match umadev_host::std_command("npm")
        .args(["install", "-g", "umadev@latest"])
        .status()
    {
        Ok(s) if s.success() => {
            println!("[ok] UmaDev upgraded. Run `umadev --version` to confirm.");
            Ok(())
        }
        Ok(s) => anyhow::bail!("npm exited with status {s}"),
        Err(e) => {
            anyhow::bail!("could not run npm ({e}); run `npm install -g umadev@latest` yourself")
        }
    }
}

/// `umadev mcp serve` — run the MCP governance server over stdio.
fn cmd_mcp(transport: String) -> Result<()> {
    match transport.as_str() {
        "serve" | "stdio" => {
            mcp::serve()?;
            Ok(())
        }
        other => anyhow::bail!("unsupported MCP transport: '{other}' (only 'serve'/'stdio')"),
    }
}

/// `umadev mcp-manage` — install/list/remove MCP servers.
fn cmd_mcp_manage(
    action: String,
    name: Option<String>,
    command: Vec<String>,
    project_root: Option<PathBuf>,
) -> Result<()> {
    let root = project_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let mut cfg = mcp_manager::McpConfig::load(&root)?;
    match action.as_str() {
        "install" | "add" => {
            let server_name = name.ok_or_else(|| {
                anyhow::anyhow!(
                    "server name required: umadev mcp-manage install <name> -- <command>"
                )
            })?;
            let cmd_str = command.join(" ");
            if cmd_str.is_empty() {
                anyhow::bail!("command required after --: e.g. `-- npx -y @mcp/server-github`");
            }
            let entry = mcp_manager::parse_command(&cmd_str);
            cfg.install(&server_name, entry);
            let path = cfg.save(&root)?;
            println!("[ok] Installed MCP server '{server_name}'.");
            println!("  → {}", path.display());
            println!("  Claude Code will auto-discover this on next launch.");
        }
        "list" | "ls" => {
            let servers = cfg.list();
            if servers.is_empty() {
                println!("No MCP servers configured.");
            } else {
                println!("MCP servers ({}):", servers.len());
                for (name, entry) in servers {
                    let detail = if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
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
                        format!("{cmd} {args}")
                    } else if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
                        url.to_string()
                    } else {
                        "(no command)".into()
                    };
                    println!("  • {name}: {detail}");
                }
            }
        }
        "remove" | "rm" | "delete" => {
            let server_name = name.ok_or_else(|| anyhow::anyhow!("server name required"))?;
            if cfg.remove(&server_name) {
                cfg.save(&root)?;
                println!("[ok] Removed MCP server '{server_name}'.");
            } else {
                anyhow::bail!("MCP server '{server_name}' not found.");
            }
        }
        other => anyhow::bail!("unknown action: '{other}' (use install/list/remove)"),
    }
    Ok(())
}

/// `umadev skill` — install/list/remove skill packages.
fn cmd_skill(action: String, target: Option<String>, project_root: Option<PathBuf>) -> Result<()> {
    let root = project_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let registry = skill_manager::SkillRegistry::new(&root);
    match action.as_str() {
        "install" | "add" => {
            let source = target.ok_or_else(|| {
                anyhow::anyhow!("source directory required: umadev skill install <dir>")
            })?;
            let source_path = std::path::Path::new(&source);
            let result = registry.install(source_path)?;
            println!("[ok] Installed skill '{}'.", result.name);
            println!("  Knowledge files: {}", result.knowledge_copied);
            if result.rules_added > 0 {
                // Governance clauses are all active by default; a skill cannot
                // (yet) enable *extra* clauses — only `.umadev/rules.toml`
                // disables them. Report as "declared" so we don't claim an
                // application step that didn't happen.
                println!(
                    "  Governance rules declared: {} (UmaDev clauses are active by default)",
                    result.rules_added
                );
            }
            if result.prompt_updated {
                println!("  CLAUDE.md updated with skill prompt.");
            }
        }
        "list" | "ls" => {
            let skills = registry.list();
            if skills.is_empty() {
                println!("No skills installed.");
            } else {
                println!("Skills ({}):", skills.len());
                for s in skills {
                    println!("  • {} v{} — {}", s.name, s.version, s.description);
                }
            }
        }
        "remove" | "rm" | "uninstall" => {
            let name = target.ok_or_else(|| anyhow::anyhow!("skill name required"))?;
            registry.remove(&name)?;
            println!("[ok] Removed skill '{name}'.");
        }
        other => anyhow::bail!("unknown action: '{other}' (use install/list/remove)"),
    }
    Ok(())
}

/// `umadev knowledge-manage` — add/list/search/remove custom knowledge.
fn cmd_knowledge(
    action: String,
    target: Option<String>,
    name: Option<String>,
    project_root: Option<PathBuf>,
) -> Result<()> {
    let root = project_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    match action.as_str() {
        "add" => {
            let source = target.ok_or_else(|| {
                anyhow::anyhow!("source path required: umadev knowledge-manage add <path>")
            })?;
            let result = knowledge_manager::add_knowledge(
                &root,
                std::path::Path::new(&source),
                name.as_deref(),
            )?;
            println!("[ok] Added knowledge '{}'.", result.name);
            println!("  Files copied: {}", result.files_copied);
            println!("  → {}", result.dest_dir.display());
        }
        "list" | "ls" => {
            let entries = knowledge_manager::list_knowledge(&root);
            if entries.is_empty() {
                println!("No custom knowledge added.");
            } else {
                println!("Custom knowledge ({}):", entries.len());
                for e in entries {
                    println!("  • {} ({} files) ← {}", e.name, e.file_count, e.source);
                }
            }
        }
        "search" => {
            let query = target.unwrap_or_default();
            if query.is_empty() {
                anyhow::bail!("query required: umadev knowledge-manage search <query>");
            }
            let results = knowledge_manager::search_knowledge(&root, &query, 10);
            if results.is_empty() {
                println!("No matches for '{query}'.");
            } else {
                println!("Search results for '{query}' ({}):", results.len());
                for r in results {
                    // Truncate by CHARS, not bytes — a byte-slice would panic
                    // mid-character on CJK previews (the common case here).
                    let preview: String = r.preview.chars().take(60).collect();
                    println!("  [{:>3}] {} — {}...", r.score, r.path, preview);
                }
            }
        }
        "remove" | "rm" => {
            let name = target.ok_or_else(|| anyhow::anyhow!("knowledge name required"))?;
            knowledge_manager::remove_knowledge(&root, &name)?;
            println!("[ok] Removed knowledge '{name}'.");
        }
        other => anyhow::bail!("unknown action: '{other}' (use add/list/search/remove)"),
    }
    Ok(())
}

/// `umadev ci` — run governance on the whole workspace (CI/CD mode).
fn cmd_ci(report_only: bool, changed_only: bool, project_root: Option<PathBuf>) -> Result<()> {
    let root = project_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let result = ci::run(&ci::CiOptions {
        report_only,
        changed_only,
        project_root: root,
    })?;
    if result.failed {
        std::process::exit(1);
    }
    Ok(())
}

/// The marker the pre-commit hook uses to identify itself (so uninstall is safe).
const PRE_COMMIT_MARKER: &str = "# umadev pre-commit governance hook";

/// Write the `umadev ci` pre-commit git hook into `.git/hooks/pre-commit`.
/// Idempotent — if a UmaDev hook is already present, it's a no-op. A
/// pre-existing non-UmaDev pre-commit hook is PRESERVED: our check is
/// appended below it (it runs first, then ours), so the user's existing commit
/// checks are never disabled.
fn install_pre_commit_hook(project_root: &Path) -> Result<PathBuf> {
    let git_dir = project_root.join(".git");
    if !git_dir.exists() {
        anyhow::bail!(
            "Not a git repository (no .git directory at {}). Run `git init` first.",
            git_dir.display()
        );
    }
    let hooks_dir = git_dir.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("pre-commit");
    let bin = std::env::current_exe().map_or_else(
        |_| "umadev".to_string(),
        |p| p.to_string_lossy().to_string(),
    );
    // If the hook exists and already has our marker, it's idempotent.
    if let Ok(existing) = std::fs::read_to_string(&hook_path) {
        if existing.contains(PRE_COMMIT_MARKER) {
            return Ok(hook_path);
        }
    }
    let our_block = format!(
        "{marker}\n\
         # Runs `umadev ci --changed-only` on staged files before commit.\n\
         # A governance violation aborts the commit. Remove with:\n\
         #   umadev uninstall --host pre-commit\n\
         {bin} ci --changed-only\n",
        marker = PRE_COMMIT_MARKER,
    );
    // Preserve a pre-existing user hook (it ran first) and append our check
    // below — no clobber, no `.bak`. A fresh hook needs a shebang; the existing
    // one already has its own.
    let script = match std::fs::read_to_string(&hook_path) {
        Ok(existing) => format!("{}\n\n{our_block}", existing.trim_end()),
        Err(_) => format!("#!/bin/sh\n{our_block}"),
    };
    std::fs::write(&hook_path, script)?;
    // Make it executable (Unix).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }
    Ok(hook_path)
}

/// Remove the UmaDev pre-commit git hook. Idempotent — does nothing if the
/// hook is absent or is not ours.
fn uninstall_pre_commit_hook(project_root: &Path) -> Result<()> {
    let hook_path = project_root.join(".git/hooks/pre-commit");
    let content = std::fs::read_to_string(&hook_path).unwrap_or_default();
    let Some(idx) = content.find(PRE_COMMIT_MARKER) else {
        return Ok(()); // absent or not ours — nothing to do.
    };
    // Strip ONLY our appended block (marker -> EOF). Install appended it after any
    // pre-existing user hook (`{user}\n\n{our_block}`), so everything before the
    // marker is the user's own hook and MUST be preserved (deleting the whole file
    // would destroy it). When nothing meaningful remains, we created the file
    // ourselves (just a `#!/bin/sh` shebang or empty) — remove it cleanly.
    let kept = content[..idx].trim_end().to_string();
    if kept.is_empty() || kept == "#!/bin/sh" {
        std::fs::remove_file(&hook_path)?;
    } else {
        std::fs::write(&hook_path, format!("{kept}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms)?;
        }
    }
    Ok(())
}

fn cmd_doctor(project_root: Option<PathBuf>) -> Result<()> {
    let workspace = resolve_root(project_root)?;
    let results = doctor::run_all(&workspace);
    print!("{}", doctor::render_report(&workspace, &results));
    if results.iter().any(|r| r.status == doctor::Status::Failed) {
        anyhow::bail!("umadev doctor: one or more checks failed");
    }
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,umadev=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn cmd_init(slug: Option<String>, project_root: Option<PathBuf>, force: bool) -> Result<()> {
    let workspace = resolve_root(project_root)?;
    let slug = match slug {
        Some(s) if !s.is_empty() => s,
        _ => infer_slug(&workspace),
    };
    let manifest = umadev_agent::SpecManifest::new(&slug);
    let path = manifest
        .write_to(&workspace, force)
        .with_context(|| format!("write {}", workspace.join("umadev.yaml").display()))?;

    // Scaffold design infrastructure into the workspace so RAG and
    // /design work out of the box. Uses include_str! so the files are
    // embedded in the binary — no external data directory needed.
    let scaffolded = scaffold_design_infrastructure(&workspace);

    // Write a default .umadevrc config template so users can discover the
    // knowledge / quality / pipeline configuration surface. Idempotent.
    let umadevrc = workspace.join(".umadevrc");
    if !umadevrc.is_file() {
        let template = "# UmaDev project configuration. Edit and re-run to take effect.\n\
# Docs: https://github.com/umacloud/umadev/blob/main/crates/umadev-agent/src/config.rs\n\
\n[quality]\nthreshold = 90           # minimum weighted score to pass the quality gate\nskip_checks = []         # e.g. [\"Dark mode support\"]\n\
\n[pipeline]\nskip_phases = []         # e.g. [\"research\"]\nmax_review_rounds = 3    # doc structural review retries\nauto_approve_gates = true # autonomous mode: auto-approve all gates (like /goal)\n\
\n[knowledge]\nenabled = true           # enable BM25 / hybrid expert-knowledge retrieval\nengine = \"bm25\"          # bm25 (offline) or hybrid (needs OPENAI_API_KEY)\ntop_k = 6                # knowledge chunks injected per phase\n";
        let _ = std::fs::write(&umadevrc, template);
        println!("  config:  {}", umadevrc.display());
    }

    // Generate CLAUDE.md so the host (claude code) knows it's in a UmaDev
    // project and reads the coach prompt. This bridges the gap between
    // UmaDev's orchestration and the host's awareness — without it, the
    // host doesn't know to follow UmaDev's pipeline instructions.
    let claude_md = workspace.join("CLAUDE.md");
    if !claude_md.is_file() {
        let claude_content = format!(
            "# CLAUDE.md — UmaDev managed project\n\n\
             This project is managed by **UmaDev** ({version}), an AI coding project-director Agent.\n\n\
             ## How this works\n\n\
             1. UmaDev orchestrates a 9-phase pipeline (clarify → research → docs → spec → frontend → backend → quality → delivery).\n\
             2. Each phase, UmaDev writes a **coach prompt** to `.umadev/coach/CURRENT.md`.\n\
             3. **Read `.umadev/coach/CURRENT.md` at the start of every task** — it contains your role, task, and constraints.\n\
             4. After completing the phase output, run `umadev continue` to advance.\n\n\
             ## Rules (non-negotiable)\n\n\
             - **No emoji as functional icons** — use Lucide / Heroicons / Tabler icon libraries.\n\
             - **No hardcoded colors** — use CSS design tokens (Tailwind config).\n\
             - **No secrets in source code** — use environment variables.\n\
             - **Follow the spec preamble** in each coach prompt.\n\n\
             ## Governance\n\n\
             Your Write/Edit/Bash calls are intercepted by UmaDev's governance hook.\n\
             Violations are blocked with a specific reason and fix suggestion.\n\
             Configure: `.umadev/rules.toml`.\n",
            version = env!("CARGO_PKG_VERSION"),
        );
        let _ = std::fs::write(&claude_md, claude_content);
        println!("  claude:  {}", claude_md.display());
    }

    // Generate .umadev/rules.toml template so users can discover the
    // governance customization surface. Idempotent — won't overwrite.
    let rules_path = umadev_governance::Policy::write_default_template(&workspace);
    if let Ok(p) = &rules_path {
        println!("  rules:   {}", p.display());
    }

    // Generate .gitignore with UmaDev-specific entries.
    let gitignore = workspace.join(".gitignore");
    if !gitignore.is_file() {
        let content = "\
# UmaDev runtime data — all of .umadev/ is per-project state, never commit it
# (workflow state, audit logs, the checkpoints shadow-git, KB index, history).
.umadev/
output/
# Generated when routing a third-party API through opencode (holds an env-var
# reference, not a raw key, but it's machine-generated routing config).
opencode.json
";
        let _ = std::fs::write(&gitignore, content);
        println!("  gitignore: {}", gitignore.display());
    }

    println!("UmaDev workspace initialised.");
    println!("  manifest: {}", path.display());
    println!(
        "  spec: {} | level: {} | profile: {} | slug: {slug}",
        umadev_spec::SPEC_VERSION,
        manifest.level.as_str(),
        manifest.profile.as_str(),
    );
    if scaffolded > 0 {
        println!("  design: {scaffolded} files scaffolded into knowledge/");
    }
    println!("\nNext steps:");
    println!("  umadev                          # launch the TUI (recommended)");
    println!("  umadev run \"<requirement>\"      # or scripted / CI form");
    println!();
    println!("Inside the TUI:");
    println!("  /claude, /codex, /opencode   switch base CLI (each uses its OWN login + model)");
    println!("  /status                      show the active base and its driving model");
    Ok(())
}

fn scaffold_design_infrastructure(workspace: &Path) -> usize {
    let files: &[(&str, &str)] = &[
        (
            "knowledge/design-systems/modern-minimal.md",
            include_str!("../../../knowledge/design-systems/modern-minimal.md"),
        ),
        (
            "knowledge/design-systems/editorial-clean.md",
            include_str!("../../../knowledge/design-systems/editorial-clean.md"),
        ),
        (
            "knowledge/design-systems/tech-utility.md",
            include_str!("../../../knowledge/design-systems/tech-utility.md"),
        ),
        (
            "knowledge/design-systems/soft-warm.md",
            include_str!("../../../knowledge/design-systems/soft-warm.md"),
        ),
        (
            "knowledge/design-systems/bold-geometric.md",
            include_str!("../../../knowledge/design-systems/bold-geometric.md"),
        ),
        (
            "knowledge/design-systems/00-craft-rules.md",
            include_str!("../../../knowledge/design-systems/00-craft-rules.md"),
        ),
        (
            "knowledge/design-systems/anti-ai-slop.md",
            include_str!("../../../knowledge/design-systems/anti-ai-slop.md"),
        ),
        (
            "knowledge/design-systems/brutalist-bold.md",
            include_str!("../../../knowledge/design-systems/brutalist-bold.md"),
        ),
        (
            "knowledge/design-systems/glass-aurora.md",
            include_str!("../../../knowledge/design-systems/glass-aurora.md"),
        ),
        (
            "knowledge/design-systems/premium-luxury.md",
            include_str!("../../../knowledge/design-systems/premium-luxury.md"),
        ),
        (
            "knowledge/design-systems/product-type-design-map.md",
            include_str!("../../../knowledge/design-systems/product-type-design-map.md"),
        ),
        (
            "knowledge/design-systems/aesthetic-families.md",
            include_str!("../../../knowledge/design-systems/aesthetic-families.md"),
        ),
        (
            "knowledge/seed-templates/saas-landing.md",
            include_str!("../../../knowledge/seed-templates/saas-landing.md"),
        ),
        (
            "knowledge/seed-templates/dashboard.md",
            include_str!("../../../knowledge/seed-templates/dashboard.md"),
        ),
        (
            "knowledge/seed-templates/blog-content.md",
            include_str!("../../../knowledge/seed-templates/blog-content.md"),
        ),
        (
            "knowledge/seed-templates/e-commerce.md",
            include_str!("../../../knowledge/seed-templates/e-commerce.md"),
        ),
        (
            "knowledge/seed-templates/auth-system.md",
            include_str!("../../../knowledge/seed-templates/auth-system.md"),
        ),
        (
            "knowledge/seed-templates/settings-page.md",
            include_str!("../../../knowledge/seed-templates/settings-page.md"),
        ),
        (
            "knowledge/seed-templates/docs-site.md",
            include_str!("../../../knowledge/seed-templates/docs-site.md"),
        ),
        // Expert methodology knowledge
        (
            "knowledge/experts/product-manager/methodology.md",
            include_str!("../../../knowledge/experts/product-manager/methodology.md"),
        ),
        (
            "knowledge/experts/architect/api-design.md",
            include_str!("../../../knowledge/experts/architect/api-design.md"),
        ),
        (
            "knowledge/experts/architect/security.md",
            include_str!("../../../knowledge/experts/architect/security.md"),
        ),
        (
            "knowledge/experts/frontend-lead/methodology.md",
            include_str!("../../../knowledge/experts/frontend-lead/methodology.md"),
        ),
        (
            "knowledge/experts/backend-lead/methodology.md",
            include_str!("../../../knowledge/experts/backend-lead/methodology.md"),
        ),
        (
            "knowledge/experts/qa-lead/test-strategy.md",
            include_str!("../../../knowledge/experts/qa-lead/test-strategy.md"),
        ),
        (
            "knowledge/experts/uiux-designer/methodology.md",
            include_str!("../../../knowledge/experts/uiux-designer/methodology.md"),
        ),
        (
            "knowledge/experts/devops/methodology.md",
            include_str!("../../../knowledge/experts/devops/methodology.md"),
        ),
        // Engineering-structure standards — how to layer, package, and write
        // the service layer for a commercial-grade codebase. Seeded so they get
        // BM25-indexed and injected into the backend / frontend phases.
        (
            "knowledge/backend/01-standards/application-layering-and-packaging.md",
            include_str!(
                "../../../knowledge/backend/01-standards/application-layering-and-packaging.md"
            ),
        ),
        (
            "knowledge/frontend/01-standards/frontend-architecture-and-layering.md",
            include_str!(
                "../../../knowledge/frontend/01-standards/frontend-architecture-and-layering.md"
            ),
        ),
        (
            "knowledge/backend/01-standards/api-and-error-conventions.md",
            include_str!("../../../knowledge/backend/01-standards/api-and-error-conventions.md"),
        ),
        (
            "knowledge/backend/01-standards/data-modeling-and-persistence.md",
            include_str!(
                "../../../knowledge/backend/01-standards/data-modeling-and-persistence.md"
            ),
        ),
        (
            "knowledge/backend/01-standards/config-and-observability.md",
            include_str!("../../../knowledge/backend/01-standards/config-and-observability.md"),
        ),
        (
            "knowledge/security/01-standards/secure-coding-baseline.md",
            include_str!("../../../knowledge/security/01-standards/secure-coding-baseline.md"),
        ),
        (
            "knowledge/testing/01-standards/test-strategy-and-layering.md",
            include_str!("../../../knowledge/testing/01-standards/test-strategy-and-layering.md"),
        ),
        (
            "knowledge/cicd/01-standards/deployment-and-delivery-standard.md",
            include_str!(
                "../../../knowledge/cicd/01-standards/deployment-and-delivery-standard.md"
            ),
        ),
        (
            "knowledge/performance/01-standards/performance-and-scalability.md",
            include_str!(
                "../../../knowledge/performance/01-standards/performance-and-scalability.md"
            ),
        ),
        // Core feature standards — auth & forms are in every commercial app and
        // the most security/UX-critical to get right.
        (
            "knowledge/backend/01-standards/auth-implementation.md",
            include_str!("../../../knowledge/backend/01-standards/auth-implementation.md"),
        ),
        (
            "knowledge/frontend/01-standards/forms-and-validation.md",
            include_str!("../../../knowledge/frontend/01-standards/forms-and-validation.md"),
        ),
        (
            "knowledge/backend/01-standards/payment-integration.md",
            include_str!("../../../knowledge/backend/01-standards/payment-integration.md"),
        ),
        (
            "knowledge/backend/01-standards/file-upload-and-storage.md",
            include_str!("../../../knowledge/backend/01-standards/file-upload-and-storage.md"),
        ),
        (
            "knowledge/backend/01-standards/background-jobs-and-async.md",
            include_str!("../../../knowledge/backend/01-standards/background-jobs-and-async.md"),
        ),
        (
            "knowledge/backend/01-standards/email-and-notifications.md",
            include_str!("../../../knowledge/backend/01-standards/email-and-notifications.md"),
        ),
        (
            "knowledge/backend/01-standards/search-and-filtering.md",
            include_str!("../../../knowledge/backend/01-standards/search-and-filtering.md"),
        ),
        (
            "knowledge/backend/01-standards/realtime-and-websocket.md",
            include_str!("../../../knowledge/backend/01-standards/realtime-and-websocket.md"),
        ),
        (
            "knowledge/frontend/01-standards/i18n-and-localization.md",
            include_str!("../../../knowledge/frontend/01-standards/i18n-and-localization.md"),
        ),
        (
            "knowledge/frontend/01-standards/accessibility-standard.md",
            include_str!("../../../knowledge/frontend/01-standards/accessibility-standard.md"),
        ),
        // Deep design assets — the full token architecture + complete a11y
        // spec. These are the backbone of premium, non-AI-looking UI.
        (
            "knowledge/frontend/01-standards/design-tokens-complete.md",
            include_str!("../../../knowledge/frontend/01-standards/design-tokens-complete.md"),
        ),
        (
            "knowledge/frontend/01-standards/accessibility-complete.md",
            include_str!("../../../knowledge/frontend/01-standards/accessibility-complete.md"),
        ),
        // Multi-platform standards —商业开发不只 web：移动/桌面/小程序/鸿蒙/跨平台。
        (
            "knowledge/cross-platform/01-standards/platform-selection-and-architecture.md",
            include_str!(
                "../../../knowledge/cross-platform/01-standards/platform-selection-and-architecture.md"
            ),
        ),
        (
            "knowledge/cross-platform/01-standards/cross-platform-frameworks.md",
            include_str!(
                "../../../knowledge/cross-platform/01-standards/cross-platform-frameworks.md"
            ),
        ),
        (
            "knowledge/mobile/01-standards/mobile-app-standard.md",
            include_str!("../../../knowledge/mobile/01-standards/mobile-app-standard.md"),
        ),
        (
            "knowledge/harmony/01-standards/harmonyos-arkts-standard.md",
            include_str!("../../../knowledge/harmony/01-standards/harmonyos-arkts-standard.md"),
        ),
        (
            "knowledge/miniprogram/01-standards/miniprogram-standard.md",
            include_str!("../../../knowledge/miniprogram/01-standards/miniprogram-standard.md"),
        ),
        (
            "knowledge/desktop/01-standards/desktop-app-standard.md",
            include_str!("../../../knowledge/desktop/01-standards/desktop-app-standard.md"),
        ),
        // Official platform DESIGN guidelines — Apple HIG / Material 3 /
        // HarmonyOS Design / WeChat mini-program design. This is where a raw
        // base CLI most often produces non-native-looking UI.
        (
            "knowledge/mobile/01-standards/ios-design-hig.md",
            include_str!("../../../knowledge/mobile/01-standards/ios-design-hig.md"),
        ),
        (
            "knowledge/mobile/01-standards/android-material-design.md",
            include_str!("../../../knowledge/mobile/01-standards/android-material-design.md"),
        ),
        (
            "knowledge/harmony/01-standards/harmonyos-design.md",
            include_str!("../../../knowledge/harmony/01-standards/harmonyos-design.md"),
        ),
        (
            "knowledge/miniprogram/01-standards/miniprogram-design.md",
            include_str!("../../../knowledge/miniprogram/01-standards/miniprogram-design.md"),
        ),
        (
            "knowledge/desktop/01-standards/desktop-design.md",
            include_str!("../../../knowledge/desktop/01-standards/desktop-design.md"),
        ),
        // Web framework official best practices + AI/LLM application standard —
        // high-volume areas where a raw base CLI most often misses official
        // patterns (Next App Router caching/RSC) or builds unsafe AI apps.
        (
            "knowledge/frontend/01-standards/web-framework-best-practices.md",
            include_str!(
                "../../../knowledge/frontend/01-standards/web-framework-best-practices.md"
            ),
        ),
        (
            "knowledge/backend/01-standards/llm-application-standard.md",
            include_str!("../../../knowledge/backend/01-standards/llm-application-standard.md"),
        ),
        (
            "knowledge/frontend/01-standards/seo-and-web-vitals.md",
            include_str!("../../../knowledge/frontend/01-standards/seo-and-web-vitals.md"),
        ),
        (
            "knowledge/cicd/01-standards/release-and-store-submission.md",
            include_str!("../../../knowledge/cicd/01-standards/release-and-store-submission.md"),
        ),
        (
            "knowledge/backend/01-standards/analytics-and-growth.md",
            include_str!("../../../knowledge/backend/01-standards/analytics-and-growth.md"),
        ),
        (
            "knowledge/backend/01-standards/backend-framework-idioms.md",
            include_str!("../../../knowledge/backend/01-standards/backend-framework-idioms.md"),
        ),
        (
            "knowledge/backend/01-standards/microservices-and-distributed.md",
            include_str!("../../../knowledge/backend/01-standards/microservices-and-distributed.md"),
        ),
        (
            "knowledge/frontend/01-standards/admin-dashboard-and-crud.md",
            include_str!("../../../knowledge/frontend/01-standards/admin-dashboard-and-crud.md"),
        ),
    ];
    let mut count = 0;
    for (rel, content) in files {
        let target = workspace.join(rel);
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&target, content).is_ok() {
            count += 1;
        }
    }
    count
}

/// Launch the conversational TUI. With no CLI flags this is the
/// recommended entry — same as running `umadev` bare.
async fn cmd_tui() -> Result<()> {
    let project_root = resolve_root(None)?;
    // Pre-register the real-time governance PreToolUse hook so that whenever
    // the session drives Claude Code (the default/most-common base, and even
    // after an in-session `/claude` switch) its file writes are governed live.
    // Idempotent + merges; inert for codex/opencode/offline (they don't read
    // `.claude/settings.json`), so it's safe to install unconditionally.
    let _ = hook::install_claude_hook(&project_root);
    let opts = umadev_tui::LaunchOptions {
        project_root,
        slug: String::new(),
        // Empty: never impose a model — the base CLI uses whatever model IT is
        // configured with (login default / connected API / local). The TUI path
        // additionally honours an explicit `/model` override in `run()`.
        model: String::new(),
    };
    umadev_tui::run(opts).await.context("TUI session failed")
}

/// Bundled arguments for `cmd_run` (keeps the signature readable).
struct RunArgs {
    requirement: String,
    backend: Option<BackendArg>,
    model: String,
    project_root: Option<PathBuf>,
    slug: String,
}

/// Attach a live event sink to the runner and spawn a background printer.
/// Returns the sink-equipped runner and a JoinHandle for the printer task.
/// The caller must `drop(runner)` then `await` the handle after their block
/// finishes, so the channel closes cleanly.
fn attach_live_sink<R: umadev_runtime::Runtime>(
    runner: AgentRunner<R>,
) -> (AgentRunner<R>, tokio::task::JoinHandle<()>) {
    let (sink, mut rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let runner = runner.with_event_sink(sink);
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            print_engine_event(&event);
        }
    });
    (runner, printer)
}

/// Render a single engine event to stderr for live CLI progress.
fn print_engine_event(event: &umadev_agent::EngineEvent) {
    use umadev_agent::EngineEvent;
    match event {
        EngineEvent::PipelineStarted { slug, .. } => {
            eprintln!("> Pipeline started: {slug}");
        }
        EngineEvent::PhaseStarted { phase } => {
            eprintln!("> Phase: {}", phase.id());
        }
        EngineEvent::Note(msg) => {
            eprintln!("  {msg}");
        }
        EngineEvent::HostOutput { phase, line } => {
            eprintln!("  │ [{phase:?}] {line}");
        }
        EngineEvent::ArtifactWritten { phase, path } => {
            eprintln!("  [ok] {} → {}", phase.id(), path.display());
        }
        EngineEvent::PhaseCompleted { phase } => {
            eprintln!("  [ok] {} complete", phase.id());
        }
        EngineEvent::GateOpened { gate } => {
            eprintln!(
                "
[gate]  Gate: {} — run `umadev continue` to proceed.",
                gate.id_str()
            );
        }
        EngineEvent::BlockCompleted { final_phase, .. } => {
            eprintln!("[ok] Block complete at {}", final_phase.id());
        }
        EngineEvent::VerifyPassed { phase, duration_ms } => {
            eprintln!("  [ok] {} verify OK ({}ms)", phase.id(), duration_ms);
        }
        EngineEvent::VerifyFailed { phase, .. } => {
            eprintln!("  [fail] {} verify FAILED", phase.id());
        }
        EngineEvent::VerifySkipped { phase, .. } => {
            eprintln!("  [skip] {} verify skipped", phase.id());
        }
        _ => {}
    }
    let _ = std::io::stderr().flush();
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    // Reject an empty / whitespace-only requirement up front with a helpful
    // message, rather than running the whole pipeline on nothing.
    if args.requirement.trim().is_empty() {
        anyhow::bail!(
            "empty requirement — describe what to build, e.g.\n  \
             umadev run \"做一个带邮箱登录的 SaaS 落地页\""
        );
    }
    let project_root = resolve_root(args.project_root)?;
    let opts = RunOptions {
        project_root: project_root.clone(),
        requirement: args.requirement,
        slug: args.slug,
        model: args.model,
        backend: args
            .backend
            .as_ref()
            .map_or(String::new(), |b| b.id().to_string()),
        design_system: String::new(),
        seed_template: String::new(),
    };

    // Two modes:
    //   --backend <host>  → drive a logged-in base CLI as the worker
    //   (default)         → offline deterministic templates
    let (report, runtime_label) = if let Some(backend) = args.backend {
        let mut driver = umadev_host::driver_for(backend.id())
            .ok_or_else(|| anyhow::anyhow!("unknown backend `{}`", backend.id()))?;
        // Run the base subprocess IN the project root — it reads/writes files
        // relative to its cwd, which differs from the launching cwd whenever
        // `--project-root` points elsewhere.
        driver.set_workspace(project_root.clone());
        match driver.probe().await {
            umadev_host::ProbeResult::Ready { version } => {
                println!("Backend {} ready ({version}).", driver.display_name());
                // Real-time governance: install the PreToolUse hook so the
                // driven `claude` subprocess fires our governor on EVERY file
                // write (not just the doc-embedded code the quality gate scans).
                // Idempotent + merges, so it never clobbers the user's
                // .claude/settings.json. This is the "hooks auto-registered"
                // behavior CLAUDE.md promises — without it the base writes
                // ungoverned in real time. Claude Code is the only base with a
                // PreToolUse hook surface.
                if backend.id() == "claude-code" {
                    if let Ok(p) = hook::install_claude_hook(&project_root) {
                        eprintln!(
                            "  [governance] real-time PreToolUse hook active ({})",
                            p.display()
                        );
                    }
                }
            }
            umadev_host::ProbeResult::NotInstalled { program } => {
                anyhow::bail!(
                    "backend `{}` not available: `{program}` is not on PATH. \
                     Install / log in to the base CLI first, or omit --backend to run offline.",
                    backend.id()
                );
            }
            umadev_host::ProbeResult::Unhealthy { detail } => {
                anyhow::bail!("backend `{}` is unhealthy: {detail}", backend.id());
            }
        }
        let label = format!(
            "Base CLI worker — {} ({})",
            driver.display_name(),
            backend.id()
        );
        let runner = AgentRunner::new(driver, opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        let report = runner
            .run_clarify(true)
            .await
            .context("clarify phase failure")?;
        drop(runner);
        let _ = printer.await;
        (report, label)
    } else {
        let label = "Offline deterministic templates (no AI; demos / CI)".to_string();
        let runner = AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        let report = runner
            .run_clarify(false)
            .await
            .context("clarify phase failure")?;
        drop(runner);
        let _ = printer.await;
        (report, label)
    };

    print_report(&project_root, &runtime_label, &report);
    Ok(())
}

fn print_report(project_root: &Path, runtime_label: &str, report: &RunReport) {
    println!(
        "UmaDev — {}.",
        if report.paused_at.is_some() {
            "pipeline paused"
        } else if report.final_phase == umadev_spec::Phase::Delivery {
            "pipeline complete"
        } else {
            // No gate, but didn't reach delivery — the quality gate blocked it.
            // Don't print "complete" for a blocked run.
            "pipeline stopped before delivery (quality gate blocked — see the quality report)"
        }
    );
    println!("  workspace: {}", project_root.display());
    println!("  runtime: {runtime_label}");
    println!(
        "  final phase: {} | active gate: {}",
        report.final_phase.id(),
        report.paused_at.map_or("none", Gate::id_str)
    );
    println!("  artifacts:");
    for phase_out in &report.completed {
        for a in &phase_out.artifacts {
            if let Ok(rel) = a.strip_prefix(project_root) {
                println!("    - {}", rel.display());
            } else {
                println!("    - {}", a.display());
            }
        }
    }
    match report.paused_at {
        Some(Gate::ClarifyGate) => {
            println!("\nAnswer the clarifying questions in output/<slug>-clarify.md, then run:");
            println!("  umadev continue       # submit answers and proceed to research");
            println!("  umadev revise \"...\"     # re-generate the questions");
        }
        Some(Gate::DocsConfirm) => {
            println!("\nReview the three core docs, then run:");
            println!("  umadev continue       # approve and advance");
            println!("  umadev revise \"…\"     # request changes inside the gate");
        }
        Some(Gate::PreviewConfirm) => {
            println!("\nVerify the preview matches the UIUX doc, then run:");
            println!(
                "  umadev continue       # approve and advance to backend / quality / delivery"
            );
            println!("  umadev revise \"…\"     # request changes inside the gate");
        }
        None => {
            println!("\nDelivery complete. The proof pack lives in `release/`.");
            println!("Run `umadev report` if you want to refresh the compliance mapping.");
        }
    }
}

/// Which block to drive relative to the active gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateBlock {
    /// Approve the gate and advance past it.
    Continue,
    /// Keep the gate; regenerate the artifacts that produced it.
    Revise,
}

/// Parse the persisted `active_gate` string into a [`Gate`], or bail with
/// a clear message when none is open / the value is unrecognised.
///
/// **Intra-phase recovery:** when `active_gate` is empty but the persisted
/// `phase` indicates the pipeline was interrupted mid-block (e.g. a process
/// kill during `docs` generation), we infer the gate to re-run from the
/// phase. This lets `umadev continue` recover an interrupted run instead
/// of dead-locking on "no active gate". The re-run regenerates the
/// interrupted block's artifacts (idempotent — overwrites the partial output)
/// and pauses at the correct gate for review.
fn resolve_active_gate(state: &umadev_agent::WorkflowState) -> Result<Gate> {
    if state.active_gate.is_empty() {
        // Try to recover from an intra-phase interruption by inferring the
        // gate from the current phase position.
        if let Some(gate) = infer_gate_from_phase(&state.phase) {
            eprintln!(
                "[warn] No active gate (phase: {}). Re-running the {} block to recover from an interruption.",
                state.phase,
                gate.id_str()
            );
            return Ok(gate);
        }
        anyhow::bail!(
            "no active gate to approve (current phase: {}). \
             The pipeline may have completed, or the state is stale. \
             Run `umadev run` to start fresh, or `umadev rollback latest` to undo.",
            state.phase
        );
    }
    // Delegate to the spec crate's typed parser (case-insensitive,
    // fail-open) instead of hand-matching the two ids here — keeps this
    // call site in sync with Gate if a third gate is ever added.
    match Gate::from_id(&state.active_gate) {
        Some(g) => Ok(g),
        None => anyhow::bail!(
            "unknown active_gate `{}` in workflow-state.json",
            state.active_gate
        ),
    }
}

/// Infer which gate block to re-run when the pipeline was interrupted
/// mid-phase (no `active_gate` persisted). Returns `None` for gate phases
/// themselves (which should have an `active_gate`) or unknown phases.
fn infer_gate_from_phase(phase: &str) -> Option<Gate> {
    let p = phase.to_ascii_lowercase();
    match p.as_str() {
        // Interrupted during research or docs generation → re-run the
        // initial block (research → docs → pause at docs_confirm).
        "research" | "docs" => Some(Gate::ClarifyGate),
        // Interrupted during spec or frontend generation → re-run the
        // docs_confirm block (spec → frontend → pause at preview_confirm).
        "spec" | "frontend" => Some(Gate::DocsConfirm),
        // Interrupted during backend / quality / delivery → re-run the
        // preview_confirm block (backend → quality → delivery).
        "backend" | "quality" | "delivery" => Some(Gate::PreviewConfirm),
        // Gate phases or unknown — can't infer.
        _ => None,
    }
}

/// Shared driver for `continue` / `revise`: reconstruct `RunOptions` from
/// the persisted state, resolve + probe the backend, run the appropriate
/// block, and print the report. `requirement_override` lets `revise` fold
/// the user's feedback into the worker prompt.
async fn drive_gate_block(
    project_root: &Path,
    state: &umadev_agent::WorkflowState,
    gate: Gate,
    backend_override: Option<BackendArg>,
    requirement_override: Option<String>,
    mode: GateBlock,
) -> Result<()> {
    // Reconstruct slug + requirement so `continue` / `revise` resolve the
    // same artifacts the original `run` produced.
    let slug = if state.slug.is_empty() {
        infer_slug(project_root)
    } else {
        state.slug.clone()
    };
    let requirement = requirement_override.unwrap_or_else(|| {
        if state.requirement.is_empty() {
            state
                .note
                .split_once(": ")
                .map_or("(no requirement recorded)", |x| x.1)
                .to_string()
        } else {
            state.requirement.clone()
        }
    });

    // Resolve backend: explicit flag > persisted state > offline.
    let backend_id: Option<String> = backend_override
        .as_ref()
        .map(|b| b.id().to_string())
        .or_else(|| {
            if state.backend.is_empty() {
                None
            } else {
                Some(state.backend.clone())
            }
        });

    let opts = RunOptions {
        project_root: project_root.to_path_buf(),
        requirement,
        slug,
        // Empty: never impose a model — the base CLI uses whatever model IT is
        // configured with (login default / connected API / local). The TUI path
        // additionally honours an explicit `/model` override in `run()`.
        model: String::new(),
        backend: backend_id.clone().unwrap_or_default(),
        design_system: String::new(),
        seed_template: String::new(),
    };

    let use_runtime = backend_id.is_some();
    let (report, runtime_label) = if let Some(id) = backend_id {
        let backend = BackendArg::from_id(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown backend `{id}` in workflow-state.json"))?;
        let mut driver = umadev_host::driver_for(backend.id())
            .ok_or_else(|| anyhow::anyhow!("no driver registered for `{}`", backend.id()))?;
        driver.set_workspace(project_root.to_path_buf());
        match driver.probe().await {
            umadev_host::ProbeResult::Ready { version } => {
                println!("Backend {} ready ({version}).", driver.display_name());
                // Real-time governance: install the PreToolUse hook so the
                // driven `claude` subprocess fires our governor on EVERY file
                // write (not just the doc-embedded code the quality gate scans).
                // Idempotent + merges, so it never clobbers the user's
                // .claude/settings.json. This is the "hooks auto-registered"
                // behavior CLAUDE.md promises — without it the base writes
                // ungoverned in real time. Claude Code is the only base with a
                // PreToolUse hook surface.
                if backend.id() == "claude-code" {
                    if let Ok(p) = hook::install_claude_hook(project_root) {
                        eprintln!(
                            "  [governance] real-time PreToolUse hook active ({})",
                            p.display()
                        );
                    }
                }
            }
            umadev_host::ProbeResult::NotInstalled { program } => {
                anyhow::bail!(
                    "backend `{}` not available: `{program}` is not on PATH. \
                     Pass --backend offline (or no --backend) to fall back.",
                    backend.id()
                );
            }
            umadev_host::ProbeResult::Unhealthy { detail } => {
                anyhow::bail!("backend `{}` is unhealthy: {detail}", backend.id());
            }
        }
        let label = format!(
            "Base CLI worker — {} ({})",
            driver.display_name(),
            backend.id()
        );
        let runner = AgentRunner::new(driver, opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        let report = if mode == GateBlock::Continue {
            runner.continue_from_gate(gate).await
        } else {
            runner.revise_at_gate(gate, use_runtime).await
        }
        .context("pipeline failure")?;
        drop(runner);
        let _ = printer.await;
        (report, label)
    } else {
        let runner = AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        let report = if mode == GateBlock::Continue {
            runner.continue_from_gate(gate).await
        } else {
            runner.revise_at_gate(gate, use_runtime).await
        }
        .context("pipeline failure")?;
        drop(runner);
        let _ = printer.await;
        (
            report,
            "Offline deterministic templates (no AI; demos / CI)".to_string(),
        )
    };

    print_report(project_root, &runtime_label, &report);
    Ok(())
}

async fn cmd_continue(
    project_root: Option<PathBuf>,
    backend_override: Option<BackendArg>,
) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let state = match umadev_agent::read_workflow_state_diagnostic(&project_root) {
        umadev_agent::ReadState::Ok(s) => s,
        umadev_agent::ReadState::Missing => anyhow::bail!(
            "no .umadev/workflow-state.json — run `umadev run` first"
        ),
        umadev_agent::ReadState::Corrupt { path, error } => anyhow::bail!(
            "workflow-state.json at {} is corrupt ({error}).              Run `umadev rollback latest` or delete it, then `umadev run` again.",
            path.display()
        ),
    };
    let gate = resolve_active_gate(&state)?;

    // Record the approval as evidence
    let clause = match gate {
        Gate::ClarifyGate => "UD-FLOW-001",
        Gate::DocsConfirm => "UD-FLOW-002",
        Gate::PreviewConfirm => "UD-FLOW-003",
    };
    let _ = record_tool_call(
        &project_root,
        "umadev/cli.continue",
        "",
        "approved",
        clause,
        &format!("user approved gate {}", gate.id_str()),
        "",
        None,
    );

    drive_gate_block(
        &project_root,
        &state,
        gate,
        backend_override,
        None,
        GateBlock::Continue,
    )
    .await
}

async fn cmd_revise(text: String, project_root: Option<PathBuf>) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let state = match umadev_agent::read_workflow_state_diagnostic(&project_root) {
        umadev_agent::ReadState::Ok(s) => s,
        umadev_agent::ReadState::Missing => anyhow::bail!(
            "no .umadev/workflow-state.json — run `umadev run` first"
        ),
        umadev_agent::ReadState::Corrupt { path, error } => anyhow::bail!(
            "workflow-state.json at {} is corrupt ({error}).              Run `umadev rollback latest` or delete it, then `umadev run` again.",
            path.display()
        ),
    };
    let gate = resolve_active_gate(&state)?;
    let outcome = classify_reply(&text);
    match outcome {
        GateOutcome::Revise(notes) => {
            let _ = record_tool_call(
                &project_root,
                "umadev/cli.revise",
                "",
                "revise",
                "UD-FLOW-004",
                &notes,
                "",
                None,
            );
            // 4.8 auto-sediment: capture this revision as an ADR + lesson.
            let _ = umadev_agent::capture_gate_revision(
                &project_root,
                &state.active_gate,
                &notes,
                &state.requirement,
            );
            println!(
                "Revising at gate `{}` — regenerating artifacts with your feedback…",
                state.active_gate
            );
            // Fold the revision into the requirement so the worker
            // actually incorporates the feedback, then re-run the block
            // that produced this gate (docs for docs_confirm, frontend
            // for preview_confirm). The pipeline pauses at the same gate.
            let base_req = if state.requirement.is_empty() {
                state
                    .note
                    .split_once(": ")
                    .map_or(String::new(), |x| x.1.to_string())
            } else {
                state.requirement.clone()
            };
            let revised = format!("{base_req}\n\n## Revision request\n{notes}");
            drive_gate_block(
                &project_root,
                &state,
                gate,
                None,
                Some(revised),
                GateBlock::Revise,
            )
            .await
        }
        GateOutcome::Approved => {
            // Defensive: user said "继续" via revise — treat as approval.
            println!("input parsed as approval; treating as `continue`.");
            cmd_continue(Some(project_root), None).await
        }
        GateOutcome::Cancelled => {
            anyhow::bail!("user cancelled the pipeline");
        }
    }
}

fn cmd_spec(clauses_only: bool) -> Result<()> {
    if clauses_only {
        println!("# {SPEC_VERSION} — clause table\n");
        println!("| ID | Layer | Level | Section | Title |");
        println!("|---|---|---|---|---|");
        for c in CLAUSES {
            println!(
                "| {} | {:?} | {:?} | {} | {} |",
                c.id, c.layer, c.level, c.section, c.title
            );
        }
        let chain: Vec<&str> = PHASE_CHAIN.iter().map(|p| p.id()).collect();
        println!("\nPhase chain: {}", chain.join(" → "));
        return Ok(());
    }
    println!("{}", include_str!("../../../spec/UMADEV_HOST_SPEC_V1.md"));
    Ok(())
}

fn cmd_history(project_root: Option<PathBuf>) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let snaps = list_snapshots(&project_root);
    if snaps.is_empty() {
        println!(
            "No snapshots yet. Snapshots are created automatically on every phase transition."
        );
        println!("Run `umadev run` / `umadev continue` to advance the pipeline.");
        return Ok(());
    }
    println!(
        "Available rollback snapshots (newest first):
"
    );
    let state = read_workflow_state(&project_root);
    for (i, ts) in snaps.iter().enumerate() {
        // Try to show what phase each snapshot would restore.
        let snap_path = project_root
            .join(".umadev/history")
            .join(format!("{ts}.json"));
        let phase = std::fs::read_to_string(&snap_path)
            .ok()
            .and_then(|t| serde_json::from_str::<WorkflowState>(&t).ok())
            .map_or("?".to_string(), |s| s.phase);
        let marker = if i == 0 { " (latest)" } else { "" };
        println!("  {ts}  →  phase: {phase}{marker}");
    }
    let _ = state;
    println!(
        "
Roll back with:  umadev rollback latest"
    );
    Ok(())
}

fn cmd_rollback(timestamp: String, project_root: Option<PathBuf>) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let snaps = list_snapshots(&project_root);
    if snaps.is_empty() {
        anyhow::bail!("no snapshots available — run `umadev history` to check");
    }
    let target = if timestamp == "latest" {
        snaps[0].clone()
    } else {
        // Allow partial match (e.g. user passes 20260614T12 to match 20260614T120000.123).
        let matches: Vec<&String> = snaps.iter().filter(|s| s.starts_with(&timestamp)).collect();
        match matches.len() {
            0 => anyhow::bail!("no snapshot matches `{timestamp}`. Run `umadev history`."),
            1 => matches[0].clone(),
            _ => anyhow::bail!(
                "`{timestamp}` is ambiguous ({} matches). Use more digits.",
                matches.len()
            ),
        }
    };
    let before = read_workflow_state(&project_root).map_or("none".to_string(), |s| s.phase);
    restore_snapshot(&project_root, &target)?;
    let after = read_workflow_state(&project_root).map_or("none".to_string(), |s| s.phase);
    println!("Rolled back to snapshot {target}.");
    println!("  phase: {before} → {after}");
    println!("Note: artifacts on disk are NOT reverted — only workflow-state.json.");
    println!("      The pipeline now resumes from phase `{after}` on the next `umadev continue`.");
    Ok(())
}

fn cmd_verify(project_root: Option<PathBuf>) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    println!("workspace: {}", project_root.display());
    println!(
        "umadev: {} (spec {SPEC_VERSION})",
        env!("CARGO_PKG_VERSION")
    );

    // --- spec manifest (UD-META-001) ---
    println!("\n## Spec manifest");
    if let Some(m) = umadev_agent::SpecManifest::read_from(&project_root) {
        println!(
            "  umadev.yaml: version={} level={} profile={} declared_by={}",
            m.spec_version,
            m.level.as_str(),
            m.profile.as_str(),
            m.declared_by,
        );
    } else {
        println!("  <no umadev.yaml — run `umadev init` (UD-META-001)>");
    }

    // --- workflow state ---
    println!("\n## Workflow state");
    if let Some(s) = read_workflow_state(&project_root) {
        print_state(&s);
    } else {
        println!("  <no .umadev/workflow-state.json — run `umadev run \"<requirement>\"`>");
    }

    // --- evidence chain ---
    println!("\n## Evidence chain");
    let audit = project_root.join(".umadev/audit");
    let api_log = audit.join("frontend-api-calls.jsonl");
    let tool_log = audit.join("tool-calls.jsonl");
    if api_log.is_file() {
        println!(
            "  - UD-EVID-001 frontend-api-calls.jsonl ({} rows)",
            line_count(&api_log)
        );
    }
    if tool_log.is_file() {
        println!(
            "  - UD-EVID-002 tool-calls.jsonl ({} rows)",
            line_count(&tool_log)
        );
    }
    if !api_log.is_file() && !tool_log.is_file() {
        println!("  <no audit logs yet>");
    }

    // --- latest quality gate ---
    println!("\n## Quality gate");
    if let Some((path, passed, total)) = latest_quality_report(&project_root) {
        println!(
            "  {} → {}/100 ({})",
            path.display(),
            total,
            if passed { "PASSED" } else { "FAILED" }
        );
    } else {
        println!("  <no quality report yet — runs at the `quality` phase>");
    }

    // --- proof packs ---
    println!("\n## Proof packs");
    let release = project_root.join("release");
    let mut packs: Vec<_> = std::fs::read_dir(&release)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| {
                    let n = e.file_name();
                    let s = n.to_string_lossy();
                    s.starts_with("proof-pack-") && s.ends_with(".zip")
                })
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    packs.sort();
    if packs.is_empty() {
        println!("  <no proof pack yet — runs at the `delivery` phase>");
    } else {
        for p in packs.iter().rev().take(3) {
            let size = std::fs::metadata(p).map_or(0, |m| m.len());
            println!("  - {} ({} KiB)", p.display(), size / 1024);
        }
    }
    Ok(())
}

fn latest_quality_report(root: &Path) -> Option<(PathBuf, bool, i64)> {
    let dir = root.join("output");
    let mut candidates: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.ends_with("-quality-gate.json"))
        })
        .collect();
    candidates.sort();
    let latest = candidates.last()?.clone();
    let body = std::fs::read_to_string(&latest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let passed = v.get("passed")?.as_bool().unwrap_or(false);
    let total = v.get("total_score")?.as_i64().unwrap_or(0);
    Some((latest, passed, total))
}

fn cmd_report(slug: Option<String>, project_root: Option<PathBuf>) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let slug = match slug {
        Some(s) if !s.is_empty() => s,
        _ => infer_slug(&project_root),
    };

    // 1. Compliance mapping (UD-EVID-004)
    match write_compliance_mapping(&project_root, &slug) {
        Some((path, doc)) => {
            println!("Wrote {} ({} clauses).", path.display(), doc.clauses.len());
            println!("  frameworks: {}", doc.summary.frameworks.join(", "));
        }
        None => {
            println!("(no compliance mapping yet — run `umadev run` first)");
        }
    }

    // 2. Project health summary — tech-debt trend + learned lessons.
    println!("\n--- project health ---");
    let output_dir = project_root.join("output");
    let current_debt = umadev_agent::tech_debt::scan_debt(&output_dir);
    let ledger = umadev_agent::tech_debt::read_ledger(&project_root);
    if !ledger.is_empty() || !current_debt.is_empty() {
        let trend = umadev_agent::tech_debt::diff_against_ledger(&current_debt, &ledger);
        println!(
            "tech-debt: {} open ({} new, {} resolved, net {:+})",
            trend.current_count, trend.new_count, trend.resolved_count, trend.net_change
        );
        let summary = umadev_agent::tech_debt::summarise(&current_debt);
        if summary.total > 0 {
            let kinds: Vec<String> = summary
                .by_kind
                .iter()
                .map(|(k, n)| format!("{k}: {n}"))
                .collect();
            println!("  by kind: {}", kinds.join(", "));
            println!("  severity score: {}", summary.severity_total);
        }
    } else {
        println!("tech-debt: none detected");
    }

    let lessons = umadev_agent::list_sedimented_lessons(&project_root);
    if lessons.is_empty() {
        println!("learned lessons: none yet");
    } else {
        println!("learned lessons: {} sedimented file(s)", lessons.len());
        for p in lessons.iter().take(5) {
            if let Some(name) = p.file_name() {
                println!("  • {}", name.to_string_lossy());
            }
        }
        if lessons.len() > 5 {
            println!("  ... and {} more", lessons.len() - 5);
        }
    }

    // Pitfall knowledge base — the self-learning "踩坑" loop's verdicts.
    let pit = umadev_agent::pitfall_efficacy_summary(&project_root);
    if pit.total > 0 {
        println!(
            "pitfall KB: {} recorded — {} validated (fix proven), {} recurring (fix insufficient), {} active",
            pit.total, pit.validated, pit.recurring, pit.active
        );
        if pit.recurring > 0 {
            println!(
                "  [warn] {} pitfall(s) recurred despite warnings — review their fixes",
                pit.recurring
            );
        }
    }
    Ok(())
}

fn print_state(s: &WorkflowState) {
    println!(
        "workflow-state: phase={} active_gate={} worker={} last_transition_at={}",
        s.phase,
        if s.active_gate.is_empty() {
            "<none>"
        } else {
            &s.active_gate
        },
        if s.backend.is_empty() {
            "offline-templates"
        } else {
            s.backend.as_str()
        },
        s.last_transition_at
    );
    if !s.note.is_empty() {
        println!("note: {}", s.note);
    }
}

fn line_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |t| t.lines().filter(|l| !l.trim().is_empty()).count())
}

fn resolve_root(project_root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = project_root {
        return Ok(root);
    }
    // Default to cwd. We deliberately do NOT honour CLAUDE_PROJECT_DIR /
    // UMADEV_PROJECT_DIR here: when the user runs `umadev` from a
    // directory, that directory IS the workspace they mean. The env vars
    // would override cwd even when the user cd'd elsewhere (e.g. a smoke
    // test in /tmp while CLAUDE_PROJECT_DIR still points at the real repo),
    // which is surprising and wrong for the CLI entry.
    std::env::current_dir().context("could not resolve project root (cwd unreadable)")
}

fn infer_slug(project_root: &std::path::Path) -> String {
    project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BackendArg must cover EXACTLY the ids that umadev-host registers
    /// as drivers. If you add a host driver, you must add the selector
    /// variant too — otherwise `--backend <new>` is unreachable from the
    /// CLI even though `driver_for` builds it.
    #[test]
    fn backend_arg_ids_match_host() {
        let host_ids: std::collections::HashSet<&str> =
            umadev_host::BACKEND_IDS.iter().copied().collect();
        let arg_ids: std::collections::HashSet<&str> = BACKEND_ARG_IDS.iter().copied().collect();
        assert_eq!(
            host_ids,
            arg_ids,
            "BackendArg and umadev-host::BACKEND_IDS drifted.\n\
             only in host: {:?}\n\
             only in arg : {:?}",
            host_ids.difference(&arg_ids).collect::<Vec<_>>(),
            arg_ids.difference(&host_ids).collect::<Vec<_>>(),
        );
    }

    /// Every selector id must resolve through driver_for — i.e. selecting a
    /// backend in the CLI can never 404 at driver-build time.
    #[test]
    fn every_backend_arg_has_a_driver() {
        for id in BACKEND_ARG_IDS {
            assert!(
                umadev_host::driver_for(id).is_some(),
                "BackendArg id `{id}` has no driver in umadev-host::driver_for"
            );
        }
    }

    /// id() / from_id() must be round-trippable for every variant.
    #[test]
    fn backend_arg_id_roundtrip() {
        for id in BACKEND_ARG_IDS {
            let var =
                BackendArg::from_id(id).unwrap_or_else(|| panic!("from_id({id}) returned None"));
            assert_eq!(var.id(), *id, "id()/from_id() not inverse for {id}");
        }
    }

    // ---- intra-phase resume recovery ----

    #[test]
    fn infer_gate_recovers_from_research_interrupt() {
        assert_eq!(infer_gate_from_phase("research"), Some(Gate::ClarifyGate));
    }

    #[test]
    fn infer_gate_recovers_from_docs_interrupt() {
        assert_eq!(infer_gate_from_phase("docs"), Some(Gate::ClarifyGate));
    }

    #[test]
    fn infer_gate_recovers_from_spec_interrupt() {
        assert_eq!(infer_gate_from_phase("spec"), Some(Gate::DocsConfirm));
    }

    #[test]
    fn infer_gate_recovers_from_frontend_interrupt() {
        assert_eq!(infer_gate_from_phase("frontend"), Some(Gate::DocsConfirm));
    }

    #[test]
    fn infer_gate_recovers_from_backend_interrupt() {
        assert_eq!(infer_gate_from_phase("backend"), Some(Gate::PreviewConfirm));
    }

    #[test]
    fn infer_gate_recovers_from_quality_interrupt() {
        assert_eq!(infer_gate_from_phase("quality"), Some(Gate::PreviewConfirm));
    }

    #[test]
    fn infer_gate_returns_none_for_gate_phases() {
        assert_eq!(infer_gate_from_phase("docs_confirm"), None);
        assert_eq!(infer_gate_from_phase("preview_confirm"), None);
    }

    #[test]
    fn infer_gate_returns_none_for_unknown() {
        assert_eq!(infer_gate_from_phase(""), None);
        assert_eq!(infer_gate_from_phase("unknown_phase"), None);
    }

    #[test]
    fn resolve_active_gate_recovers_interrupted_state() {
        // Simulate a workflow state interrupted mid-docs (no active gate).
        let state = umadev_agent::WorkflowState {
            phase: "docs".to_string(),
            active_gate: String::new(),
            slug: "test".to_string(),
            requirement: "test".to_string(),
            last_transition_at: "2026-01-01T00:00:00Z".to_string(),
            note: "Advanced to docs".to_string(),
            backend: String::new(),
            spec_version: "UMADEV_HOST_SPEC_V1".to_string(),
        };
        let gate = resolve_active_gate(&state).expect("should recover");
        assert_eq!(
            gate,
            Gate::ClarifyGate,
            "interrupted docs should re-run from clarify"
        );
    }
}
