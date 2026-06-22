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
//! The rest (`run` / `continue` / `revise` / `spec` / `verify` / `deploy` /
//! `report` / `doctor` / `examples` / `guide` / `rollback` / `history`) are
//! hidden from `--help` but still work for scripts, and mirror TUI slash commands.
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
    /// Adopt an EXISTING repository (brownfield): detect the stack, index the
    /// source, reverse-derive the API contract, and write a lean boundary doc.
    #[command(
        long_about = "Adopt an EXISTING project so UmaDev can work on it INCREMENTALLY\n\
                      (brownfield), instead of scaffolding a brand-new app.\n\
                      \n\
                      `adopt` runs five fail-open steps on the workspace:\n  \
                      1. detect the stack + recover real test/build/lint commands\n  \
                      2. index your source tree into .umadev/project-source-index/\n  \
                      3. reverse-derive an API contract from existing frontend calls\n     \
                         into .umadev/contracts/ (the adopted baseline)\n  \
                      4. write a lean UMADEV.md boundary brief (commands + rules)\n  \
                      5. drop a .umadev/adopt.json baseline marker so later runs\n     \
                         bias toward incremental change, not a rewrite\n\
                      \n\
                      Nothing in your source is modified — adopt only writes the\n\
                      UmaDev artifacts above. Re-running is safe (idempotent).",
        after_help = "EXAMPLES:\n  \
                      umadev adopt                       # adopt the current directory\n  \
                      umadev adopt ./legacy-app          # adopt a specific path\n  \
                      umadev adopt --project-root ./app  # equivalent, explicit flag"
    )]
    Adopt {
        /// Path to the existing repository to adopt. Defaults to the current
        /// directory. (A positional alias for `--project-root`.)
        path: Option<PathBuf>,
        /// Workspace root; defaults to current directory. Overrides `path`
        /// when both are given.
        #[arg(long)]
        project_root: Option<PathBuf>,
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
        /// Trust / autonomy tier: `plan` (research + plan only, read-only),
        /// `guarded` (default — pause at every confirmation gate), or `auto`
        /// (fully autonomous). Irreversible actions (.git / network /
        /// destructive shell) are always confirmed, even in `auto`.
        #[arg(long, default_value = "guarded")]
        mode: String,
        /// Drive the run over ONE continuous base session (the long-session
        /// model — see `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`) instead of a
        /// fresh per-phase base process. Opt-in + gradual-rollout: also enabled
        /// by `UMADEV_CONTINUOUS=1`. Fail-open: if the continuous session can't
        /// start, the run falls back to the existing single-shot path.
        #[arg(long)]
        continuous: bool,
    },
    /// Lightweight fast track for a trivial task (skip the heavy phases).
    #[command(
        hide = true,
        long_about = "Run the LIGHTWEIGHT fast track for a small / trivial task — a\n\
                      one-line tweak, a tiny style nudge, a small script. The full\n\
                      nine-phase pipeline is overkill for that, so `quick` runs a lean\n\
                      single shot: clarify-lite (spec) -> implement -> quality verify.\n\
                      \n\
                      It SKIPS research, the three core documents, both confirmation\n\
                      gates (docs_confirm / preview_confirm), and the delivery\n\
                      proof-pack. Governance still applies on every write, and every\n\
                      phase still leaves an auditable artifact — leaner, not invisible.\n\
                      \n\
                      Use plain `umadev run` for anything non-trivial; the planner also\n\
                      auto-suggests this track when it classifies a requirement as\n\
                      trivial, and you can always override by running the full pipeline.",
        after_help = "EXAMPLES:\n  \
                      umadev quick \"把页头文案改一下\"                     # offline\n  \
                      umadev quick \"tweak the header copy\" --backend claude-code\n  \
                      umadev quick \"rename the Foo button to Bar\" --backend codex"
    )]
    Quick {
        /// Plain-text trivial task, e.g. "改个文案".
        task: String,
        /// Drive an already-logged-in base CLI as the worker. When
        /// omitted, the lean pipeline runs offline with deterministic templates.
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Optional model override (empty by default — never imposed).
        #[arg(long, default_value = "")]
        model: String,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Project slug used in artifact filenames.
        #[arg(long, default_value = "")]
        slug: String,
        /// Trust / autonomy tier: `plan` / `guarded` (default) / `auto`.
        /// See `umadev run --help`. Irreversible actions always confirm.
        #[arg(long, default_value = "guarded")]
        mode: String,
    },
    /// Re-run a single named phase (reuses the prior run's context).
    #[command(
        hide = true,
        long_about = "Re-run ONE named phase of the current run, reusing the prior run's\n\
                      context (requirement / slug / backend from\n\
                      .umadev/workflow-state.json) so the inputs are identical.\n\
                      \n\
                      The headline use is recovering a phase that DEGRADED because the\n\
                      base went offline mid-run — its artifact is an offline\n\
                      placeholder marked with a sibling `.DEGRADED` file. Fix the base,\n\
                      then `umadev redo <phase>` regenerates just that phase and clears\n\
                      its `.DEGRADED` markers on success.\n\
                      \n\
                      Valid phases: research, docs, spec, frontend, backend, quality,\n\
                      delivery (aliases like fe / be / qa / api also work). Unknown\n\
                      names and a missing prior run produce a friendly error.",
        after_help = "EXAMPLES:\n  \
                      umadev redo frontend                       # re-run just frontend\n  \
                      umadev redo backend --backend claude-code  # against a specific base\n  \
                      umadev redo qa                             # alias for quality"
    )]
    Redo {
        /// Phase to re-run (e.g. `frontend`, `backend`, `quality`).
        phase: String,
        /// Drive the re-run via an already-logged-in base CLI. When omitted,
        /// falls back to whatever the original run recorded (or offline).
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
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
    /// Show worker token usage (per run / per phase) + a rough cost estimate.
    #[command(
        long_about = "Show the token usage UmaDev has already recorded for this\n\
                      machine: a per-run, per-phase breakdown of the input+output\n\
                      tokens each base CLI reported, run + grand totals, and a ROUGH\n\
                      blended-rate cost estimate (reference only — UmaDev drives your\n\
                      own base subscription and never bills you).\n\
                      \n\
                      Read-only — reads `~/.umadev/usage.jsonl`, writes nothing. A\n\
                      fresh machine with no runs yet shows a friendly empty state.",
        after_help = "EXAMPLES:\n  \
                      umadev usage             # full per-run / per-phase report\n  \
                      umadev usage | less      # paged"
    )]
    Usage,
    /// Show what UmaDev has learned: high-frequency pitfalls + proven patterns.
    #[command(
        long_about = "Make UmaDev's self-evolution visible. Reads the pitfall knowledge\n\
                      base and prints: high-frequency pitfalls (with how often each\n\
                      recurred and whether its fix is now validated), the failed fixes\n\
                      UmaDev is currently steering AWAY from, and the success patterns\n\
                      that passed the quality gate and are safe to reuse.\n\
                      \n\
                      Read-only — reads `.umadev/learned/`, writes nothing and never\n\
                      changes the learning logic. A project that hasn't hit any\n\
                      pitfalls yet shows a friendly empty state.",
        after_help = "EXAMPLES:\n  \
                      umadev lessons                      # what's been learned here\n  \
                      umadev lessons --project-root ./app"
    )]
    Lessons {
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
    /// Print the `UMADEV_HOST_SPEC_V1` specification.
    #[command(
        hide = true,
        long_about = "Print the UMADEV_HOST_SPEC_V1 specification — the normative\n\
                      contract UmaDev enforces (31 clauses across 4 layers + 9\n\
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
                      latest quality-gate score, and proof-pack zips.\n\
                      \n\
                      With --runtime, additionally PROVE the app runs: boot the\n\
                      detected dev server, wait for it to answer, probe the documented\n\
                      routes over HTTP, and write `.umadev/audit/runtime-proof.json`\n\
                      (folded into the delivery proof-pack). Fail-open — a missing dev\n\
                      server / curl / routes is recorded as 'not verified', never an\n\
                      error.",
        after_help = "EXAMPLES:\n  \
                      umadev verify\n  \
                      umadev verify --runtime\n  \
                      umadev verify --runtime --project-root ./app"
    )]
    Verify {
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Boot the app and prove it actually runs (dev server + route probes).
        #[arg(long)]
        runtime: bool,
    },
    /// Ship the project: detect the deploy target and (optionally) deploy.
    #[command(
        hide = true,
        long_about = "The post-delivery handoff step. Detect the deploy target from\n\
                      the workspace's own files (vercel.json / Next.js → Vercel,\n\
                      netlify.toml → Netlify, fly.toml → Fly, wrangler.* → Cloudflare\n\
                      Pages, Dockerfile → container image, a built dist/out → static\n\
                      host) and print the exact deploy command.\n\
                      \n\
                      Without --run, this only DETECTS and prints the recipe (safe to\n\
                      run anywhere). With --run it actually executes the deploy command\n\
                      against your own logged-in platform CLI, captures the preview URL\n\
                      + log, and writes `.umadev/audit/deploy-proof.json` (folded into\n\
                      the delivery proof-pack). UmaDev owns no credentials and injects\n\
                      nothing — the deploy runs through whatever CLI you've logged in.\n\
                      \n\
                      Fail-open: an unknown platform / missing CLI / failed deploy is\n\
                      recorded as 'not deployed' with a manual hint, never an error.",
        after_help = "EXAMPLES:\n  \
                      umadev deploy                 # detect + print the command\n  \
                      umadev deploy --run           # actually deploy + write proof\n  \
                      umadev deploy --run --command \"npx vercel --prod\""
    )]
    Deploy {
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Actually run the deploy (default is detect-and-print only).
        #[arg(long)]
        run: bool,
        /// Override the deploy command (defaults to the detected platform's).
        #[arg(long)]
        command: Option<String>,
        /// Skip the irreversible-action confirmation prompt (for scripts/CI).
        /// A deploy is a network action the reversibility floor always gates;
        /// `--yes` is the only way to bypass that gate non-interactively.
        #[arg(long)]
        yes: bool,
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
        /// Generate the PR-ready review report (`output/<slug>-review-report.md`)
        /// from this run's evidence, and run the pre-PR security scan first.
        #[arg(long)]
        review: bool,
    },
    /// Open a GitHub PR whose body is the run's review report + proof-pack.
    #[command(
        long_about = "Turn the finished run into the most trustworthy PR on the team.\n\
                      The PR body is UmaDev's own evidence: the PR-ready review\n\
                      report (contract / acceptance / coverage / governance /\n\
                      security / runtime + rollback) followed by a proof-pack\n\
                      summary — a reviewer reads a self-asserting, source-cited\n\
                      case for merge, not a bare diff.\n\
                      \n\
                      Fail-open + safe by design. It first checks readiness (git\n\
                      repo? uncommitted changes? GitHub remote? `gh` installed +\n\
                      logged in?). If anything is missing it prints the exact\n\
                      manual steps and stops — it never crashes and never force-\n\
                      pushes. When ready it commits on a FEATURE branch (creating\n\
                      one first if HEAD is on the default branch — it never commits\n\
                      directly on the default branch), pushes, and `gh pr create`s\n\
                      with the generated body. Without --create it is a dry run:\n\
                      it writes the body + prints the plan, opening nothing.\n\
                      \n\
                      UmaDev owns no credentials — the push + PR run through your\n\
                      own logged-in `git` / `gh`.",
        after_help = "EXAMPLES:\n  \
                      umadev pr                      # dry run: write body + show plan\n  \
                      umadev pr --create             # actually open the PR\n  \
                      umadev pr --create --slug my-app"
    )]
    Pr {
        /// Project slug used in artifact filenames.
        #[arg(long)]
        slug: Option<String>,
        /// Workspace root; defaults to current directory.
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Actually open the PR (default is a dry run: write body + print plan).
        #[arg(long)]
        create: bool,
        /// Skip the irreversible-action confirmation prompt (for scripts/CI).
        /// Pushing + opening a PR are network/VCS actions the reversibility
        /// floor always gates; `--yes` bypasses that gate non-interactively.
        #[arg(long)]
        yes: bool,
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
    let cli = Cli::parse();

    // The TUI owns the alternate screen, so a log line written to the terminal
    // corrupts the display and sticks in the input box. Detect the TUI launch
    // (no subcommand + a real TTY) and route logs to a file in that case;
    // every CLI verb keeps logging to the terminal as before.
    let is_tui = launches_tui(
        cli.command.is_some(),
        std::io::IsTerminal::is_terminal(&std::io::stdin()),
    );
    init_tracing(is_tui);

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
        Command::Adopt { path, project_root } => cmd_adopt(path, project_root),
        Command::Run {
            requirement,
            backend,
            model,
            project_root,
            slug,
            mode,
            continuous,
        } => {
            cmd_run(RunArgs {
                requirement,
                backend,
                model,
                project_root,
                slug,
                mode,
                continuous,
            })
            .await
        }
        Command::Quick {
            task,
            backend,
            model,
            project_root,
            slug,
            mode,
        } => {
            cmd_quick(RunArgs {
                requirement: task,
                backend,
                model,
                project_root,
                slug,
                mode,
                // `quick` is the lean single-shot fast track — never the
                // long-session pipeline path.
                continuous: false,
            })
            .await
        }
        Command::Redo {
            phase,
            backend,
            project_root,
        } => cmd_redo(phase, backend, project_root).await,
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
        Command::Usage => cmd_usage(),
        Command::Lessons { project_root } => cmd_lessons(project_root),
        Command::Spec { clauses } => cmd_spec(clauses),
        Command::Verify {
            project_root,
            runtime,
        } => cmd_verify(project_root, runtime).await,
        Command::Deploy {
            project_root,
            run,
            command,
            yes,
        } => cmd_deploy(project_root, run, command, yes).await,
        Command::Report {
            slug,
            project_root,
            review,
        } => cmd_report(slug, project_root, review),
        Command::Pr {
            slug,
            project_root,
            create,
            yes,
        } => cmd_pr(slug, project_root, create, yes),
        Command::Doctor { project_root } => cmd_doctor(project_root).await,
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

async fn cmd_doctor(project_root: Option<PathBuf>) -> Result<()> {
    let workspace = resolve_root(project_root)?;
    let results = doctor::run_all(&workspace).await;
    print!("{}", doctor::render_report(&workspace, &results));
    if results.iter().any(|r| r.status == doctor::Status::Failed) {
        anyhow::bail!("umadev doctor: one or more checks failed");
    }
    Ok(())
}

/// `true` when this invocation launches the interactive TUI — the only path
/// that owns the alternate screen and must therefore NOT log to the terminal.
/// The TUI launches with no subcommand on a real TTY; a piped/CI run (no TTY)
/// prints help instead, and any subcommand is a plain CLI verb.
fn launches_tui(has_subcommand: bool, stdin_is_tty: bool) -> bool {
    !has_subcommand && stdin_is_tty
}

/// Initialize the tracing subscriber.
///
/// `to_file = true` (the TUI launch) routes every log line to
/// `~/.umadev/logs/umadev.log` instead of the terminal, because the TUI owns
/// the alternate screen and any stray stdout/stderr write corrupts the display
/// and sticks in the input box. If the log file can't be opened we discard logs
/// (`io::sink`) rather than ever fall back to the terminal while the TUI is up.
/// `to_file = false` (every CLI verb) keeps the original terminal logging.
fn init_tracing(to_file: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,umadev=info"));
    if to_file {
        match open_log_file() {
            Some(file) => {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .try_init();
            }
            None => {
                // No writable log file → discard logs entirely. NEVER write to
                // the terminal here: that is the bug we are fixing.
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_target(false)
                    .with_writer(std::io::sink)
                    .try_init();
            }
        }
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init();
    }
}

/// Best-effort open `~/.umadev/logs/umadev.log` for appending. Returns `None`
/// on any failure (no home dir, unwritable path) so the caller discards logs
/// rather than risk corrupting the TUI. Cross-platform home: `HOME` then
/// `USERPROFILE` (Windows).
fn open_log_file() -> Option<std::fs::File> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    open_log_file_in(&PathBuf::from(home))
}

/// Open `<home>/.umadev/logs/umadev.log` for appending, creating the directory
/// tree. Returns `None` on any IO failure. Split out from [`open_log_file`] so
/// it is testable without mutating the process-global `HOME`.
fn open_log_file_in(home: &std::path::Path) -> Option<std::fs::File> {
    let dir = home.join(".umadev").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("umadev.log"))
        .ok()
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

/// `umadev adopt [path]` — onboard an EXISTING repository (brownfield). Detects
/// the stack, indexes the source tree, reverse-derives the API contract,
/// writes a lean boundary doc, and drops the baseline marker. Fail-open: the
/// underlying `run_adopt` never errors, so this prints a summary even on a
/// sparse / empty workspace.
fn cmd_adopt(path: Option<PathBuf>, project_root: Option<PathBuf>) -> Result<()> {
    let lang = cli_lang();
    // `--project-root` wins over the positional `path`; else the positional;
    // else cwd.
    let workspace = resolve_root(project_root.or(path))?;
    let report = umadev_agent::run_adopt(&workspace);

    if report.looks_adopted() {
        println!("{}", umadev_i18n::t(lang, "adopt.title"));
    } else {
        println!("{}", umadev_i18n::t(lang, "adopt.empty"));
    }
    println!(
        "{}",
        umadev_i18n::tf(lang, "adopt.workspace", &[&workspace.display().to_string()])
    );
    println!("{}", umadev_i18n::tf(lang, "adopt.stack", &[&report.stack]));
    if !report.dev_server.is_empty() {
        println!(
            "{}",
            umadev_i18n::tf(lang, "adopt.dev_server", &[&report.dev_server])
        );
    }

    // Commands.
    if report.commands.is_empty() {
        println!("{}", umadev_i18n::t(lang, "adopt.no_commands"));
    } else {
        println!("{}", umadev_i18n::t(lang, "adopt.commands_header"));
        for c in &report.commands {
            println!(
                "{}",
                umadev_i18n::tf(lang, "adopt.command_line", &[&c.name, &c.command])
            );
        }
    }

    // Counts.
    println!(
        "{}",
        umadev_i18n::tf(
            lang,
            "adopt.api_count",
            &[&report.api_endpoints.to_string()]
        )
    );
    println!(
        "{}",
        umadev_i18n::tf(
            lang,
            "adopt.index_count",
            &[
                &report.indexed_files.to_string(),
                &report.indexed_chunks.to_string(),
            ],
        )
    );

    // Artifacts written.
    if !report.artifacts.is_empty() {
        println!("{}", umadev_i18n::t(lang, "adopt.artifacts_header"));
        for a in &report.artifacts {
            println!("{}", umadev_i18n::tf(lang, "adopt.artifact_line", &[a]));
        }
    }

    // Skips / notes (fail-open visibility).
    if !report.notes.is_empty() {
        println!("{}", umadev_i18n::t(lang, "adopt.notes_header"));
        for n in &report.notes {
            println!("{}", umadev_i18n::tf(lang, "adopt.note_line", &[n]));
        }
    }

    println!();
    println!("{}", umadev_i18n::t(lang, "adopt.next_steps"));
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
            "knowledge/design-systems/design-system-deep-dive.md",
            include_str!("../../../knowledge/design-systems/design-system-deep-dive.md"),
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
    /// Trust / autonomy tier string (`plan` / `guarded` / `auto`); parsed into
    /// [`umadev_agent::TrustMode`] at the boundary, fail-open to `guarded`.
    mode: String,
    /// Opt-in to the continuous long-session run path (one base session for the
    /// whole run). OR'd with [`umadev_agent::continuous_enabled_from_env`] at
    /// the boundary; fail-open back to the single-shot path if the session
    /// can't start. `quick` never sets this.
    continuous: bool,
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

/// A fresh UUID-v4 session id for a run, so the driven base reuses ONE
/// continuous session across the pipeline's serial phases (research → docs →
/// spec → frontend → backend) instead of a fresh stateless `--print` process
/// per phase that re-feeds the whole ~90KB context every time. This is the
/// long-session model: the base keeps context (it remembers the PRD when
/// writing code), like driving Claude Code directly, which is what makes it
/// fast. Pure (nanos + per-process counter + pid, avalanched) — no `uuid` dep.
fn new_run_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());
    let counter = u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
    let pid = u128::from(std::process::id());
    let mut x = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (pid << 64);
    x ^= x >> 47;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^= x >> 47;
    let mut u = x.to_be_bytes();
    u[6] = (u[6] & 0x0F) | 0x40; // version 4
    u[8] = (u[8] & 0x3F) | 0x80; // RFC-4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
    )
}

/// Derive the continuous session's autonomy flag from the trust tier: only
/// `auto` lets the base write unattended end-to-end; `guarded` / `plan` keep
/// the human-in-the-loop posture (the gate pauses do the gating, and the
/// per-turn approval floor still denies irreversible actions). Mirrors how the
/// single-shot path reads `mode.gates_auto_approve()`.
fn continuous_autonomous(mode: umadev_agent::TrustMode) -> bool {
    mode.gates_auto_approve()
}

/// Map the start phase of the NEXT continuous block from the gate the prior
/// block paused at — the same gate-anchored block split the single-shot path
/// uses (`run_initial_block` → docs gate → spec block → preview gate → backend
/// block). A clarify gate (only emitted by the single-shot clarify phase) is
/// not produced on the continuous path, so it never appears here.
fn continuous_resume_phase(gate: umadev_agent::Gate) -> umadev_spec::Phase {
    match gate {
        umadev_agent::Gate::DocsConfirm => umadev_spec::Phase::Spec,
        // ClarifyGate is never produced on the continuous path; fail-open to the
        // post-preview tail so a stray value can't wedge.
        umadev_agent::Gate::PreviewConfirm | umadev_agent::Gate::ClarifyGate => {
            umadev_spec::Phase::Backend
        }
    }
}

/// Render a continuous [`umadev_agent::RunOutcome`] as the `RunReport` shape the
/// CLI's [`print_report`] already understands, so the continuous and single-shot
/// paths print identically. The continuous path emits its detailed progress over
/// the live event sink; this report carries only the terminal phase + the gate
/// it paused at (if any), which is all `print_report` needs.
fn continuous_report(outcome: &umadev_agent::RunOutcome) -> RunReport {
    use umadev_agent::RunOutcome;
    match outcome {
        RunOutcome::PausedAtGate(gate) => RunReport {
            final_phase: match gate {
                umadev_agent::Gate::DocsConfirm => umadev_spec::Phase::DocsConfirm,
                umadev_agent::Gate::PreviewConfirm => umadev_spec::Phase::PreviewConfirm,
                umadev_agent::Gate::ClarifyGate => umadev_spec::Phase::Research,
            },
            paused_at: Some(*gate),
            completed: Vec::new(),
        },
        RunOutcome::Completed => RunReport {
            final_phase: umadev_spec::Phase::Delivery,
            paused_at: None,
            completed: Vec::new(),
        },
        // A hard stop did NOT reach delivery and is NOT a gate pause — report the
        // last code-ish phase with no gate so `print_report` prints the honest
        // "stopped before delivery" line rather than "complete".
        RunOutcome::HardStop(_) => RunReport {
            final_phase: umadev_spec::Phase::Quality,
            paused_at: None,
            completed: Vec::new(),
        },
    }
}

/// Drive a continuous run over ONE live [`umadev_runtime::BaseSession`] to its
/// first gate (or, under `auto`, all the way to completion / a hard stop). The
/// `session` is owned here and `end()`-ed once the run settles, so it spans every
/// block of the run with context retained.
///
/// `auto` walks across the gates by resuming the next block at the phase the
/// prior block paused after; `guarded` / `plan` stop at the first gate exactly
/// like the single-shot `run_clarify` path (the user then drives `continue`).
async fn drive_continuous_run(
    runner: &AgentRunner<OfflineRuntime>,
    session: &mut dyn umadev_runtime::BaseSession,
    mode: umadev_agent::TrustMode,
) -> Result<umadev_agent::RunOutcome> {
    use umadev_agent::RunOutcome;
    let mut start_after = umadev_spec::Phase::Research;
    // A finite hop ceiling: research→docs gate→spec→preview gate→backend covers
    // at most three blocks; cap a touch higher so a defensive resume can't loop.
    for _ in 0..6 {
        let outcome = runner
            .run_continuous_block(session, start_after)
            .await
            .context("continuous run block failed")?;
        match outcome {
            RunOutcome::PausedAtGate(gate) if mode.gates_auto_approve() => {
                // Auto tier: approve the gate ourselves and resume the next block.
                eprintln!(
                    "\n{}",
                    umadev_i18n::tlf("continuous.auto_gate_resumed", &[gate.id_str()])
                );
                start_after = continuous_resume_phase(gate);
            }
            other => return Ok(other),
        }
    }
    Ok(RunOutcome::Completed)
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
    let mode = umadev_agent::TrustMode::parse_or_default(&args.mode);
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
        mode,
        // Capture the strict-coverage opt-in ONCE here at the app boundary; the
        // runner reads this snapshot, never the live env (which races in parallel).
        strict_coverage: umadev_agent::strict_coverage_from_env(),
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
        // Long-session model: pin a fresh session id + enable continuation so the
        // base reuses ONE session across this run's serial phases instead of a
        // fresh, context-re-feeding `--print` process per phase. The first call
        // creates the session; later phases resume it (the base remembers the PRD
        // when it writes code). fail-open: a driver that can't pin a session just
        // ignores these (no-op default), keeping the old per-call behavior.
        driver.set_session_id(Some(new_run_session_id()));
        driver.set_continue_session(true);
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

        // ── Continuous long-session path (opt-in, gradual rollout) ──────────
        //
        // When the user passed `--continuous` (or set `UMADEV_CONTINUOUS=1`),
        // drive the WHOLE run over ONE live base session instead of a fresh
        // per-phase base process. The two paths COEXIST and the switch picks
        // between them. **Fail-open:** if the continuous session can't start, we
        // fall through to the single-shot driver path below — the run never dies
        // just because the long-session brain was unreachable.
        let continuous = args.continuous || umadev_agent::continuous_enabled_from_env();
        if continuous {
            match umadev_host::session_for(
                backend.id(),
                &project_root,
                &opts.model,
                continuous_autonomous(mode),
            )
            .await
            {
                Ok(mut session) => {
                    println!(
                        "{}",
                        umadev_i18n::tlf("continuous.session_active", &[backend.id()])
                    );
                    // The runner here is only an options + event-sink carrier for
                    // the continuous driver (which drives the session directly,
                    // not `R::complete`); the offline runtime is never invoked.
                    let runner =
                        AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
                    runner.start().context("failed to start agent")?;
                    let (runner, printer) = attach_live_sink(runner);
                    let outcome = drive_continuous_run(&runner, session.as_mut(), mode).await;
                    // Always end the session (release the process / server),
                    // regardless of how the drive finished.
                    let _ = session.end().await;
                    drop(runner);
                    let _ = printer.await;
                    let outcome = outcome?;
                    print_report(&project_root, &label, &continuous_report(&outcome));
                    return Ok(());
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        umadev_i18n::tlf("continuous.session_unavailable", &[&e.to_string()])
                    );
                    // Fall through to the single-shot path below with `opts` intact.
                }
            }
        }

        let runner = AgentRunner::new(driver, opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        // `auto` tier drives end-to-end headless (no gate pauses); `guarded` /
        // `plan` pause at the first gate exactly as before. Without this, a
        // headless `run --mode auto` stalled at `docs_confirm` waiting on a
        // human who never arrives.
        let report = if mode.gates_auto_approve() {
            runner
                .run_auto_to_completion(true)
                .await
                .context("pipeline failure")?
        } else {
            runner
                .run_clarify(true)
                .await
                .context("clarify phase failure")?
        };
        drop(runner);
        let _ = printer.await;
        (report, label)
    } else {
        let label = "Offline deterministic templates (no AI; demos / CI)".to_string();
        let runner = AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        let report = if mode.gates_auto_approve() {
            runner
                .run_auto_to_completion(false)
                .await
                .context("pipeline failure")?
        } else {
            runner
                .run_clarify(false)
                .await
                .context("clarify phase failure")?
        };
        drop(runner);
        let _ = printer.await;
        (report, label)
    };

    print_report(&project_root, &runtime_label, &report);
    Ok(())
}

/// `umadev quick "<task>"` — the lightweight fast track. Mirrors [`cmd_run`]
/// but drives the lean single-shot [`AgentRunner::run_light`] (spec-lite ->
/// implement -> quality, no gates) instead of the full pipeline.
async fn cmd_quick(args: RunArgs) -> Result<()> {
    if args.requirement.trim().is_empty() {
        anyhow::bail!(
            "empty task — describe the small change, e.g.\n  \
             umadev quick \"把页头文案改一下\""
        );
    }
    let project_root = resolve_root(args.project_root)?;
    let mode = umadev_agent::TrustMode::parse_or_default(&args.mode);
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
        mode,
        // Capture the strict-coverage opt-in ONCE here at the app boundary; the
        // runner reads this snapshot, never the live env (which races in parallel).
        strict_coverage: umadev_agent::strict_coverage_from_env(),
    };

    let (report, runtime_label) = if let Some(backend) = args.backend {
        let mut driver = umadev_host::driver_for(backend.id())
            .ok_or_else(|| anyhow::anyhow!("unknown backend `{}`", backend.id()))?;
        driver.set_workspace(project_root.clone());
        // Long-session model: pin a fresh session id + enable continuation so the
        // base reuses ONE session across this run's serial phases instead of a
        // fresh, context-re-feeding `--print` process per phase. The first call
        // creates the session; later phases resume it (the base remembers the PRD
        // when it writes code). fail-open: a driver that can't pin a session just
        // ignores these (no-op default), keeping the old per-call behavior.
        driver.set_session_id(Some(new_run_session_id()));
        driver.set_continue_session(true);
        match driver.probe().await {
            umadev_host::ProbeResult::Ready { version } => {
                println!("Backend {} ready ({version}).", driver.display_name());
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
            "Base CLI worker — {} ({}) · lightweight track",
            driver.display_name(),
            backend.id()
        );
        let runner = AgentRunner::new(driver, opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        let report = runner.run_light(true).await.context("light run failure")?;
        drop(runner);
        let _ = printer.await;
        (report, label)
    } else {
        let label = "Offline deterministic templates · lightweight track".to_string();
        let runner = AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
        runner.start().context("failed to start agent")?;
        let (runner, printer) = attach_live_sink(runner);
        let report = runner.run_light(false).await.context("light run failure")?;
        drop(runner);
        let _ = printer.await;
        (report, label)
    };

    print_lean_report(
        &project_root,
        &runtime_label,
        "lightweight task complete (spec -> implement -> quality, no gates)",
        &report,
    );
    Ok(())
}

/// `umadev redo <phase>` — re-run a single named phase using the prior run's
/// persisted context. Resolves the backend the same way `continue` does
/// (explicit flag > persisted state > offline). Rejects an unknown phase name
/// and a missing prior run with a friendly message.
async fn cmd_redo(
    phase_name: String,
    backend_override: Option<BackendArg>,
    project_root: Option<PathBuf>,
) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    // Parse the phase name first so a typo fails fast with the valid set.
    let Some(phase) = umadev_agent::phase_from_id(&phase_name) else {
        anyhow::bail!(
            "unknown phase `{phase_name}`. Valid phases: {}",
            umadev_agent::redoable_phase_ids().join(", ")
        );
    };
    // Reuse the prior run's context (requirement / slug / backend) from state.
    let state = match umadev_agent::read_workflow_state_diagnostic(&project_root) {
        umadev_agent::ReadState::Ok(s) => s,
        umadev_agent::ReadState::Missing => anyhow::bail!(
            "no prior run found (.umadev/workflow-state.json missing) — start one \
             with `umadev run` before re-running a single phase."
        ),
        umadev_agent::ReadState::Corrupt { path, error } => anyhow::bail!(
            "workflow-state.json at {} is corrupt ({error}). \
             Run `umadev rollback latest` or delete it, then `umadev run` again.",
            path.display()
        ),
    };

    let slug = if state.slug.is_empty() {
        infer_slug(&project_root)
    } else {
        state.slug.clone()
    };
    let requirement = if state.requirement.is_empty() {
        state
            .note
            .split_once(": ")
            .map_or("(no requirement recorded)", |x| x.1)
            .to_string()
    } else {
        state.requirement.clone()
    };
    // backend: explicit flag > persisted state > offline.
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
        project_root: project_root.clone(),
        requirement,
        slug,
        model: String::new(),
        backend: backend_id.clone().unwrap_or_default(),
        design_system: String::new(),
        seed_template: String::new(),
        // Resume paths honour the existing gate semantics (the user explicitly
        // invoked `continue`); `plan` read-only gating only applies to the
        // initial `run`, so resume defaults to guarded.
        mode: umadev_agent::TrustMode::Guarded,
        // Snapshot the strict-coverage opt-in here at the app boundary (read env
        // once), not live in the runner — a mid-run env read races in parallel.
        strict_coverage: umadev_agent::strict_coverage_from_env(),
    };

    let _ = record_tool_call(
        &project_root,
        "umadev/cli.redo",
        "",
        "redo",
        "UD-FLOW-005",
        &format!("user re-ran phase {}", phase.id()),
        "",
        None,
    );

    let (report, runtime_label) = if let Some(id) = backend_id {
        let backend = BackendArg::from_id(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown backend `{id}` in workflow-state.json"))?;
        let mut driver = umadev_host::driver_for(backend.id())
            .ok_or_else(|| anyhow::anyhow!("no driver registered for `{}`", backend.id()))?;
        driver.set_workspace(project_root.clone());
        // Long-session model: pin a fresh session id + enable continuation so the
        // base reuses ONE session across this run's serial phases instead of a
        // fresh, context-re-feeding `--print` process per phase. The first call
        // creates the session; later phases resume it (the base remembers the PRD
        // when it writes code). fail-open: a driver that can't pin a session just
        // ignores these (no-op default), keeping the old per-call behavior.
        driver.set_session_id(Some(new_run_session_id()));
        driver.set_continue_session(true);
        match driver.probe().await {
            umadev_host::ProbeResult::Ready { version } => {
                println!("Backend {} ready ({version}).", driver.display_name());
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
                     Omit --backend to re-run offline.",
                    backend.id()
                );
            }
            umadev_host::ProbeResult::Unhealthy { detail } => {
                anyhow::bail!("backend `{}` is unhealthy: {detail}", backend.id());
            }
        }
        let label = format!(
            "Base CLI worker — {} ({}) · redo {}",
            driver.display_name(),
            backend.id(),
            phase.id()
        );
        let runner = AgentRunner::new(driver, opts);
        let (runner, printer) = attach_live_sink(runner);
        let report = runner
            .redo_phase(phase, true)
            .await
            .context("redo phase failure")?;
        drop(runner);
        let _ = printer.await;
        (report, label)
    } else {
        let label = format!("Offline deterministic templates · redo {}", phase.id());
        let runner = AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
        let (runner, printer) = attach_live_sink(runner);
        let report = runner
            .redo_phase(phase, false)
            .await
            .context("redo phase failure")?;
        drop(runner);
        let _ = printer.await;
        (report, label)
    };

    print_lean_report(
        &project_root,
        &runtime_label,
        &format!("re-ran the `{}` phase", phase.id()),
        &report,
    );
    Ok(())
}

/// Compact report for the lean entries (`quick` / `redo`). Unlike
/// [`print_report`], these intentionally stop short of `delivery` (Light has no
/// delivery phase; a single-phase redo runs exactly one phase), so the
/// "stopped before delivery (quality gate blocked)" wording would be wrong here.
fn print_lean_report(project_root: &Path, runtime_label: &str, headline: &str, report: &RunReport) {
    println!("UmaDev — {headline}.");
    println!("  workspace: {}", project_root.display());
    println!("  runtime: {runtime_label}");
    println!("  final phase: {}", report.final_phase.id());
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
        // Resume paths default to guarded — see the `continue` path above.
        mode: umadev_agent::TrustMode::Guarded,
        // Snapshot the strict-coverage opt-in here at the app boundary (read env
        // once), not live in the runner — a mid-run env read races in parallel.
        strict_coverage: umadev_agent::strict_coverage_from_env(),
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

/// Resolve + set the process-wide UI language for CLI output, from the saved
/// `~/.umadev/config.toml` (falling back to system-locale detection). Mirrors
/// what the TUI does on launch so `umadev usage` / `umadev lessons` speak the
/// same language as the chat. Returns the resolved language for `t`/`tf`.
fn cli_lang() -> umadev_i18n::Lang {
    let lang = umadev_tui::config::load().resolved_lang();
    umadev_i18n::set_lang(lang);
    lang
}

/// `umadev usage` — print recorded worker token usage per run / per phase, with
/// run + grand totals and a rough advisory cost estimate. Pure read of
/// `~/.umadev/usage.jsonl`; fail-open empty state when nothing is recorded.
fn cmd_usage() -> Result<()> {
    let lang = cli_lang();
    let report = umadev_agent::runner::usage_report();
    if report.is_empty() {
        println!("{}", umadev_i18n::t(lang, "usage.empty"));
        return Ok(());
    }
    let total_calls = report.total_calls.to_string();
    let run_count = report.runs.len().to_string();
    println!(
        "{}",
        umadev_i18n::tf(lang, "usage.title", &[&total_calls, &run_count])
    );
    println!();
    for run in &report.runs {
        let idx = run.index.to_string();
        let backends = if run.backends.is_empty() {
            "offline".to_string()
        } else {
            run.backends.join(", ")
        };
        println!(
            "{}",
            umadev_i18n::tf(lang, "usage.run_header", &[&idx, &backends])
        );
        for p in &run.phases {
            let calls = p.calls.to_string();
            let tokens = p.tokens.to_string();
            println!(
                "{}",
                umadev_i18n::tf(lang, "usage.phase_line", &[&p.phase, &calls, &tokens])
            );
        }
        let rcalls = run.calls.to_string();
        let rtokens = run.tokens.to_string();
        println!(
            "{}",
            umadev_i18n::tf(lang, "usage.run_total", &[&rcalls, &rtokens])
        );
        println!();
    }
    let grand = report.total_tokens.to_string();
    println!("{}", umadev_i18n::tf(lang, "usage.grand_total", &[&grand]));
    let cost = format!(
        "{:.2}",
        umadev_agent::runner::rough_cost_usd(report.total_tokens)
    );
    println!("{}", umadev_i18n::tf(lang, "usage.cost_estimate", &[&cost]));
    println!("{}", umadev_i18n::t(lang, "usage.note_combined"));
    Ok(())
}

/// `umadev lessons` — print what UmaDev has learned in this workspace: high-
/// frequency pitfalls, the failed fixes it now steers away from, and validated
/// success patterns. Pure read of `.umadev/learned/`; never mutates the KB.
fn cmd_lessons(project_root: Option<PathBuf>) -> Result<()> {
    let lang = cli_lang();
    let project_root = resolve_root(project_root)?;
    let report = umadev_agent::lessons::lessons_report(&project_root);
    if report.is_empty() {
        println!("{}", umadev_i18n::t(lang, "lessons.empty"));
        return Ok(());
    }
    println!("{}", umadev_i18n::t(lang, "lessons.title"));
    let e = report.efficacy;
    println!(
        "{}",
        umadev_i18n::tf(
            lang,
            "lessons.efficacy",
            &[
                &e.total.to_string(),
                &e.validated.to_string(),
                &e.recurring.to_string(),
                &e.active.to_string(),
            ],
        )
    );
    println!();

    if !report.top_pitfalls.is_empty() {
        println!("{}", umadev_i18n::t(lang, "lessons.top_header"));
        for p in &report.top_pitfalls {
            let (icon, status_key) = status_chrome(p.status);
            let status = umadev_i18n::t(lang, status_key);
            println!(
                "{}",
                umadev_i18n::tf(
                    lang,
                    "lessons.pitfall_line",
                    &[icon, &p.title, &p.hits.to_string(), status],
                )
            );
            if !p.fix.is_empty() {
                println!(
                    "{}",
                    umadev_i18n::tf(lang, "lessons.pitfall_fix", &[&truncate_cli(&p.fix, 200)])
                );
            }
            if !p.context.is_empty() {
                println!(
                    "{}",
                    umadev_i18n::tf(lang, "lessons.pitfall_ctx", &[&p.context.join(", ")])
                );
            }
        }
        println!();
    }

    // Failed fixes UmaDev is now steering away from (deduped across pitfalls).
    let mut avoid: Vec<String> = Vec::new();
    for p in &report.recurring {
        for f in &p.failed_fixes {
            let f = truncate_cli(f, 160);
            if !avoid.contains(&f) {
                avoid.push(f);
            }
        }
    }
    if !avoid.is_empty() {
        println!("{}", umadev_i18n::t(lang, "lessons.recurring_header"));
        for f in &avoid {
            println!("{}", umadev_i18n::tf(lang, "lessons.avoid_line", &[f]));
        }
        println!();
    }

    if !report.validated_patterns.is_empty() {
        println!("{}", umadev_i18n::t(lang, "lessons.validated_header"));
        for v in &report.validated_patterns {
            println!(
                "{}",
                umadev_i18n::tf(lang, "lessons.validated_line", &[&v.title, &v.summary])
            );
        }
    }
    Ok(())
}

/// Map a pitfall status to its (icon, i18n status-label key) for the lessons view.
fn status_chrome(status: umadev_agent::lessons::PitfallStatus) -> (&'static str, &'static str) {
    use umadev_agent::lessons::PitfallStatus;
    match status {
        PitfallStatus::Validated => ("[ok]", "lessons.status.validated"),
        PitfallStatus::Recurring => ("[warn]", "lessons.status.recurring"),
        PitfallStatus::Active => ("[pitfall]", "lessons.status.active"),
    }
}

/// Truncate a string to `max` chars with an ellipsis (char-safe). Local helper
/// so CLI output stays tidy without pulling the agent crate's private one.
fn truncate_cli(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
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

async fn cmd_verify(project_root: Option<PathBuf>, runtime: bool) -> Result<()> {
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

    // --- pre-PR security scan (UD-SEC-003) ---
    println!("\n## Security scan");
    let scan_path = project_root.join(umadev_agent::security_scan_rel_path());
    match std::fs::read_to_string(&scan_path)
        .ok()
        .and_then(|b| serde_json::from_str::<umadev_agent::SecurityScan>(&b).ok())
    {
        Some(scan) => {
            println!("  {}", scan.summary_line());
            for r in &scan.results {
                println!(
                    "    [{}] {} ({}) — {}",
                    r.status.as_str(),
                    r.tool,
                    r.category,
                    r.detail
                );
            }
        }
        None => println!("  <no security scan yet — runs at the `delivery` phase>"),
    }

    // --- review report (PR-ready checklist) ---
    println!("\n## Review report");
    let review_path = project_root.join(umadev_agent::review_report_rel_path(&infer_slug(
        &project_root,
    )));
    if review_path.is_file() {
        println!("  {} present", review_path.display());
    } else {
        println!("  <no review report yet — `umadev report --review` or the `delivery` phase>");
    }

    // --- deploy proof (post-delivery handoff) ---
    // Read-only: show any recorded deploy + the detected target so the user
    // knows whether the product is live and, if not, how to ship it.
    println!("\n## Deploy");
    let deploy_proof_path = project_root.join(umadev_agent::deploy_proof_rel_path());
    if let Some(p) = std::fs::read_to_string(&deploy_proof_path)
        .ok()
        .and_then(|b| serde_json::from_str::<umadev_agent::DeployProof>(&b).ok())
    {
        println!("  {}", p.summary_line());
    } else {
        let target = umadev_agent::detect_deploy_target(&project_root);
        if let Some(cmd) = target.deploy_command() {
            println!(
                "  <not deployed yet — detected {}: `{}` (run `umadev deploy --run`)>",
                target.label(),
                cmd
            );
        } else {
            println!("  <no deploy target detected — `umadev deploy` to check>");
        }
    }

    // --- runtime proof (UD-EVID-005 runtime evidence) ---
    // Only when --runtime: boot the app, prove it answers, probe its routes,
    // and write `.umadev/audit/runtime-proof.json` (folded into the proof-pack).
    if runtime {
        let lang = umadev_i18n::current();
        println!("\n## {}", umadev_i18n::t(lang, "runtime.header"));
        println!("  {}", umadev_i18n::t(lang, "runtime.running"));
        let proof = umadev_agent::run_runtime_proof(&project_root).await;
        match umadev_agent::write_runtime_proof(&project_root, &proof) {
            Ok(path) => println!(
                "  {}",
                umadev_i18n::tf(lang, "runtime.written", &[&path.display().to_string()])
            ),
            Err(e) => println!(
                "  {}",
                umadev_i18n::tf(lang, "runtime.write_failed", &[&e.to_string()])
            ),
        }
        print_runtime_proof(lang, &proof);
    }

    Ok(())
}

/// Print the human-readable runtime-proof summary (localized). The structured
/// JSON is the authoritative artifact; this is the at-a-glance view.
fn print_runtime_proof(lang: umadev_i18n::Lang, proof: &umadev_agent::RuntimeProof) {
    use umadev_agent::RuntimeStatus;
    if let Some(label) = &proof.dev_server {
        println!(
            "  {}",
            umadev_i18n::tf(lang, "runtime.dev_server", &[label])
        );
    }
    if let Some(url) = &proof.base_url {
        println!("  {}", umadev_i18n::tf(lang, "runtime.base_url", &[url]));
    }
    match &proof.status {
        RuntimeStatus::Verified => {
            let ready = proof
                .ready_ms
                .map_or_else(|| "-".to_string(), |ms| format!("{ms}ms"));
            println!("  {}", umadev_i18n::tf(lang, "runtime.verified", &[&ready]));
            let ok = proof.routes.iter().filter(|r| r.ok).count();
            println!(
                "  {}",
                umadev_i18n::tf(
                    lang,
                    "runtime.routes",
                    &[&ok.to_string(), &proof.routes.len().to_string()]
                )
            );
            for r in &proof.routes {
                let mark = if r.ok { "ok" } else { "!!" };
                println!(
                    "    [{mark}] {} → {} ({}ms)",
                    r.path,
                    if r.status == 0 {
                        umadev_i18n::t(lang, "runtime.no_response").to_string()
                    } else {
                        r.status.to_string()
                    },
                    r.ms
                );
            }
            if let Some(e2e) = &proof.e2e {
                let key = if e2e.passed {
                    "runtime.e2e_passed"
                } else {
                    "runtime.e2e_failed"
                };
                println!(
                    "  {}",
                    umadev_i18n::tf(lang, key, &[&e2e.command, &e2e.ms.to_string()])
                );
            }
        }
        RuntimeStatus::NotVerified(reason) => {
            println!(
                "  {}",
                umadev_i18n::tf(lang, "runtime.not_verified", &[reason])
            );
        }
    }
}

/// `umadev deploy` — the post-delivery handoff. Detects the deploy target and,
/// with `--run`, executes the deploy (fail-open) and writes the deploy-proof
/// that folds into the proof-pack. Without `--run` it only prints the recipe.
async fn cmd_deploy(
    project_root: Option<PathBuf>,
    run: bool,
    command: Option<String>,
    yes: bool,
) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let lang = umadev_i18n::current();
    println!("workspace: {}", project_root.display());

    let target = umadev_agent::detect_deploy_target(&project_root);
    if target == umadev_agent::DeployTarget::None && command.is_none() {
        println!("{}", umadev_i18n::t(lang, "deploy.no_target"));
        return Ok(());
    }
    println!(
        "{}",
        umadev_i18n::tf(lang, "deploy.detected", &[target.label()])
    );
    let recipe = command
        .clone()
        .or_else(|| target.deploy_command())
        .unwrap_or_default();
    if !recipe.is_empty() {
        println!(
            "{}",
            umadev_i18n::tf(lang, "deploy.confirm_preflight", &[&recipe])
        );
    }

    // Detect-and-print only: stop here unless the user explicitly opts in to a
    // real deploy. The deploy is the user's outward-facing action.
    if !run {
        return Ok(());
    }

    // Reversibility floor (fail-SAFE, the inverse of governance fail-open): a
    // deploy reaches the network and ships outward — it is an irreversible
    // action the trust floor escalates to a confirmation REGARDLESS of mode
    // (even `auto` does not get to skip it). We protect the user's project, so
    // when in doubt we confirm. `--yes` is the explicit script/CI bypass; the
    // action is audited either way (UD-CODE-* governance trail).
    let mode = umadev_agent::TrustMode::Auto; // the strictest caller; floor still gates
                                              // Probe the floor on a `git push`-class string: a deploy publishes outward
                                              // (network class), so it must be gated even for a recipe like `npx vercel
                                              // --prod` that the generic classifier wouldn't flag on its own.
    let deploy_probe = format!("git push (deploy) {recipe}");
    if umadev_agent::requires_confirmation(mode, &deploy_probe, "") && !yes {
        let prompt = format!(
            "About to run an IRREVERSIBLE network deploy:\n  {recipe}\nProceed? \
             即将执行不可逆的网络部署,确认继续?"
        );
        if !confirm(&prompt) {
            println!("Deploy cancelled. 已取消部署。");
            let _ = record_tool_call(
                &project_root,
                "umadev/cli.deploy",
                "",
                "block",
                "UD-FLOW-008",
                &format!("user declined irreversible deploy: {recipe}"),
                "",
                None,
            );
            return Ok(());
        }
    }
    // Audit the (now confirmed / --yes) irreversible action before spawning it.
    let _ = record_tool_call(
        &project_root,
        "umadev/cli.deploy",
        "",
        "audit",
        "UD-FLOW-008",
        &format!("running irreversible deploy command: {recipe}"),
        "",
        None,
    );

    println!("{}", umadev_i18n::tf(lang, "deploy.running", &[&recipe]));
    let proof = umadev_agent::run_deploy(&project_root, command.as_deref()).await;
    match &proof.status {
        umadev_agent::DeployStatus::Deployed => {
            let addr = proof
                .url
                .clone()
                .unwrap_or_else(|| umadev_i18n::t(lang, "deploy.done_no_url").to_string());
            println!("{}", umadev_i18n::tf(lang, "deploy.done", &[&addr]));
        }
        umadev_agent::DeployStatus::NotDeployed(reason) => {
            let exit = proof
                .exit_code
                .map_or_else(|| "-".to_string(), |c| c.to_string());
            let login_hint = umadev_i18n::t(lang, "deploy.login_hint");
            println!(
                "{}",
                umadev_i18n::tf(lang, "deploy.failed", &[&exit, reason, login_hint])
            );
        }
    }
    match umadev_agent::write_deploy_proof(&project_root, &proof) {
        Ok(path) => println!(
            "{}",
            umadev_i18n::tf(lang, "deploy.proof_written", &[&path.display().to_string()])
        ),
        Err(e) => println!(
            "{}",
            umadev_i18n::tf(lang, "deploy.exec_failed", &[&recipe, &e.to_string()])
        ),
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

fn cmd_report(slug: Option<String>, project_root: Option<PathBuf>, review: bool) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let slug = match slug {
        Some(s) if !s.is_empty() => s,
        _ => infer_slug(&project_root),
    };
    let lang = umadev_i18n::current();

    // `--review`: run the pre-PR security scan, then assemble + write the
    // PR-ready review report, and surface its verdict. Standalone path so a
    // reviewer can regenerate the report without a full delivery run.
    if review {
        println!("{}", umadev_i18n::t(lang, "review.scanning"));
        let scan = umadev_agent::run_security_scan(&project_root);
        let _ = umadev_agent::write_security_scan(&project_root, &scan);
        println!("  {}", scan.summary_line());
        match umadev_agent::write_review_report(&project_root, &slug) {
            Ok(path) => {
                let report = umadev_agent::build_review_report(&project_root, &slug);
                println!(
                    "{}",
                    umadev_i18n::tf(lang, "review.written", &[&path.display().to_string()])
                );
                for c in &report.claims {
                    let mark = match c.verdict {
                        umadev_agent::Verdict::Pass => "ok",
                        umadev_agent::Verdict::Warn => "!!",
                        umadev_agent::Verdict::Fail => "XX",
                        umadev_agent::Verdict::Info => "i ",
                    };
                    println!("  [{mark}] {}", c.title);
                }
                println!(
                    "{}",
                    if report.mergeable() {
                        umadev_i18n::t(lang, "review.mergeable")
                    } else {
                        umadev_i18n::t(lang, "review.blocked")
                    }
                );
            }
            Err(e) => println!(
                "{}",
                umadev_i18n::tf(lang, "review.write_failed", &[&e.to_string()])
            ),
        }
        return Ok(());
    }

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

/// `umadev pr [--create]` — PR mode. Open a GitHub PR whose body is the run's
/// own evidence (the `review.rs` review report + a proof-pack summary).
///
/// Two safety-first modes:
///   - default (dry run): assess readiness, write the PR body, print the exact
///     branch/commit/PR plan — opens NOTHING.
///   - `--create`: when every readiness check passes, commit on a FEATURE
///     branch (creating one first if HEAD is on the default branch — never
///     committing directly on the default branch), push, and `gh pr create`
///     with the generated body.
///
/// Fail-open throughout: any failing precondition or external-command error
/// prints the manual recipe and returns Ok — never a crash, never a force-push,
/// never a rewrite of the user's existing commits.
fn cmd_pr(
    slug: Option<String>,
    project_root: Option<PathBuf>,
    create: bool,
    yes: bool,
) -> Result<()> {
    let project_root = resolve_root(project_root)?;
    let slug = match slug {
        Some(s) if !s.is_empty() => s,
        _ => infer_slug(&project_root),
    };
    let lang = umadev_i18n::current();
    println!("{}", umadev_i18n::t(lang, "pr.scanning"));

    // 1. Always (re)run the pre-PR security scan so the review report folds in a
    //    fresh verdict, then render + persist the PR body. Fail-open: a write
    //    error is reported, not fatal.
    let scan = umadev_agent::run_security_scan(&project_root);
    let _ = umadev_agent::write_security_scan(&project_root, &scan);
    let body = umadev_agent::render_pr_body(&project_root, &slug);
    let body_rel = umadev_agent::pr_body_rel_path(&slug);
    let body_path = project_root.join(&body_rel);
    if let Some(parent) = body_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&body_path, &body) {
        Ok(()) => println!(
            "{}",
            umadev_i18n::tf(lang, "pr.body_written", &[&body_path.display().to_string()])
        ),
        Err(e) => println!(
            "{}",
            umadev_i18n::tf(lang, "pr.body_write_failed", &[&e.to_string()])
        ),
    }

    // 2. Assess readiness + compute the branch plan (pure).
    let readiness = umadev_agent::assess_readiness(&project_root);
    let plan = umadev_agent::plan_branches(&readiness, &slug);
    println!("\n{}", umadev_i18n::t(lang, "pr.readiness"));
    for c in &readiness.checks {
        let mark = if c.ok { "[x]" } else { "[ ]" };
        println!("  {mark} {}", c.label);
    }
    println!(
        "{}",
        umadev_i18n::tf(
            lang,
            "pr.plan",
            &[
                &plan.head_branch,
                &plan.base_branch,
                if plan.needs_new_branch {
                    umadev_i18n::t(lang, "pr.plan_new_branch")
                } else {
                    umadev_i18n::t(lang, "pr.plan_reuse_branch")
                }
            ]
        )
    );

    // 3. Not ready, or a dry run → print manual steps / the plan and stop. We
    //    NEVER touch the repo unless the user explicitly opts in with --create
    //    AND every precondition holds.
    if !readiness.ready() {
        println!(
            "\n{}",
            umadev_agent::manual_steps(&readiness, &slug, &body_rel)
        );
        return Ok(());
    }
    if !create {
        println!("\n{}", umadev_i18n::t(lang, "pr.dry_run"));
        return Ok(());
    }

    // Reversibility floor (fail-SAFE): a `git push` + `gh pr create` reach the
    // network and publish work outward — irreversible actions the trust floor
    // escalates to a confirmation REGARDLESS of mode (auto cannot skip it). We
    // protect the user's repo, so in doubt we confirm. `--yes` is the explicit
    // script/CI bypass; the push + PR-create are audited regardless.
    let push_cmd = format!("git push -u origin {}", plan.head_branch);
    let mode = umadev_agent::TrustMode::Auto; // strictest caller; floor still gates
    if umadev_agent::requires_confirmation(mode, &push_cmd, "") && !yes {
        let prompt = format!(
            "About to push `{}` and open a PR (IRREVERSIBLE network action). Proceed? \
             即将推送分支并开 PR(不可逆网络动作),确认继续?",
            plan.head_branch
        );
        if !confirm(&prompt) {
            println!("PR cancelled. 已取消开 PR。");
            let _ = record_tool_call(
                &project_root,
                "umadev/cli.pr",
                "",
                "block",
                "UD-FLOW-008",
                &format!(
                    "user declined irreversible push/PR for branch {}",
                    plan.head_branch
                ),
                "",
                None,
            );
            return Ok(());
        }
    }

    // 4. Ready + --create → drive git + gh. Each step is fail-open: on the first
    //    error we stop and fall back to the manual recipe, leaving the repo as
    //    we found it (no force, no history rewrite).
    if plan.needs_new_branch {
        // SAFETY: HEAD is on the default branch — branch first, never commit on
        // it directly. `git switch -c` is non-destructive (creates + checks out).
        println!(
            "{}",
            umadev_i18n::tf(lang, "pr.branching", &[&plan.head_branch])
        );
        if !run_pr_git(&project_root, &["switch", "-c", &plan.head_branch]) {
            return pr_fallback(&readiness, &slug, &body_rel, lang);
        }
    }
    println!("{}", umadev_i18n::t(lang, "pr.committing"));
    if !run_pr_git(&project_root, &["add", "-A"]) {
        return pr_fallback(&readiness, &slug, &body_rel, lang);
    }
    let commit_msg = format!("{slug}: UmaDev pipeline output");
    if !run_pr_git(&project_root, &["commit", "-m", &commit_msg]) {
        // A failed commit usually means "nothing staged" — non-fatal, but we
        // can't open a PR with no commit, so fall back to manual.
        return pr_fallback(&readiness, &slug, &body_rel, lang);
    }
    // Plain `push -u` (NEVER `--force`): if the remote rejects it we surface the
    // error and stop rather than overwriting anything.
    println!(
        "{}",
        umadev_i18n::tf(lang, "pr.pushing", &[&plan.head_branch])
    );
    // Audit the irreversible network push before it runs.
    let _ = record_tool_call(
        &project_root,
        "umadev/cli.pr.push",
        "",
        "audit",
        "UD-FLOW-008",
        &push_cmd,
        "",
        None,
    );
    if !run_pr_git(&project_root, &["push", "-u", "origin", &plan.head_branch]) {
        return pr_fallback(&readiness, &slug, &body_rel, lang);
    }

    // 5. Open the PR with the generated body. `gh` reads our own login.
    println!("{}", umadev_i18n::t(lang, "pr.opening"));
    // Audit the irreversible PR-create before it runs.
    let _ = record_tool_call(
        &project_root,
        "umadev/cli.pr.create",
        "",
        "audit",
        "UD-FLOW-008",
        &format!(
            "gh pr create --base {} --head {}",
            plan.base_branch, plan.head_branch
        ),
        "",
        None,
    );
    let body_arg = body_path.to_string_lossy().to_string();
    let gh_out = std::process::Command::new("gh")
        .args([
            "pr",
            "create",
            "--base",
            &plan.base_branch,
            "--head",
            &plan.head_branch,
            "--title",
            &slug,
            "--body-file",
            &body_arg,
        ])
        .current_dir(&project_root)
        .output();
    match gh_out {
        Ok(out) if out.status.success() => {
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("{}", umadev_i18n::tf(lang, "pr.opened", &[&url]));
            println!("{}", umadev_i18n::t(lang, "pr.review_loop"));
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            println!("{}", umadev_i18n::tf(lang, "pr.create_failed", &[&err]));
            return pr_fallback(&readiness, &slug, &body_rel, lang);
        }
        Err(e) => {
            println!(
                "{}",
                umadev_i18n::tf(lang, "pr.create_failed", &[&e.to_string()])
            );
            return pr_fallback(&readiness, &slug, &body_rel, lang);
        }
    }
    Ok(())
}

/// Run a git subcommand in `project_root` for the PR flow, printing a fail-open
/// notice on error. Returns `true` on success. Never panics, never `--force`.
fn run_pr_git(project_root: &Path, args: &[&str]) -> bool {
    match std::process::Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let lang = umadev_i18n::current();
            println!(
                "{}",
                umadev_i18n::tf(lang, "pr.git_failed", &[&args.join(" "), err.trim()])
            );
            false
        }
        Err(e) => {
            let lang = umadev_i18n::current();
            println!(
                "{}",
                umadev_i18n::tf(lang, "pr.git_failed", &[&args.join(" "), &e.to_string()])
            );
            false
        }
    }
}

/// Common fail-open exit for the `--create` path: a step failed, so print the
/// manual recipe (which is safe + force-free) and return Ok. The repo is left
/// as we found it; we never retry destructively.
fn pr_fallback(
    readiness: &umadev_agent::PrReadiness,
    slug: &str,
    body_rel: &str,
    lang: umadev_i18n::Lang,
) -> Result<()> {
    println!("\n{}", umadev_i18n::t(lang, "pr.fallback"));
    println!("{}", umadev_agent::manual_steps(readiness, slug, body_rel));
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

    #[test]
    fn launches_tui_only_on_no_subcommand_and_a_tty() {
        // The only path that owns the alternate screen → must log to a file.
        assert!(launches_tui(false, true), "no subcommand + TTY → TUI");
        // A subcommand is a plain CLI verb (logs to terminal as before).
        assert!(!launches_tui(true, true), "a subcommand is not the TUI");
        // No TTY (piped / CI) prints help, never the TUI.
        assert!(!launches_tui(false, false), "no TTY → not the TUI");
        assert!(
            !launches_tui(true, false),
            "subcommand + no TTY → not the TUI"
        );
    }

    #[test]
    fn open_log_file_in_creates_the_tree_and_never_panics() {
        // A writable home → ~/.umadev/logs/umadev.log is created. No global env
        // mutation, so this can't flake other tests that read HOME.
        let tmp = std::env::temp_dir().join(format!("umadev-logtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let f = open_log_file_in(&tmp);
        assert!(f.is_some(), "a writable home yields a log file");
        assert!(
            tmp.join(".umadev")
                .join("logs")
                .join("umadev.log")
                .is_file(),
            "the log file is created on disk"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

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

    // ---- continuous long-session run path wiring ----

    /// Only `auto` makes the continuous session autonomous; `guarded` / `plan`
    /// keep the human-in-the-loop posture (gate pauses + the approval floor).
    #[test]
    fn continuous_autonomous_only_for_auto() {
        assert!(continuous_autonomous(umadev_agent::TrustMode::Auto));
        assert!(!continuous_autonomous(umadev_agent::TrustMode::Guarded));
        assert!(!continuous_autonomous(umadev_agent::TrustMode::Plan));
    }

    /// The next continuous block resumes at the gate-anchored start phase — the
    /// same block split the single-shot path uses.
    #[test]
    fn continuous_resume_phase_is_gate_anchored() {
        assert_eq!(
            continuous_resume_phase(Gate::DocsConfirm),
            umadev_spec::Phase::Spec
        );
        assert_eq!(
            continuous_resume_phase(Gate::PreviewConfirm),
            umadev_spec::Phase::Backend
        );
    }

    /// The continuous outcome → `RunReport` mapping `print_report` consumes:
    /// a gate pause carries the gate, completion reaches delivery with no gate,
    /// and a hard stop reports a pre-delivery phase with no gate (so it never
    /// prints "complete").
    #[test]
    fn continuous_report_maps_each_outcome() {
        use umadev_agent::RunOutcome;
        let paused = continuous_report(&RunOutcome::PausedAtGate(Gate::DocsConfirm));
        assert_eq!(paused.paused_at, Some(Gate::DocsConfirm));

        let done = continuous_report(&RunOutcome::Completed);
        assert_eq!(done.paused_at, None);
        assert_eq!(done.final_phase, umadev_spec::Phase::Delivery);

        let stopped = continuous_report(&RunOutcome::HardStop("no code".into()));
        assert_eq!(stopped.paused_at, None);
        assert_ne!(
            stopped.final_phase,
            umadev_spec::Phase::Delivery,
            "a hard stop must NOT report as a completed delivery"
        );
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
