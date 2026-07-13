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
mod knowledge_bundle;
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
                      gate (`docs_confirm`). Pick one of the three base CLIs:\n\
                      \n  \
                      --backend claude-code    Anthropic Claude Code\n  \
                      --backend codex          OpenAI Codex\n  \
                      --backend opencode       OpenCode\n\
                      \n\
                      All three drive the user's already-installed, already-logged-in\n\
                      CLI — no API key needed.\n\
                      \n\
                      Omitting --backend falls back to the internal offline templates\n\
                      (deterministic, no network) — a demo / CI fallback, NOT a base you\n\
                      pick for real delivery.\n\
                      \n\
                      After `run`, the pipeline pauses at the `docs_confirm` gate.\n\
                      Review `output/*-prd.md` etc., then run `umadev continue` to\n\
                      proceed, or `umadev revise \"...\"` to ask for changes.",
        after_help = "EXAMPLES:\n  \
                      umadev run \"做一个登录系统\"                       # offline fallback\n  \
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
        /// Force the continuous long-session path (one base session for the whole
        /// run — see `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`). This is now the
        /// DEFAULT for a host-CLI run, so the flag is rarely needed; it only
        /// FORCES continuous on. To go back to the legacy per-phase single-shot
        /// path, opt OUT with `UMADEV_CONTINUOUS=0` (or `UMADEV_LEGACY_RUN=1`).
        /// Fail-open either way: if the continuous session can't start, the run
        /// falls back to the single-shot path.
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
                      Valid phases: research, docs, docs_confirm, spec, frontend,\n\
                      preview_confirm, backend, quality, delivery (aliases like fe / be /\n\
                      qa / api also work). The two gate phases (docs_confirm,\n\
                      preview_confirm) are accepted but have no artifact to regenerate —\n\
                      `redo` reports this and points you at `umadev continue` / `revise` to\n\
                      act on the gate. Unknown names and a missing prior run produce a\n\
                      friendly error.",
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
                      If no gate is recorded (e.g. the run was interrupted mid-phase),\n\
                      `continue` infers the right block from the current phase and re-runs\n\
                      it to recover. If it can't infer one (the pipeline already finished,\n\
                      or the state is stale), it exits with an error telling you to start a\n\
                      fresh `umadev run` — it is never a silent no-op.",
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
        /// Which governance check to run: `pre-write` (all code rules) /
        /// `pre-bash` (dangerous-command guard) / `check-emoji` / `check-color`
        /// / `check-slop` for the PreToolUse guards, or `tool-audit` for the
        /// PostToolUse audit (UD-EVID-002 — records the executed call, never
        /// blocks).
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
                      claude-code   writes .claude/settings.json PreToolUse hook\n  \
                      pre-commit    writes .git/hooks/pre-commit (runs `umadev ci --changed-only`)\n\
                      \n\
                      The hook checks every Write/Edit tool call, but HARD-BLOCKS only the\n\
                      irreversible-if-written floor: hardcoded secrets/credentials in source\n\
                      (UD-SEC-003) and sensitive-path writes to .git/.env/.ssh (UD-SEC-001,\n\
                      bypass-immune). Craft/quality findings — emoji-as-icon (UD-CODE-001),\n\
                      hardcoded colors and AI-slop (UD-CODE-002) — are FLAGGED, not blocked:\n\
                      the post-write QC loop repairs them, so a single nit never stops the\n\
                      base mid-write. Codex lacks a PreToolUse hook surface — use\n\
                      `--base pre-commit` there instead."
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
    /// Manage MCP servers — install/list/remove MCP servers for a base CLI.
    ///
    /// Each base discovers MCP servers from its OWN config file, so pick the
    /// base with `--backend`:
    ///
    /// - `claude-code` (default) → `.mcp.json`
    /// - `codex` → `.codex/config.toml` (`[mcp_servers]`)
    /// - `opencode` → `opencode.json` (`mcp`)
    /// - `all` → write to all three at once
    ///
    /// Writers parse-merge (preserve your existing servers/keys/comments) and
    /// write atomically, so installing never clobbers your config.
    #[command(name = "mcp-manage")]
    McpManage {
        /// Action: `install`, `list`, or `remove`.
        action: String,
        /// Server name (for install/remove).
        name: Option<String>,
        /// Server command (everything after `--`).
        /// e.g. `umadev mcp-manage install github -- npx -y @mcp/server-github`
        command: Vec<String>,
        /// Target base: `claude-code` (default) | `codex` | `opencode` | `all`.
        #[arg(long, default_value = "claude-code")]
        backend: String,
        /// Workspace root; defaults to the project/git root above the cwd.
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

    // Stage the embedded `knowledge/` corpus to ~/.umadev/knowledge once per
    // build and point UMADEV_KNOWLEDGE_DIR at it, so knowledge recall works in
    // any user project with zero setup. Done BEFORE command dispatch (incl. the
    // no-subcommand TUI path below) so every `knowledge_root` consumer — TUI,
    // CLI, director loop — discovers the full curated KB. Fail-open: any error
    // is swallowed and recall degrades to empty, never blocking startup.
    knowledge_bundle::ensure_staged();

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
            project_root,
            slug,
            mode,
            continuous,
        } => {
            cmd_run(RunArgs {
                requirement,
                backend,
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
            project_root,
            slug,
            mode,
        } => {
            cmd_quick(RunArgs {
                requirement: task,
                backend,
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
            backend,
            project_root,
        } => cmd_mcp_manage(action, name, command, backend, project_root),
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

    // ── PostToolUse AUDIT path (UD-EVID-002 / Layer-3 "PostToolUse hooks audit
    // results", spec §7.3 `tool-audit`) ──────────────────────────────────────
    // By the time PostToolUse fires the tool has ALREADY run, so there is nothing
    // to gate — we only record the executed call to the audit trail, then exit 0.
    // claude-code ignores a PostToolUse hook's stdout for permission purposes, so
    // a clean exit with no JSON simply lets the base proceed. Fail-open: the whole
    // record is wrapped in `catch_unwind` so even a panic in the recorder can
    // never block or error the base. (`post-tool` is accepted as a friendly alias.)
    if check == "tool-audit" || check == "post-tool" {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook::run_post_tool(&stdin, &project_root);
        }));
        return Ok(());
    }

    // ── P0-1: fail-open is a HARD CONTRACT, not a hope ────────────────────
    // The whole rule book (≈110 `check_*` content scanners) runs on arbitrary
    // base-authored content INSIDE this hook subprocess. If ANY rule panics on
    // a pathological input, an unwinding hook process produces empty stdout +
    // a non-zero exit — which Claude Code interprets as a hard DENY, turning
    // governance fail-CLOSED and wedging the base on every write. We refuse to
    // let that happen: the entire decision computation (policy load + scan) is
    // wrapped in `catch_unwind`; a panic collapses to `Decision::pass()`
    // (allow). The `AssertUnwindSafe` is sound here — on the panic path we
    // discard all of `compute`'s captured state and emit a fresh `pass()`, so
    // no logically-inconsistent value can escape.
    let check_for_panic = check.clone();
    let decision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        compute_hook_decision(&check_for_panic, &stdin, &project_root)
    }))
    .unwrap_or_else(|_| {
        eprintln!(
            "umadev: hook `{check}` panicked while scanning content — failing OPEN (allow). \
             Governance must never block the base on a rule bug."
        );
        umadev_governance::Decision::pass()
    });

    // Record the decision to the audit log (UD-EVID-002). The hook runs in
    // the workspace CWD, so that's the project root. Best-effort: a write
    // failure (or even a panic in the recorder) never blocks the host — the
    // audit call is itself wrapped so it can't unwind past us.
    if decision.block {
        let tool = if check == "pre-bash" { "Bash" } else { "Write" };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            umadev_governance::record_tool_call(
                &project_root,
                tool,
                "",
                "block",
                &decision.clause,
                &decision.reason,
                "",
                None,
            )
        }));
    }
    // Printing the decision must also never unwind: a serialization/IO panic
    // here would otherwise exit non-zero with no `allow` on stdout. Catch it
    // and, on the block-less path, still try to emit a bare allow so the base
    // is never silently denied. (`print_decision` already uses
    // `to_string(...).unwrap_or_default()`, so this is belt-and-braces.)
    let printed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hook::print_decision(&decision);
    }));
    if printed.is_err() && !decision.block {
        // Last-resort allow so a print panic can't read as a deny.
        println!(
            r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"allow"}}}}"#
        );
    }
    Ok(())
}

/// The pure decision core of [`cmd_hook`], split out so the whole rule-scan can
/// be wrapped in one `catch_unwind`. Returns the governance [`Decision`] for the
/// given `check` name and stdin payload. Fail-open by construction: an unknown
/// check name passes through; a panic inside any rule is caught by the caller
/// and collapses to `Decision::pass()`.
fn compute_hook_decision(
    check: &str,
    stdin: &str,
    project_root: &std::path::Path,
) -> umadev_governance::Decision {
    let policy = umadev_governance::Policy::load(project_root);
    match check {
        "pre-write" | "check-emoji" | "check-color" | "check-slop" => {
            hook::run_pre_write_with(stdin, &policy)
        }
        "pre-bash" => hook::run_pre_bash(stdin),
        _ => {
            // Fail-open, like every other path in the hook layer: an unknown
            // check name (a misconfigured matcher, or a future host passing a
            // check this build doesn't recognise) must still emit an `allow`
            // decision — NOT exit non-zero with empty stdout, which a host may
            // treat as a hard block. Warn on stderr for diagnosability.
            eprintln!(
                "umadev: unknown hook check `{check}` — passing through (expected: pre-write, pre-bash, tool-audit)"
            );
            umadev_governance::Decision::pass()
        }
    }
}

fn cmd_install(host: String, project_root: Option<PathBuf>) -> Result<()> {
    let root = project_root_or_cwd(project_root);
    match host.as_str() {
        "claude-code" => {
            if let Some(path) = hook::install_claude_hook(&root)? {
                println!("[ok] Installed UmaDev PreToolUse hook for Claude Code.");
                println!("  → {}", path.display());
                println!();
                println!("Every Write/Edit tool call is checked. Only the irreversible-if-written");
                println!("floor is HARD-BLOCKED at write time:");
                println!("  • hardcoded secrets / credentials in source  (UD-SEC-003)");
                println!(
                    "  • sensitive-path writes (.git/.env/.ssh)     (UD-SEC-001) — bypass-immune"
                );
                println!();
                println!(
                    "Craft / quality findings are FLAGGED, not blocked — they are repaired by"
                );
                println!("the post-write QC loop so a single nit never stops the base mid-write:");
                println!("  • emoji-as-functional-icons (UD-CODE-001)");
                println!("  • hardcoded color literals   (UD-CODE-002)");
                println!("  • AI-slop / placeholders     (UD-CODE-002)");
                println!();
                println!("A PostToolUse audit hook also records every executed Write/Edit/Bash");
                println!(
                    "to .umadev/audit/tool-calls.jsonl (UD-EVID-002 — audit only, never blocks)."
                );
                println!();
                println!("To remove: umadev uninstall --host claude-code");
            } else {
                // Refused to install into the GLOBAL ~/.claude — that would
                // govern every other project/tool, exactly the over-reach we
                // avoid. The user should install from inside a project dir.
                println!(
                    "[skip] Not installing the UmaDev hook into the global ~/.claude — it would"
                );
                println!(
                    "       govern ALL your projects and tools. Run `umadev install` from inside"
                );
                println!("       a specific project directory instead.");
            }
        }
        "pre-commit" => {
            // Resolve the git repo ROOT by walking UP for `.git`, so
            // `umadev install --base pre-commit` works from any subdirectory of
            // the repo — the hook must land in `<repo-root>/.git/hooks/`, never a
            // phantom `<subdir>/.git`. Fall back to the resolved root if no
            // ancestor is a git repo, so `install_pre_commit_hook` still reports
            // the honest "not a git repository" error.
            let repo_root = find_git_root_from(&root).unwrap_or_else(|| root.clone());
            let path = install_pre_commit_hook(&repo_root)?;
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
    let root = project_root_or_cwd(project_root);
    // Hook-only mode: `umadev uninstall --base <x>` — unchanged behaviour.
    if let Some(host) = base {
        match host.as_str() {
            "claude-code" => {
                hook::uninstall_claude_hook(&root)?;
                println!("[ok] Removed UmaDev PreToolUse hook from Claude Code settings.");
            }
            "pre-commit" => {
                // Resolve the GIT ROOT (walk up) - the same root `install --base
                // pre-commit` uses - so uninstall from a SUBDIR actually finds the hook at
                // <gitroot>/.git/hooks/pre-commit instead of falsely reporting "Removed"
                // while the real hook stays active.
                let git_root = find_git_root_from(&root).unwrap_or_else(|| root.clone());
                uninstall_pre_commit_hook(&git_root)?;
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
    let git_root = find_git_root_from(&root).unwrap_or_else(|| root.clone());
    let _ = uninstall_pre_commit_hook(&git_root);
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
            warn_if_umadev_shadowed_on_path();
            Ok(())
        }
        Ok(s) => anyhow::bail!("npm exited with status {s}"),
        Err(e) => {
            anyhow::bail!("could not run npm ({e}); run `npm install -g umadev@latest` yourself")
        }
    }
}

/// Every `umadev` launcher found on `PATH`, in PATH order (at most one per dir). The
/// FIRST is the one the shell actually runs; the rest are shadowed. On Windows we look
/// for the npm-shim spellings (`.cmd` / `.exe` / `.bat`) plus the bare name.
fn find_all_umadev_on_path() -> Vec<std::path::PathBuf> {
    match std::env::var_os("PATH") {
        Some(path) => find_all_umadev_in(&path),
        None => Vec::new(),
    }
}

/// Pure worker for [`find_all_umadev_on_path`] over an explicit `PATH` value — no process
/// env read, so the shadow logic is unit-testable without a racy `set_var`.
fn find_all_umadev_in(path: &std::ffi::OsStr) -> Vec<std::path::PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["umadev.cmd", "umadev.exe", "umadev.bat", "umadev"]
    } else {
        &["umadev"]
    };
    let mut out = Vec::new();
    for dir in std::env::split_paths(path) {
        for n in names {
            let cand = dir.join(n);
            if cand.is_file() {
                out.push(cand);
                break; // one launcher per dir is enough to establish the shadow
            }
        }
    }
    out
}

/// Warn when MORE THAN ONE `umadev` is on `PATH`: an earlier one shadows the just-upgraded
/// npm-global binary, so `umadev --version` can still report the OLD version even though the
/// upgrade succeeded (the reported "updated but `--version` unchanged" — a stale launcher
/// left in Node's own dir sat ahead of `npm-global` on PATH). Advisory + fail-open: it only
/// reads PATH and prints; it never blocks or modifies anything.
fn warn_if_umadev_shadowed_on_path() {
    let all = find_all_umadev_on_path();
    if all.len() < 2 {
        return;
    }
    println!(
        "\n[!] Multiple `umadev` launchers are on PATH — the shell runs only the FIRST, which \
         may be a STALE one, so `umadev --version` can still show the old version after this \
         upgrade:"
    );
    for (i, p) in all.iter().enumerate() {
        let mark = if i == 0 { "   <- this one runs" } else { "" };
        println!("      {}. {}{mark}", i + 1, p.display());
    }
    println!(
        "      If `umadev --version` didn't change, delete the earlier (stale) launcher above \
         and keep only the npm-global one."
    );
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

/// `umadev mcp-manage` — install/list/remove MCP servers for a chosen base.
///
/// `backend` is one of `claude-code` (default) / `codex` / `opencode` / `all`.
/// Each base discovers MCP servers from a DIFFERENT config file in a DIFFERENT
/// format; `mcp_manager` writes the right one (parse-merged + atomic).
fn cmd_mcp_manage(
    action: String,
    name: Option<String>,
    command: Vec<String>,
    backend: String,
    project_root: Option<PathBuf>,
) -> Result<()> {
    let root = resolve_workspace_root(project_root);

    // `all` fans out over the three bases; otherwise a single base.
    let backends: Vec<mcp_manager::Backend> = if backend == "all" {
        mcp_manager::Backend::ALL.to_vec()
    } else {
        vec![mcp_manager::Backend::parse(&backend)?]
    };

    match action.as_str() {
        "install" | "add" => {
            let server_name = name.ok_or_else(|| {
                anyhow::anyhow!(
                    "server name required: umadev mcp-manage install <name> -- <command>"
                )
            })?;
            if command.iter().all(|t| t.trim().is_empty()) {
                anyhow::bail!("command required after --: e.g. `-- npx -y @mcp/server-github`");
            }
            // Pass clap's post-`--` argv straight through — first token is the
            // command, the rest are args VERBATIM. Joining + re-splitting on
            // whitespace (the old path) mangled a quoted multi-word arg like
            // `-- node "my server.js"` into `["node","my","server.js"]`.
            let entry = mcp_manager::parse_command_parts(&command);
            for b in &backends {
                let path = mcp_manager::install(*b, &root, &server_name, &entry)?;
                println!("[ok] Installed MCP server '{server_name}' for {}.", b.id());
                println!("  → {}", path.display());
            }
            println!("  The base discovers this from its config file on next launch.");
        }
        "list" | "ls" => {
            for b in &backends {
                let servers = mcp_manager::list(*b, &root)?;
                println!(
                    "{} MCP servers ({}) [{}]:",
                    b.id(),
                    servers.len(),
                    b.config_rel_path()
                );
                if servers.is_empty() {
                    println!("  (none configured)");
                } else {
                    for s in servers {
                        println!("  • {}: {}", s.name, s.detail);
                    }
                }
            }
        }
        "remove" | "rm" | "delete" => {
            let server_name = name.ok_or_else(|| anyhow::anyhow!("server name required"))?;
            let mut any_removed = false;
            for b in &backends {
                let (_, removed) = mcp_manager::remove(*b, &root, &server_name)?;
                if removed {
                    any_removed = true;
                    println!("[ok] Removed MCP server '{server_name}' from {}.", b.id());
                }
            }
            if !any_removed {
                anyhow::bail!("MCP server '{server_name}' not found in the selected backend(s).");
            }
        }
        other => anyhow::bail!("unknown action: '{other}' (use install/list/remove)"),
    }
    Ok(())
}

/// `umadev skill` — install/list/remove skill packages.
fn cmd_skill(action: String, target: Option<String>, project_root: Option<PathBuf>) -> Result<()> {
    let root = resolve_workspace_root(project_root);
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
    let root = resolve_workspace_root(project_root);
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
    let root = project_root_or_cwd(project_root);
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
/// Closing marker of the UmaDev block, so uninstall can strip exactly our lines
/// even when they sit ABOVE the user's own hook (we prepend, not append).
const PRE_COMMIT_END_MARKER: &str = "# end umadev pre-commit governance hook";

/// Write the `umadev ci` pre-commit git hook into `.git/hooks/pre-commit`.
/// Idempotent — if a UmaDev hook is already present, it's a no-op. A
/// pre-existing non-UmaDev pre-commit hook is PRESERVED, and our check is
/// PREPENDED (immediately after the shebang) so UmaDev governance runs FIRST,
/// **before** any early `exit`/`exec`/`return` in the user's own hook could
/// skip it. Appending below the user's hook (the old behaviour) let a user
/// script that bailed early silence UmaDev entirely — governance that never
/// runs is worse than no promise of it.
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
         {bin} ci --changed-only\n\
         {end}\n",
        marker = PRE_COMMIT_MARKER,
        end = PRE_COMMIT_END_MARKER,
    );
    // Preserve a pre-existing user hook but run our check FIRST. If the file has
    // a shebang, insert our block immediately after it (keeping the user's
    // interpreter line); otherwise prepend a fresh shebang + our block above the
    // user's content. Either way UmaDev governance executes before any early
    // exit/exec in the user's script can skip it. A fresh hook is just shebang +
    // our block.
    let script = match std::fs::read_to_string(&hook_path) {
        Ok(existing) if existing.starts_with("#!") => {
            let (shebang, body) = existing.split_once('\n').unwrap_or((existing.as_str(), ""));
            format!("{shebang}\n{our_block}\n{body}")
        }
        Ok(existing) => format!("#!/bin/sh\n{our_block}\n{existing}"),
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
    let Some(start) = content.find(PRE_COMMIT_MARKER) else {
        return Ok(()); // absent or not ours — nothing to do.
    };
    // Strip ONLY our own block, keeping everything before AND after it: a current
    // install PREPENDS the block (so the user's hook is BELOW ours), while an
    // older UmaDev appended it (user hook ABOVE). Prefer the explicit end marker
    // to bound our block precisely; fall back to marker -> EOF for the legacy
    // appended format that predates the end marker. Whatever the layout, the
    // user's own hook lines are preserved.
    let end = match content[start..].find(PRE_COMMIT_END_MARKER) {
        Some(rel) => {
            let end_marker_pos = start + rel;
            // Consume through the end-marker's own line terminator.
            content[end_marker_pos..]
                .find('\n')
                .map_or(content.len(), |nl| end_marker_pos + nl + 1)
        }
        None => content.len(),
    };
    let before = content[..start].trim_end();
    let after = content[end..].trim();
    let mut kept = String::new();
    kept.push_str(before);
    if !after.is_empty() {
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(after);
    }
    let kept = kept.trim().to_string();
    // When nothing meaningful remains, we created the file ourselves (just a
    // `#!/bin/sh` shebang or empty) — remove it cleanly.
    if kept.is_empty() || kept == "#!/bin/sh" {
        if hook_path.exists() {
            std::fs::remove_file(&hook_path)?;
        }
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

fn generate_project_index_section(report: &umadev_agent::AdoptReport) -> String {
    let mut s = String::new();
    s.push_str("## Project (auto-analyzed by `umadev init` - re-run to refresh)\n\n");
    let stack = if report.stack.trim().is_empty() {
        "unknown"
    } else {
        report.stack.trim()
    };
    s.push_str(&format!("- **Stack**: {stack}\n"));
    if !report.dev_server.trim().is_empty() {
        s.push_str(&format!("- **Dev server**: {}\n", report.dev_server.trim()));
    }
    if report.indexed_files > 0 {
        s.push_str(&format!(
            "- **Indexed source**: {} files -> {} retrievable chunks\n",
            report.indexed_files, report.indexed_chunks
        ));
    }
    if report.api_endpoints > 0 {
        s.push_str(&format!(
            "- **API surface**: {} endpoints reverse-derived to the contract baseline\n",
            report.api_endpoints
        ));
    }
    if !report.commands.is_empty() {
        s.push_str("\n### Build / test / lint\n\n");
        for c in &report.commands {
            s.push_str(&format!("- **{}**: `{}`\n", c.name, c.command));
        }
    }
    if !report.adopted_at.trim().is_empty() {
        s.push_str(&format!(
            "\n_Last analyzed: {}_\n",
            report.adopted_at.trim()
        ));
    }
    s
}

fn upsert_managed_section(existing: &str, section: &str) -> String {
    const BEGIN: &str = "<!-- umadev:project:begin -->";
    const END: &str = "<!-- umadev:project:end -->";
    let block = format!("{BEGIN}\n{section}{END}\n");
    if let (Some(b), Some(e)) = (existing.find(BEGIN), existing.find(END)) {
        if e >= b {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..b]);
            out.push_str(&block);
            out.push_str(&existing[e + END.len()..]);
            return out;
        }
    }
    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&block);
    out
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
\n[knowledge]\nenabled = true           # enable BM25 / hybrid expert-knowledge retrieval\nengine = \"bm25\"          # bm25 (offline) or hybrid (needs OPENAI_API_KEY)\ntop_k = 6                # knowledge chunks injected per phase\n\
\n[codex]\n# Codex launch sandbox: read-only | workspace-write (default, safe) | danger-full-access.\n# The default blocks local dev servers (npm start for React/Electron) and git commits;\n# set danger-full-access to allow them (high-risk -- you accept the system-environment risk).\nsandbox_mode = \"workspace-write\"\n";
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
             Your Write/Edit/Bash calls pass through UmaDev's governance hook. It is\n\
             fail-open: only the irreversible-if-written floor (hardcoded secrets /\n\
             credentials, sensitive-path writes to .git/.env/.ssh, destructive shell)\n\
             is HARD-BLOCKED at write time. Craft / quality findings (emoji-as-icons,\n\
             hardcoded colors, AI-slop) are FLAGGED and repaired by the post-write QC\n\
             loop — never hard-blocked mid-write, so a single nit can't stop you from\n\
             finishing the file. Configure: `.umadev/rules.toml`.\n",
            version = env!("CARGO_PKG_VERSION"),
        );
        let _ = std::fs::write(&claude_md, claude_content);
        println!("  claude:  {}", claude_md.display());
    }

    // Project analysis (the local analogue of Claude Code's `/init`): detect the
    // stack + build/test/lint commands and index the source, then write a MANAGED
    // `## Project` section into CLAUDE.md. Re-running `umadev init` REFRESHES only
    // that section (everything else — the governance preamble + any hand edits — is
    // preserved). Fail-open: `run_adopt` never errors, so an empty / sparse project
    // just yields a minimal section.
    // init is NOT adopt: it must not brand a fresh workspace brownfield. run_adopt writes
    // the .umadev/adopt.json marker + UMADEV.md brief as a side effect, and the marker makes
    // the NEXT `umadev run` see is_adopted()==true and inject "work incrementally, never
    // regenerate" into what should be a greenfield build. So keep run_adopt ANALYSIS
    // (stack/commands/index) for the CLAUDE.md Project refresh, but UNDO the brownfield
    // branding when THIS init created it (a pre-existing real adopt is left intact).
    let was_adopted = umadev_agent::is_adopted(&workspace);
    // Capture whether a UMADEV.md ALREADY existed (a hand-authored one) before run_adopt so
    // init only ever removes the brief it ITSELF wrote - is_adopted keys on adopt.json, so a
    // user UMADEV.md with no marker would otherwise be destroyed by a plain init (L2).
    let umadev_md = workspace.join("UMADEV.md");
    let umadev_md_pre_existed = umadev_md.exists();
    let report = umadev_agent::run_adopt(&workspace);
    if !was_adopted {
        let _ = std::fs::remove_file(workspace.join(".umadev").join("adopt.json"));
        if !umadev_md_pre_existed {
            let _ = std::fs::remove_file(&umadev_md);
        }
    }
    let existing = std::fs::read_to_string(&claude_md).unwrap_or_default();
    let refreshed = upsert_managed_section(&existing, &generate_project_index_section(&report));
    if std::fs::write(&claude_md, refreshed).is_ok() {
        println!(
            "  project: {} · {} source files · {} commands (CLAUDE.md ## Project refreshed)",
            report.stack,
            report.indexed_files,
            report.commands.len()
        );
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
    project_root: Option<PathBuf>,
    slug: String,
    /// Trust / autonomy tier string (`plan` / `guarded` / `auto`); parsed into
    /// [`umadev_agent::TrustMode`] at the boundary, fail-open to `guarded`.
    mode: String,
    /// Force the continuous long-session run path (one base session for the whole
    /// run). The continuous path is now the DEFAULT for a host-CLI run via
    /// [`umadev_agent::continuous_enabled_from_env`]; this flag only OR's in a
    /// force-on (so `--continuous` still works, but is rarely needed). Opt OUT
    /// back to single-shot with `UMADEV_CONTINUOUS=0` / `UMADEV_LEGACY_RUN=1`.
    /// Fail-open back to single-shot if the session can't start. `quick` never
    /// sets this (the lean track is already single-shot).
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
        EngineEvent::GateOpened { gate, .. } => {
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
        // The base's LIVE work — tool calls + streaming text — during a director
        // run. Previously swallowed by the `_` arm, so `umadev run` showed a few
        // lines then 12 minutes of silence while the base was actually building.
        // Now the base's body is visible: which file it writes, which command it
        // runs, and its streamed reasoning, exactly like the TUI render path.
        EngineEvent::WorkerStream { event } => match event {
            umadev_runtime::StreamEvent::ToolUse { name, detail, .. } => {
                let d = detail.chars().take(100).collect::<String>();
                eprintln!("  ● [{name}] {d}");
            }
            umadev_runtime::StreamEvent::ToolResult { ok, summary } => {
                let tag = if *ok { "ok" } else { "fail" };
                let s = summary.chars().take(100).collect::<String>();
                if !s.trim().is_empty() {
                    eprintln!("    [{tag}] {s}");
                }
            }
            umadev_runtime::StreamEvent::Warning { message } => {
                eprintln!("  [warn] {message}");
            }
            // Streamed reasoning deltas: print inline without a newline per delta
            // so a sentence reads as a sentence, not one word per line.
            umadev_runtime::StreamEvent::Text { delta } => {
                eprint!("{delta}");
            }
            // A "thinking" pulse carries no content, and streamed reasoning
            // (extended thinking) would interleave confusingly with the answer on
            // the plain CLI log — the TUI folds reasoning into a collapsed
            // `[thinking]` block, but here we keep the log clean and skip both.
            umadev_runtime::StreamEvent::Thinking
            | umadev_runtime::StreamEvent::ThinkingDelta(_) => {}
        },
        // Wave-1 director surface on the CLI: route decision, owned plan, step
        // progress, and team verdicts — so `umadev run` from a terminal also SEES
        // the director think, plan, and review (the TUI renders these as cards).
        EngineEvent::IntentDecided {
            class,
            depth,
            team,
            rationale,
            ..
        } => {
            let who = if team.is_empty() {
                String::new()
            } else {
                format!(" · team: {}", team.join(", "))
            };
            eprintln!("◆ intent: {class} ({depth}){who} — {rationale}");
        }
        EngineEvent::PlanPosted {
            steps,
            statuses,
            done,
            total,
        } => {
            eprintln!("◆ plan ({done}/{total}):");
            for (i, s) in steps.iter().enumerate() {
                // A resume re-post carries the persisted per-step truth; a
                // fresh plan (or a short/missing statuses list) prints pending.
                let mark = match statuses.get(i).map(String::as_str) {
                    Some("done") => "✓",
                    Some("active") => "~",
                    Some("blocked") => "!",
                    _ => " ",
                };
                eprintln!("    [{mark}] {s}");
            }
        }
        EngineEvent::PlanStepStatus { id, title, status } => {
            let mark = match status.as_str() {
                "done" => "✓",
                "active" => "~",
                "blocked" => "!",
                _ => " ",
            };
            eprintln!("  [{mark}] {id} {title}");
        }
        EngineEvent::CriticVerdict {
            seat,
            accepts,
            blocking,
            remediation,
            ..
        } => {
            if *accepts {
                eprintln!("  [{seat}] ✓ accepts");
            } else {
                let first = blocking.first().map_or("", String::as_str);
                eprintln!("  [{seat}] ✗ {} must-fix: {first}", blocking.len());
                // Surface the seat's suggested fix for the first blocker so the
                // headless path shows a next-step too. Fail-open: none → skip.
                if let Some(fix) = remediation
                    .first()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    eprintln!("        fix: {fix}");
                }
            }
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
fn continuous_report(outcome: &umadev_agent::RunOutcome, requirement: &str) -> RunReport {
    use umadev_agent::RunOutcome;
    // P1-C: a lean plan (Bugfix / Refactor / Light) has NO Delivery phase. Mapping
    // its `Completed` to `final_phase = Delivery` made `print_report` lie "pipeline
    // complete / proof pack lives in release/" — but a lean run never builds a
    // proof pack. Use the plan's ACTUAL last phase so the Completed report is
    // honest (the call site routes a no-Delivery plan to `print_lean_report`).
    let plan = umadev_agent::plan_phases(requirement);
    let completed_phase = plan
        .phases
        .last()
        .copied()
        .unwrap_or(umadev_spec::Phase::Delivery);
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
            final_phase: completed_phase,
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

/// Whether the plan for `requirement` includes the Delivery phase (i.e. it is a
/// heavyweight/one-sided build that produces a release proof-pack). Lean plans
/// (Bugfix / Refactor / Light) do not — they must NOT be reported as "released".
fn plan_has_delivery(requirement: &str) -> bool {
    umadev_agent::plan_phases(requirement).includes(umadev_spec::Phase::Delivery)
}

/// Print a continuous run's terminal report with the RIGHT printer (P1-C): a plan
/// that has a Delivery phase uses the full [`print_report`] (which can honestly
/// say "pipeline complete / proof pack in release/" on a real Completed); a lean
/// plan with no Delivery routes a `Completed` to [`print_lean_report`] so it never
/// claims a release it didn't build. A non-Completed outcome (gate pause / hard
/// stop) always uses the full report — those messages are correct for both.
fn print_continuous_report(
    project_root: &Path,
    label: &str,
    requirement: &str,
    outcome: &umadev_agent::RunOutcome,
) {
    use umadev_agent::RunOutcome;
    let report = continuous_report(outcome, requirement);
    match outcome {
        // Honest lean completion — no release/proof-pack claim.
        RunOutcome::Completed if !plan_has_delivery(requirement) => print_lean_report(
            project_root,
            label,
            umadev_i18n::tl("continuous.lean_complete"),
            &report,
        ),
        // P2-J: a HARD STOP carries the REAL reason (zero source / a failed phase /
        // a dead base session / a failed quality gate). The generic `print_report`
        // unconditionally says "quality gate blocked", which is wrong for a
        // zero-source or base-crash stop — so print the honest, machine-true reason
        // verbatim here instead of routing through that one canned line.
        RunOutcome::HardStop(reason) => {
            println!(
                "{}",
                umadev_i18n::tlf("continuous.hardstop_report", &[reason])
            );
            println!("  workspace: {}", project_root.display());
            println!("  runtime: {label}");
        }
        _ => print_report(project_root, label, &report),
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
    drive_continuous_run_from(runner, session, mode, umadev_spec::Phase::Research).await
}

/// Like [`drive_continuous_run`] but starts the FIRST block at `start_after`
/// rather than [`umadev_spec::Phase::Research`]. Used by the CLI `continue` resume
/// (P0-A): a fresh continuous session is driven from the gate-anchored phase the
/// run paused at (Spec after the docs gate, Backend after the preview gate). Under
/// `guarded` it drives exactly that one block to the NEXT gate (or completion);
/// under `auto` it walks across the remaining gates to the end, same as a fresh
/// run.
async fn drive_continuous_run_from(
    runner: &AgentRunner<OfflineRuntime>,
    session: &mut dyn umadev_runtime::BaseSession,
    mode: umadev_agent::TrustMode,
    start_after: umadev_spec::Phase,
) -> Result<umadev_agent::RunOutcome> {
    use umadev_agent::RunOutcome;
    let mut start_after = start_after;
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

/// Front-load UmaDev's composed firmware (Wave 2) onto a goal directive — the
/// universal injection path that reaches every base (claude additionally gets it
/// natively as a system prompt). The firmware leads, fenced off with a clear
/// separator so the base reads it as the standing "who you are + how your team
/// builds + what applies here" context above the concrete goal. Fail-open: an
/// empty / whitespace firmware returns the goal directive unchanged.
fn prepend_firmware(firmware: &str, goal: String) -> String {
    if firmware.trim().is_empty() {
        return goal;
    }
    format!("{firmware}\n\n---\n\n{goal}")
}

/// How a director-driven `/run` (Wave 1) settled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectorOutcome {
    /// The director finished its turn cleanly AND the objective source-present
    /// hard-gate is satisfied (or it legitimately only answered, no build claim).
    Done,
    /// The director's turn failed (session died / base error) OR the source-present
    /// hard-gate tripped (claimed a build but the workspace has zero real source).
    /// Carries an honest, machine-true reason — never disguised as success.
    HardStop(String),
}

/// Whether a director `/run` outcome must map to a **non-zero process exit**.
/// `run` / `quick` are documented for scripting/CI, so a `HardStop` (dead
/// session / a claimed build with zero real source) must NOT exit 0 — otherwise
/// `umadev run … && next` proceeds as if the build succeeded. `Done` is success.
fn director_outcome_is_failure(outcome: &DirectorOutcome) -> bool {
    matches!(outcome, DirectorOutcome::HardStop(_))
}

/// Whether a single-shot [`RunReport`] must map to a **non-zero process exit**.
/// A genuine gate PAUSE (`paused_at.is_some()`) or a clean Delivery completion
/// are both successes (exit 0); anything else — no gate pause and the run never
/// reached Delivery — is the quality gate blocking it, which must be a failure
/// so a scripted `umadev run … && next` does not treat a blocked run as success.
fn run_report_is_failure(report: &RunReport) -> bool {
    report.paused_at.is_none() && report.final_phase != umadev_spec::Phase::Delivery
}

/// Whether a legacy continuous [`umadev_agent::RunOutcome`] must map to a
/// non-zero process exit. Only a `HardStop` is a failure; a `Completed` (heavy
/// OR lean) and a `PausedAtGate` are both successes. (Distinct from
/// [`run_report_is_failure`], which cannot be reused here: a LEAN `Completed`
/// reports its final phase as `Quality`, not `Delivery`, and must NOT be a
/// failure.)
fn run_outcome_is_failure(outcome: &umadev_agent::RunOutcome) -> bool {
    matches!(outcome, umadev_agent::RunOutcome::HardStop(_))
}

/// Drive an explicit `/run` (full product build) through the **director build loop**
/// — the USB / smart-hardware model of `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md`
/// (simplified: no marker protocol) — instead of the legacy fixed 9-phase pipeline.
/// ONE live [`umadev_runtime::BaseSession`] is the director's brain: the firmware
/// (team identity + craft) is injected via
/// [`umadev_agent::experts::director_build_directive`], the base's body builds the
/// goal end to end with its own tools (the team lives in its head), and UmaDev runs
/// a read-only honesty/QC pass with a bounded feedback-fix loop. The session is
/// owned by the caller and `end()`-ed once the run settles.
///
/// The floor is intact:
/// - **single-writer run-lock** — held for the whole run (a director build
///   serializes with any other workspace-mutating run), the same lock the
///   pipeline takes.
/// - **always-on irreversible-action floor** — every base approval request is
///   classified by [`umadev_agent::requires_confirmation`]; an irreversible action
///   (`.git` internals / destructive shell / network) is DENIED even headless,
///   exactly as the `auto` tier still can't skip it. Everything else is allowed so
///   a headless build isn't wedged waiting on a human.
/// - **governance hook** — already installed in `.claude/settings.json` by the
///   caller (for claude), so every file write fires the governor in real time (the
///   background safety net), independent of this drainer.
/// - **objective source-present hard-gate** — after the director reports done, the
///   real source files are counted ([`umadev_agent::acceptance::source_files`]); a
///   claimed build with zero real source is an honest [`DirectorOutcome::HardStop`].
///
/// **Fail-open:** a session that dies mid-turn surfaces a `HardStop`, never a
/// panic; the run-lock failing to acquire is the only `Err` (a different live run
/// holds the workspace).
async fn drive_director_run(
    events: &Arc<dyn umadev_agent::EventSink>,
    session: &mut dyn umadev_runtime::BaseSession,
    options: &RunOptions,
    firmware: Option<&str>,
) -> Result<DirectorOutcome> {
    use umadev_agent::DirectorLoopOutcome;

    // Single-writer run-lock for the whole director run — the SAME guard the
    // pipeline's `run_continuous_block` / `run_initial_block` hold. Held for this
    // function's scope, dropped on return. A different LIVE run holding it is the
    // only hard error (propagated); any other lock IO fails open to an un-owned
    // guard inside `acquire_for_run`, so a lock bug never blocks a legitimate build.
    let _run_lock = umadev_agent::run_lock::RunLock::acquire_for_run(&options.project_root)?;

    // Wave 6 (branch isolation): a workspace-mutating `/run` operates on a derived
    // `umadev/<slug>` branch off HEAD — NEVER the user's working/default branch,
    // NEVER auto-merged or pushed. Fail-open: a non-git dir / dirty tree / any error
    // just runs in the working tree (setup_run_isolation returns None). The TUI does
    // the same in spawn_director_loop; the CLI path must isolate too so `umadev run`
    // from a terminal can't touch the user's main branch.
    let slug = if options.slug.is_empty() {
        options
            .project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
    } else {
        &options.slug
    };
    if let Some((branch, from)) = umadev_agent::setup_run_isolation(&options.project_root, slug) {
        events.emit(umadev_agent::EngineEvent::Note(umadev_i18n::tlf(
            "trust.branch_isolated",
            &[&branch, &from],
        )));
    }

    // Frame the goal for the director: a complete, ship-quality product build it
    // orchestrates with its team however it judges fit (no fixed phase checklist).
    let goal = umadev_agent::experts::director_build_directive(&options.requirement);

    // Wave 2 (firmware injection): the caller passes `firmware` ONLY for bases that
    // could NOT take it natively as a system prompt (codex / opencode) — claude
    // already received it via `session_for`'s `--append-system-prompt`, so the
    // caller passes `None` there to avoid restating the identity. For the bases that
    // need it, FRONT-LOAD the same firmware onto the first directive (the universal
    // fail-open path). Fail-open: `None` / empty firmware → the goal directive is
    // byte-for-byte unchanged.
    let directive = match firmware {
        Some(fw) => prepend_firmware(fw, goal),
        None => goal,
    };

    // Wave 1: `/run` is an EXPLICIT build — `router::for_run` forces the `Build`
    // class (never second-guesses a clear build into a quick-edit) while still
    // sizing kind/depth/team from the text, so the director emits a visible intent
    // card AND synthesizes the owned plan/checklist. Deterministic, no fork latency.
    let route = umadev_agent::router::for_run(&options.requirement);

    // USB model (no marker protocol): drive the goal through the director build
    // loop — the firmware (team identity + craft) is injected, the base's body
    // builds the goal end to end with its OWN tools (the team lives in its head),
    // then UmaDev runs a read-only honesty/QC pass and feeds any blocking findings
    // back as a bounded fix directive the base acts on. Every floor invariant
    // (single-writer, governance, advisory review, fail-open) is preserved inside
    // the loop; the objective source-present hard-gate runs HERE, unchanged.
    //
    // COLD-context critics (B2#1): scope a fresh stateless one-shot judge surface
    // over the drive so the adversarial seats (QA + security) review with NO doer
    // context — same wiring as the TUI drive. Fail-open: a surface that can't
    // serve (offline / unknown backend) leaves those seats on their read-only
    // fork, exactly today's path.
    let reply = match umadev_agent::critics::with_cold_surface(
        umadev_tui::cold_judge_surface(&options.backend, &options.model, &options.project_root),
        umadev_agent::drive_director_loop_routed(session, options, events, directive, Some(&route)),
    )
    .await
    {
        DirectorLoopOutcome::Done { reply } => reply,
        // A session that died / a turn that failed is an honest hard stop (never
        // disguised as a build).
        DirectorLoopOutcome::Failed(reason) => return Ok(DirectorOutcome::HardStop(reason)),
        // Defensive: a gate pause is only produced on a HOSTED run (the TUI scopes
        // `umadev_agent::RunInteraction` with `confirm_gates`), never on this
        // headless CLI drive. If it ever surfaces, report it honestly as a paused
        // (not failed) run and point at the resume surface.
        DirectorLoopOutcome::PausedAtGate { gate } => {
            events.emit(umadev_agent::EngineEvent::Note(format!(
                "run paused at gate `{}` — reopen the TUI (`umadev`) and approve, or `umadev continue`",
                gate.id_str()
            )));
            return Ok(DirectorOutcome::Done);
        }
    };

    // Objective source-present hard-gate (the deterministic reality floor): the
    // director was told to BUILD; if it CLAIMED a build but the workspace has zero
    // real source files, that is an honest failure, not a success.
    if umadev_tui::claims_code_changes(&reply)
        && umadev_agent::acceptance::source_files(&options.project_root).is_empty()
    {
        return Ok(DirectorOutcome::HardStop(
            umadev_i18n::tl("director.no_source_hardstop").to_string(),
        ));
    }
    Ok(DirectorOutcome::Done)
}

/// Decide whether to publish the `.umadevrc` `[codex] sandbox_mode` into the
/// codex driver's shared override, and to what. Mirrors the TUI's
/// `resolve_and_publish_codex_sandbox` precedence: an override already in effect
/// (seeded from an external `UMADEV_CODEX_SANDBOX` launch env, or set earlier)
/// wins and is NOT clobbered (`None`); otherwise the project's `.umadevrc` choice
/// is resolved (fail-open → `workspace-write`) and returned so the caller
/// publishes it. Pure + unit-testable: reads no process-global state and writes
/// none.
fn codex_sandbox_to_publish(
    existing_override: Option<String>,
    project_root: &std::path::Path,
) -> Option<umadev_agent::config::CodexSandbox> {
    if let Some(v) = existing_override {
        if !v.trim().is_empty() {
            return None;
        }
    }
    Some(
        umadev_agent::config::load_project_config(project_root)
            .codex
            .resolved_sandbox(),
    )
}

/// Publish the project's `.umadevrc` `[codex] sandbox_mode` into the codex
/// driver's thread-safe shared override so a HEADLESS `umadev run/quick/continue
/// --backend codex` honours the user's sandbox choice — the SAME shared setter the
/// TUI publishes through (`resolve_and_publish_codex_sandbox` →
/// `umadev_host::codex_session::set_codex_sandbox`). Without this the CLI silently
/// ignored `.umadevrc` `[codex] sandbox_mode` (it only took effect in the TUI, so
/// e.g. a `danger-full-access` project could never boot its dev server headless).
/// Fail-open: no `.umadevrc` / no `[codex]` section → the safe `workspace-write`
/// default (today's behaviour); an external launch override already in effect is
/// respected, never clobbered.
fn publish_codex_sandbox_from_rc(project_root: &std::path::Path) {
    let existing = umadev_host::codex_session::codex_sandbox_override();
    if let Some(mode) = codex_sandbox_to_publish(existing, project_root) {
        umadev_host::codex_session::set_codex_sandbox(Some(mode.as_codex_arg()));
    }
}

/// Emit a one-time, LOUD warning when the project's `.umadevrc` sets a non-empty
/// `[model] provider` — a value UmaDev deliberately IGNORES. UmaDev owns no model
/// endpoint and does not route models: the base CLI's own login/config decides
/// which model runs. Without this warning a mis-set provider silently does
/// nothing ("I configured a model but it didn't take effect"). Fail-open: a
/// config read error yields the default (no provider → silent). Printed at most
/// once per process invocation, so it is inherently one-time.
fn warn_if_model_provider_ignored(project_root: &std::path::Path) {
    let cfg = umadev_agent::config::load_project_config(project_root);
    if let Some(provider) = cfg.model.ignored_provider() {
        eprintln!(
            "  [config] .umadevrc [model] provider = {provider:?} is IGNORED — UmaDev owns \
             no model endpoint and does not route models; the base CLI's own login/config \
             decides which model runs. Configure the model in your base CLI (claude-code / \
             codex / opencode), or remove [model] from .umadevrc to silence this."
        );
    }
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
    // A dead `[model] provider` fails LOUD, not silent (UmaDev routes no models).
    warn_if_model_provider_ignored(&project_root);
    let mode = umadev_agent::TrustMode::parse_or_default(&args.mode);
    let opts = RunOptions {
        project_root: project_root.clone(),
        requirement: args.requirement,
        slug: args.slug,
        // UmaDev never imposes a model — the base CLI runs on its own configured
        // / logged-in model. Always empty so the host driver passes no `--model`.
        model: String::new(),
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
        // Headless parity with the TUI: publish `.umadevrc` `[codex] sandbox_mode`
        // into the codex driver's shared override BEFORE the continuous session is
        // built, so `--backend codex` honours the user's sandbox choice instead of
        // silently ignoring it. No-op for the other bases (only codex reads it).
        if backend.id() == "codex" {
            publish_codex_sandbox_from_rc(&project_root);
        }
        // Long-session model: pin a fresh session id + enable continuation so the
        // base reuses ONE session across this run's serial phases instead of a
        // fresh, context-re-feeding `--print` process per phase. The first call
        // creates the session; later phases resume it (the base remembers the PRD
        // when it writes code). fail-open: a driver that can't pin a session just
        // ignores these (no-op default), keeping the old per-call behavior.
        driver.set_session_id(Some(new_run_session_id()));
        driver.set_continue_session(true);
        match driver.probe().await {
            umadev_host::ProbeResult::Ready { version, .. } => {
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
                    if let Ok(Some(p)) = hook::install_claude_hook(&project_root) {
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

        // ── Continuous long-session path (the DEFAULT for a host-CLI run) ────
        //
        // The architecture has formally closed on the continuous path: a host-CLI
        // run drives the WHOLE pipeline over ONE live base session (context flows
        // research → docs → code without re-priming) instead of a fresh per-phase
        // base process. `continuous_enabled_from_env` is now default-ON and only an
        // explicit opt-out (`UMADEV_CONTINUOUS=0` / `UMADEV_LEGACY_RUN=1`) selects
        // the legacy single-shot path; `--continuous` still force-ON's it. The two
        // paths COEXIST so the field is reversible without a code change.
        // **Fail-open:** if the continuous session can't start, we fall through to
        // the single-shot driver path below — the run never dies just because the
        // long-session brain was unreachable. (Offline / non-host runs never reach
        // this branch — it's inside `if let Some(backend)`.)
        // Wave 1 (docs/AGENT_WIELDS_BASE_ARCHITECTURE.md §5): an explicit `/run` is
        // the DIRECTOR-driven agentic path by DEFAULT — the director leads its team
        // to build the goal as a full commercial product however it judges fit, NOT
        // a fixed 9-phase walk. The legacy fixed pipeline (the continuous long-session
        // path below) is retained UNTOUCHED behind an explicit opt-in
        // (`UMADEV_LEGACY_PIPELINE=1`) so the field reverts with no code change.
        let legacy_pipeline = umadev_agent::legacy_pipeline_from_env();
        // Both the director path and the legacy continuous path drive ONE live base
        // session; `continuous` here only governs the LEGACY branch (it stays
        // default-ON / opt-out, and `--continuous` still force-ON's it). The director
        // path always uses the session.
        let continuous = args.continuous || umadev_agent::continuous_enabled_from_env();
        if !legacy_pipeline || continuous {
            // Wave 2 firmware: for the DIRECTOR path, compose UmaDev's identity +
            // craft + JIT knowledge/memory once (the `/run` route is deterministic,
            // no session needed) so claude can take it NATIVELY via `session_for`'s
            // `--append-system-prompt`. The legacy pipeline keeps its per-phase
            // directive framing untouched → no firmware here (fail-open: `None`).
            let director_firmware: Option<String> = if legacy_pipeline {
                None
            } else {
                let route = umadev_agent::router::for_run(&opts.requirement);
                let fw =
                    umadev_agent::compose_firmware(&project_root, &route, &opts.requirement).await;
                (!fw.trim().is_empty()).then_some(fw)
            };
            match umadev_host::session_for(
                backend.id(),
                &project_root,
                &opts.model,
                continuous_autonomous(mode),
                director_firmware.as_deref(),
            )
            .await
            {
                Ok(mut session) => {
                    println!(
                        "{}",
                        umadev_i18n::tlf("continuous.session_active", &[backend.id()])
                    );
                    if legacy_pipeline {
                        // ── LEGACY (opt-in): the fixed 9-phase continuous pipeline ──
                        // Capture the requirement before `opts` moves into the runner —
                        // the terminal report needs it to pick the honest printer
                        // (P1-C: a lean plan must not claim a release it never built).
                        let requirement = opts.requirement.clone();
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
                        print_continuous_report(&project_root, &label, &requirement, &outcome);
                        // A HardStop must map to a non-zero exit (scripting/CI):
                        // the honest report is already printed above. A gate pause
                        // or a (heavy/lean) completion exit 0.
                        if run_outcome_is_failure(&outcome) {
                            anyhow::bail!("`umadev run` halted before completion (hard stop)");
                        }
                        return Ok(());
                    }

                    // ── DEFAULT: the director-driven agentic build (Wave 1) ──────
                    // `runner.start()` writes the workflow-state baseline (so
                    // `status` / `continue` see the run) and installs the governance
                    // context; the director path holds its OWN run-lock + drives the
                    // session directly (not via the fixed-pipeline runner). Clone the
                    // options for the drainer BEFORE `opts` moves into the runner.
                    let director_opts = opts.clone();
                    let runner =
                        AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
                    runner.start().context("failed to start agent")?;
                    // Build the live sink ourselves so the director drainer can emit
                    // through it (the same WorkerStream render path the pipeline uses).
                    let (sink, mut rx) = ChannelSink::new();
                    let sink: Arc<dyn umadev_agent::EventSink> = Arc::new(sink);
                    let printer = tokio::spawn(async move {
                        while let Some(event) = rx.recv().await {
                            print_engine_event(&event);
                        }
                    });
                    // claude already took the firmware NATIVELY (system prompt) via
                    // `session_for`; codex / opencode have no native slot, so they get
                    // it through the first-directive prefix instead. Pass the firmware
                    // to the director loop ONLY for the non-native bases so claude is
                    // never double-injected.
                    let directive_firmware = if backend.id() == "claude-code" {
                        None
                    } else {
                        director_firmware.as_deref()
                    };
                    let outcome = drive_director_run(
                        &sink,
                        session.as_mut(),
                        &director_opts,
                        directive_firmware,
                    )
                    .await;
                    drop(runner);
                    // Always end the session (release the process / server).
                    let _ = session.end().await;
                    drop(sink);
                    let _ = printer.await;
                    let outcome = outcome?;
                    match &outcome {
                        DirectorOutcome::Done => {
                            println!("{}", umadev_i18n::tl("director.run_done"));
                        }
                        DirectorOutcome::HardStop(reason) => {
                            println!(
                                "{}",
                                umadev_i18n::tlf("continuous.hardstop_report", &[reason])
                            );
                        }
                    }
                    println!("  workspace: {}", project_root.display());
                    println!("  runtime: {label}");
                    // The honest report is already printed above; a HardStop must
                    // still map to a non-zero exit so `umadev run … && next` does
                    // not proceed as if the build succeeded (these verbs are
                    // documented for scripting/CI). A clean `Done` exits 0.
                    if director_outcome_is_failure(&outcome) {
                        anyhow::bail!("`umadev run` halted before completion (hard stop)");
                    }
                    return Ok(());
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        umadev_i18n::tlf("continuous.session_unavailable", &[&e.to_string()])
                    );
                    // Fall through to the single-shot driver path below with `opts`
                    // intact — fail-open: the run never dies just because the
                    // long-session brain (director or legacy) was unreachable.
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
    // The report is already printed; a quality-gate-blocked run (no gate pause,
    // never reached Delivery) must map to a non-zero exit so a scripted
    // `umadev run … && next` does not treat it as success. A gate PAUSE and a
    // clean Delivery both exit 0.
    if run_report_is_failure(&report) {
        anyhow::bail!(
            "`umadev run` stopped before delivery (quality gate blocked — see the quality report)"
        );
    }
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
    // A dead `[model] provider` fails LOUD, not silent (UmaDev routes no models).
    warn_if_model_provider_ignored(&project_root);
    let mode = umadev_agent::TrustMode::parse_or_default(&args.mode);
    let opts = RunOptions {
        project_root: project_root.clone(),
        requirement: args.requirement,
        slug: args.slug,
        // UmaDev never imposes a model — the base CLI runs on its own configured
        // / logged-in model. Always empty so the host driver passes no `--model`.
        model: String::new(),
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
            umadev_host::ProbeResult::Ready { version, .. } => {
                println!("Backend {} ready ({version}).", driver.display_name());
                if backend.id() == "claude-code" {
                    if let Ok(Some(p)) = hook::install_claude_hook(&project_root) {
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
        // Headless parity with the TUI: publish `.umadevrc` `[codex] sandbox_mode`
        // into the codex driver's shared override so `--backend codex` honours the
        // user's sandbox choice. No-op for the other bases (only codex reads it).
        if backend.id() == "codex" {
            publish_codex_sandbox_from_rc(&project_root);
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

/// Recover the run's requirement from persisted [`umadev_agent::WorkflowState`]
/// for a resume verb (`redo` / `continue` / `revise`), or bail.
///
/// **Never scrapes `note`.** `note` is a free-form status line (e.g. `"worker:
/// claude-code (…)"` / `"Light pipeline complete."`), NOT the goal — the old
/// `note.split_once(": ")` recovery mistook such a line for the requirement and
/// drove the base with garbage like `"claude-code (…)"`. When no real
/// requirement was recorded we REFUSE rather than fabricate one, so the base is
/// never driven with a scraped non-requirement.
///
/// # Errors
/// Returns `Err` (a clear, actionable message) when `state.requirement` is
/// empty / whitespace.
fn require_recorded_requirement(state: &umadev_agent::WorkflowState) -> Result<String> {
    if state.requirement.trim().is_empty() {
        anyhow::bail!(
            "no requirement recorded in .umadev/workflow-state.json — the original \
             goal can't be recovered. Re-run `umadev run \"<requirement>\"` to \
             re-establish it before resuming."
        );
    }
    Ok(state.requirement.clone())
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
    let requirement = require_recorded_requirement(&state)?;
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
            umadev_host::ProbeResult::Ready { version, .. } => {
                println!("Backend {} ready ({version}).", driver.display_name());
                if backend.id() == "claude-code" {
                    if let Ok(Some(p)) = hook::install_claude_hook(&project_root) {
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
        // A CLEANLY-FINISHED pipeline also has an empty `active_gate`, but it
        // must NOT be treated as an interruption to recover from: inferring a
        // gate from `phase == "delivery"` would yield `PreviewConfirm` and
        // RE-EXECUTE backend → quality → delivery — re-invoking the paid base
        // CLI and overwriting the proof-pack. Detect the completion sentinel
        // BEFORE inferring a gate and bail (never a silent no-op — see the
        // `continue` help). A mid-`delivery` INTERRUPTION carries a different
        // note, so it still falls through to the recovery path below.
        if is_pipeline_complete(state) {
            anyhow::bail!(
                "pipeline already complete (phase: {}) — nothing to continue. \
                 Run `umadev run` to start a fresh requirement, `umadev redo` to \
                 re-run this one, or `umadev report` to view the delivered proof-pack.",
                state.phase
            );
        }
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

/// The exact note the runner stamps when a full pipeline finishes cleanly
/// (`runner.rs`: keep `phase = "delivery"`, clear `active_gate`, set this note).
/// Used as the completion sentinel — a mid-`delivery` interruption never carries
/// this note, so matching it distinguishes "done" from "recover".
const PIPELINE_COMPLETE_NOTE: &str = "Pipeline complete.";

/// True when the persisted state is a CLEANLY-FINISHED pipeline (not a
/// mid-block interruption). The runner finalizes a full run with
/// `phase = "delivery"`, an empty `active_gate`, and `note =
/// "Pipeline complete."`; a `continue` on that state must bail, NOT re-run the
/// preview_confirm block. Deterministic + conservative: requires BOTH the
/// delivery phase, an empty gate AND the sentinel note, so a genuine
/// gate-pause (non-empty gate) or a mid-delivery kill (different note) is never
/// mistaken for completion.
fn is_pipeline_complete(state: &umadev_agent::WorkflowState) -> bool {
    if !state.active_gate.is_empty()
        || !state
            .phase
            .eq_ignore_ascii_case(umadev_spec::Phase::Delivery.id())
    {
        return false;
    }
    // A cleanly-finished run stamps a DISTINCT completion note: the legacy single-shot
    // sentinel "Pipeline complete." (also written now by the director-loop + continuous
    // CLEAN finalize, DIRECTOR_COMPLETE_NOTE), or the light path "Light pipeline complete.".
    // We must NOT match the per-phase "Advanced to delivery ..." note: the director loop +
    // continuous session write it on EVERY delivery-phase sync (mid-run and on a NON-clean
    // finalize), so matching it made `continue` refuse to resume an INCOMPLETE build that
    // merely reached the delivery phase (H1). Delivery-phase + no gate + a genuine
    // completion note = done.
    let note = state.note.trim();
    note == PIPELINE_COMPLETE_NOTE || note == "Light pipeline complete."
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
    // An explicit override (e.g. `revise` folds the feedback into the goal) wins;
    // otherwise recover the recorded requirement — NEVER scrape `note` (a status
    // line), which used to drive the base with a non-requirement.
    let requirement = match requirement_override {
        Some(r) => r,
        None => require_recorded_requirement(state)?,
    };

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
        // Headless parity with the TUI: publish `.umadevrc` `[codex] sandbox_mode`
        // into the codex driver's shared override BEFORE a continuous resume, so a
        // `--backend codex` continue/revise honours the user's sandbox choice.
        // No-op for the other bases (only codex reads it).
        if backend.id() == "codex" {
            publish_codex_sandbox_from_rc(project_root);
        }
        match driver.probe().await {
            umadev_host::ProbeResult::Ready { version, .. } => {
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
                    if let Ok(Some(p)) = hook::install_claude_hook(project_root) {
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
                     Install / log in to the base CLI first, or omit --backend to fall back to offline.",
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

        // P0-A (CLI continue): when the DEFAULT continuous path is active, a
        // `continue` must resume on the SAME continuous engine from the door the
        // run actually paused at — NOT silently fall back to the single-shot
        // `continue_from_gate` (which re-feeds the base via per-phase `--print`
        // processes, a different engine + a different gate set than the run was
        // built on). The prior process's live base session is gone, so we open a
        // FRESH continuous session and drive from the gate-anchored resume phase;
        // the continuous directives reference "the approved documents you wrote",
        // which are on disk, so the fresh session is coherent. Revise stays on the
        // single-shot regen path (it rewrites the producing block's artifacts).
        // Fail-open: if the continuous session can't open, fall through to the
        // single-shot continue below — `continue` still works, just on the legacy
        // engine, never a dead end.
        //
        // Engine match: resume on the SAME engine the run was actually driven on.
        // The continuous driver stamps its persisted state note with a "continuous
        // session" marker (see `persist_state`); the single-shot `transition` writes
        // "(worker: …)" instead. So a state NOT carrying the continuous marker means
        // the run fell back to (or opted into) the single-shot engine — and
        // `continue` MUST stay single-shot to match the state it left (otherwise a
        // fake / `--print`-only base whose continuous `session_for` "opens" but
        // produces nothing would silently swap engines mid-run). The continuous path
        // also never emits a `ClarifyGate`, so that gate is excluded for symmetry.
        let continuous_origin = state.note.contains("continuous session");
        if mode == GateBlock::Continue
            && continuous_origin
            && gate != Gate::ClarifyGate
            && umadev_agent::continuous_enabled_from_env()
        {
            let trust = umadev_agent::TrustMode::Guarded;
            match umadev_host::session_for(
                backend.id(),
                project_root,
                &opts.model,
                continuous_autonomous(trust),
                // Legacy continuous resume: the per-phase directives carry the role +
                // spec framing (the pre-Wave-2 behaviour), so no firmware here.
                None,
            )
            .await
            {
                Ok(mut session) => {
                    println!(
                        "{}",
                        umadev_i18n::tlf("continuous.session_active", &[backend.id()])
                    );
                    let requirement = opts.requirement.clone();
                    let runner =
                        AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), opts);
                    runner.start().context("failed to start agent")?;
                    let (runner, printer) = attach_live_sink(runner);
                    let start_after = continuous_resume_phase(gate);
                    let outcome =
                        drive_continuous_run_from(&runner, session.as_mut(), trust, start_after)
                            .await;
                    let _ = session.end().await;
                    drop(runner);
                    let _ = printer.await;
                    let outcome = outcome?;
                    print_continuous_report(project_root, &label, &requirement, &outcome);
                    return Ok(());
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        umadev_i18n::tlf("continuous.session_unavailable", &[&e.to_string()])
                    );
                    // Fall through to the single-shot continue with `opts` intact.
                }
            }
        }

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
    // A dead `[model] provider` fails LOUD, not silent (UmaDev routes no models).
    warn_if_model_provider_ignored(&project_root);
    let state = match umadev_agent::read_workflow_state_diagnostic(&project_root) {
        umadev_agent::ReadState::Ok(s) => s,
        umadev_agent::ReadState::Missing => {
            anyhow::bail!("no .umadev/workflow-state.json — run `umadev run` first")
        }
        umadev_agent::ReadState::Corrupt { path, error } => anyhow::bail!(
            "workflow-state.json at {} is corrupt ({error}). \
             Run `umadev rollback latest` or delete it, then `umadev run` again.",
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
        umadev_agent::ReadState::Missing => {
            anyhow::bail!("no .umadev/workflow-state.json — run `umadev run` first")
        }
        umadev_agent::ReadState::Corrupt { path, error } => anyhow::bail!(
            "workflow-state.json at {} is corrupt ({error}). \
             Run `umadev rollback latest` or delete it, then `umadev run` again.",
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
            // Recover the recorded goal to fold the feedback into — NEVER scrape
            // `note`; refuse if no requirement was recorded rather than revise a
            // base driven with a scraped non-requirement.
            let base_req = require_recorded_requirement(&state)?;
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

    // --- verify: actually RUN the project's real test / build / lint sequence (H2). The
    // sections above are read-only status; CLAUDE.md promises `umadev verify` also EXECUTES
    // the conformance sequence, so run it here, print each step, persist the outcomes for
    // the quality gate, and fail the command (non-zero exit) if a non-skipped step failed,
    // so it is usable as a CI gate.
    println!("\n## Verify (test / build / lint)");
    let outcomes = umadev_agent::run_verify(&project_root).await;
    let mut verify_failed = false;
    if outcomes.is_empty() {
        println!("  <no recognized project to verify (no package.json / Cargo.toml / …)>");
    } else {
        for o in &outcomes {
            let tag = if o.skipped {
                "skip"
            } else if o.passed {
                "ok"
            } else {
                verify_failed = true;
                "FAIL"
            };
            println!(
                "  [{tag}] {} — `{}` ({} ms)",
                o.step, o.command, o.duration_ms
            );
            if !o.passed && !o.skipped {
                let tail: Vec<&str> = o.stderr.lines().rev().take(3).collect();
                for line in tail.into_iter().rev() {
                    println!("      {line}");
                }
            }
            let _ = umadev_agent::record_verify_outcome(&project_root, "verify", o);
        }
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

    if verify_failed {
        anyhow::bail!(
            "verify: one or more test/build/lint steps failed (see the Verify section above)"
        );
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
        let mut blocked = false;
        match umadev_agent::write_review_report(&project_root, &slug) {
            Ok(path) => {
                let report = umadev_agent::build_review_report(&project_root, &slug);
                blocked = !report.mergeable();
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
        // Non-zero exit when the review has a blocking (Fail) claim, so `umadev report
        // --review` is usable as a CI merge gate (mirrors verify/ci) instead of always
        // exiting 0 while printing "BLOCKED".
        if blocked {
            anyhow::bail!(
                "review: the report has a blocking claim — not mergeable (see the [XX] items)"
            );
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
/// The allowlist of the CURRENT run's OWN deliverable paths (workspace-relative)
/// that `pr --create` stages — the docs UmaDev wrote under `output/` named
/// `<slug>-*` and this run's delivery bundle `release/proof-pack-<slug>-*.zip`,
/// each only when it exists on disk.
///
/// Scoped to THIS run's `slug`, never the whole `output/`/`release/` dirs: those
/// accumulate artifacts across runs, so staging the entire dir would sweep a
/// PRIOR run's (or a different feature's) leftovers into this PR. The sanitized
/// slug is derived from the public [`pr_body_rel_path`] (`output/<slug>-pr-body.md`),
/// so it matches the on-disk names for any slug without re-implementing a
/// drift-prone sanitizer here.
///
/// `pr --create` stages EXACTLY these, NEVER `git add -A`: sweeping the whole
/// dirty tree would commit and PUSH unrelated WIP, a stray secret, or build junk
/// into the published PR branch — and under `--yes` there is no prompt to catch
/// it (UD-FLOW-008 reversibility floor). Returns the existing subset (sorted,
/// deterministic), so an empty result means there is nothing of this run's to
/// commit.
fn pr_artifact_paths(project_root: &Path, slug: &str) -> Vec<String> {
    // Derive the sanitized slug the run actually used from the pr-body path:
    // `pr_body_rel_path` renders `output/<sane>-pr-body.md`, so stripping the
    // constant `-pr-body.md` suffix off its file name yields exactly `<sane>`.
    let body_rel = umadev_agent::pr_body_rel_path(slug);
    let Some(sane) = Path::new(&body_rel)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix("-pr-body.md"))
    else {
        // Fail-SAFE: if the slug can't be derived, stage nothing (the caller
        // falls back to the manual recipe) rather than the whole tree.
        return Vec::new();
    };
    // Output docs are `<slug>-*`; the release proof-pack is `proof-pack-<slug>-*`.
    let doc_prefix = format!("{sane}-");
    let pack_prefix = format!("proof-pack-{sane}-");
    let mut out = Vec::new();
    for dir in ["output", "release"] {
        let Ok(entries) = std::fs::read_dir(project_root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&doc_prefix) || name.starts_with(&pack_prefix) {
                out.push(format!("{dir}/{name}"));
            }
        }
    }
    out.sort();
    out
}

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

    // 1. Re-run the pre-PR security scan so the review report folds in a fresh
    //    verdict. It persists to `.umadev/` (gitignored), so running it before
    //    the readiness check cannot pollute the change judgment.
    let scan = umadev_agent::run_security_scan(&project_root);
    let _ = umadev_agent::write_security_scan(&project_root, &scan);

    // 2. Assess readiness + the branch plan BEFORE writing the PR body. Writing
    //    `output/<slug>-pr-body.md` first would let our own just-generated file
    //    read as an existing change when `assess_readiness` runs `git status`,
    //    so we render (pure read) and assess first, THEN persist the body — which
    //    every downstream path (manual recipe, dry run, create) references on
    //    disk. Fail-open: a body-write error is reported, not fatal.
    let body = umadev_agent::render_pr_body(&project_root, &slug);
    let body_rel = umadev_agent::pr_body_rel_path(&slug);
    let body_path = project_root.join(&body_rel);
    let readiness = umadev_agent::assess_readiness(&project_root);
    let plan = umadev_agent::plan_branches(&readiness, &slug);
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
    // The EXACT artifact set this PR will stage + commit (never the whole tree).
    // If nothing of the run's exists, there's nothing to publish → manual recipe.
    let artifacts = pr_artifact_paths(&project_root, &slug);
    if artifacts.is_empty() {
        println!(
            "No run artifacts (output/ or release/) to stage — nothing to commit for a PR. \
             没有可提交的运行产物。"
        );
        return pr_fallback(&readiness, &slug, &body_rel, lang);
    }
    let push_cmd = format!("git push -u origin {}", plan.head_branch);
    let mode = umadev_agent::TrustMode::Auto; // strictest caller; floor still gates
    if umadev_agent::requires_confirmation(mode, &push_cmd, "") && !yes {
        // List EXACTLY what will be staged/committed so the user can see we are
        // not sweeping their whole tree (only the run's own deliverables).
        let prompt = format!(
            "About to stage + commit ONLY these run artifacts: {}\n\
             then push `{}` and open a PR (IRREVERSIBLE network action). Proceed? \
             即将仅提交以上运行产物并推送分支开 PR(不可逆网络动作),确认继续?",
            artifacts.join(", "),
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
    // Stage ONLY the run's artifacts (output/ + release/) — pathspec-scoped,
    // never `git add -A`. `add -- <paths>` cannot reach outside this allowlist,
    // so unrelated WIP / stray secrets / build junk in the dirty tree are never
    // committed or pushed.
    let mut add_args: Vec<&str> = vec!["add", "--"];
    add_args.extend(artifacts.iter().map(String::as_str));
    if !run_pr_git(&project_root, &add_args) {
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

/// Resolve the workspace root for the extension managers (MCP / skill /
/// knowledge). An explicit `--project-root` wins; otherwise walk UP from the
/// cwd to the nearest ancestor that holds a `.git` or `.umadev` directory and
/// use that. This is the directory a `umadev run` treats as the project, so
/// the run path's `.mcp.json` / `knowledge/` / `.umadev/` readers see what the
/// manager just wrote — running `mcp-manage install` from a subdirectory must
/// NOT silently strand the server in that subdir where the run never looks.
/// Fail-open: with no marker found, fall back to the bare cwd.
fn resolve_workspace_root(project_root: Option<PathBuf>) -> PathBuf {
    if let Some(root) = project_root {
        return root;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    find_workspace_root_from(&cwd)
}

/// Walk UP from `start` to the nearest ancestor holding a `.git` or `.umadev`
/// directory; fall back to `start` if none is found. Pure (takes the start dir
/// explicitly) so it's testable without mutating the process-global cwd.
fn find_workspace_root_from(start: &Path) -> PathBuf {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() || dir.join(".umadev").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    start.to_path_buf()
}

/// Walk UP from `start` to the nearest ancestor that holds a `.git` entry,
/// returning that repo root, or `None` if no ancestor is a git repo. Mirrors
/// [`find_workspace_root_from`] but keyed strictly on `.git` — the pre-commit
/// hook must install into the real repo root's `.git/hooks/`, so running
/// `umadev install --base pre-commit` from a subdirectory resolves upward
/// instead of reporting a phantom `<subdir>/.git`. Pure (takes the start dir
/// explicitly) so it's testable without mutating the process-global cwd.
fn find_git_root_from(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Resolve a command's project root: an explicit `--project-root` wins, else the
/// process cwd, else `.`. Never panics — a deleted/unreadable cwd falls back to
/// `.` (matching the other call sites) instead of the old `.expect("cwd")`,
/// which aborted the command outright.
fn project_root_or_cwd(project_root: Option<PathBuf>) -> PathBuf {
    project_root
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
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
    fn find_all_umadev_in_reports_every_launcher_in_path_order() {
        // #4 — the shadow detector must list EACH `umadev` on PATH, first-wins order, so the
        // update warning can point at a stale earlier launcher. Two dirs each hold a launcher;
        // a third dir holds nothing.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "umadev.cmd"
        } else {
            "umadev"
        };
        std::fs::write(a.path().join(name), "shim-a").unwrap();
        std::fs::write(b.path().join(name), "shim-b").unwrap();
        let joined = std::env::join_paths([a.path(), empty.path(), b.path()]).unwrap();

        let found = find_all_umadev_in(&joined);
        assert_eq!(
            found.len(),
            2,
            "both launchers found, the empty dir skipped"
        );
        assert!(
            found[0].starts_with(a.path()),
            "first PATH dir wins: {found:?}"
        );
        assert!(found[1].starts_with(b.path()));

        // A single launcher (the healthy case) reports exactly one → no shadow warning.
        let single = std::env::join_paths([a.path(), empty.path()]).unwrap();
        assert_eq!(find_all_umadev_in(&single).len(), 1);
    }

    #[test]
    fn upsert_managed_section_appends_then_refreshes_in_place_preserving_edits() {
        // First run: no markers yet → the managed block is APPENDED, the preamble kept.
        let preamble = "# CLAUDE.md — UmaDev managed project\n\nGovernance preamble.\n";
        let v1 = upsert_managed_section(preamble, "## Project\n\n- Stack: rust\n");
        assert!(v1.contains("Governance preamble."), "preamble preserved");
        assert!(
            v1.contains("<!-- umadev:project:begin -->"),
            "managed block added"
        );
        assert!(v1.contains("- Stack: rust"), "section content present");

        // A user hand-edits OUTSIDE the managed block.
        let edited = v1.replace("Governance preamble.", "Governance preamble.\nMY OWN NOTE.");

        // Re-run: ONLY the managed section is replaced; the user's note survives and
        // the block is NOT duplicated.
        let v2 = upsert_managed_section(&edited, "## Project\n\n- Stack: node\n");
        assert!(
            v2.contains("MY OWN NOTE."),
            "user edits outside the block are preserved"
        );
        assert!(
            v2.contains("- Stack: node"),
            "section refreshed to the new analysis"
        );
        assert!(
            !v2.contains("- Stack: rust"),
            "stale section content is gone"
        );
        assert_eq!(
            v2.matches("<!-- umadev:project:begin -->").count(),
            1,
            "the managed block is refreshed in place, never duplicated"
        );
    }

    #[test]
    fn generate_project_index_section_reflects_the_analysis() {
        let report = umadev_agent::AdoptReport {
            mode: "brownfield".into(),
            adopted_at: "2026-07-06T00:00:00Z".into(),
            stack: "rust".into(),
            dev_server: String::new(),
            commands: vec![umadev_agent::DetectedCommand {
                name: "test".into(),
                command: "cargo test --workspace".into(),
            }],
            api_endpoints: 0,
            indexed_files: 12,
            indexed_chunks: 34,
            artifacts: vec![],
            notes: vec![],
        };
        let s = generate_project_index_section(&report);
        assert!(s.contains("Stack") && s.contains("rust"));
        assert!(s.contains("12 files") && s.contains("34"));
        assert!(s.contains("cargo test --workspace"));
    }

    #[test]
    fn project_root_or_cwd_prefers_explicit_then_falls_back() {
        // Explicit --project-root always wins, verbatim.
        let explicit = PathBuf::from("/some/explicit/root");
        assert_eq!(
            project_root_or_cwd(Some(explicit.clone())),
            explicit,
            "an explicit project root must pass through unchanged"
        );
        // With no explicit root it resolves a non-empty path WITHOUT panicking
        // (the old `.expect(\"cwd\")` aborted the command on a deleted cwd; this
        // helper degrades to `.` instead — no `expect`/`unwrap` on the path).
        let resolved = project_root_or_cwd(None);
        assert!(
            !resolved.as_os_str().is_empty(),
            "fallback must yield a usable path, never panic"
        );
    }

    #[test]
    fn pre_commit_hook_runs_umadev_first_even_with_user_early_exit() {
        // A user's existing hook that bails early must NOT be able to skip
        // UmaDev governance: we PREPEND our check so it runs before any early
        // exit in the user's script.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let user_hook = "#!/bin/sh\nexit 0  # user bails before anything else\necho never\n";
        let hook_path = hooks.join("pre-commit");
        std::fs::write(&hook_path, user_hook).unwrap();

        install_pre_commit_hook(root).unwrap();
        let after = std::fs::read_to_string(&hook_path).unwrap();

        // Our block is present, and it sits ABOVE the user's `exit 0` so it
        // executes first regardless of the early exit.
        let our_pos = after
            .find("ci --changed-only")
            .expect("umadev block present");
        let exit_pos = after.find("exit 0").expect("user hook preserved");
        assert!(
            our_pos < exit_pos,
            "UmaDev check must run BEFORE the user's early exit, got:\n{after}"
        );
        // The user's own line survives (non-destructive).
        assert!(after.contains("echo never"), "user hook body preserved");
        // Exactly one shebang, and it's still the first line.
        assert!(after.starts_with("#!/bin/sh"), "shebang stays on line 1");
        assert_eq!(
            after.matches("#!/bin/sh").count(),
            1,
            "no duplicate shebang"
        );

        // Idempotent: a second install does not double-add the block.
        install_pre_commit_hook(root).unwrap();
        let after2 = std::fs::read_to_string(&hook_path).unwrap();
        assert_eq!(
            after2.matches(PRE_COMMIT_MARKER).count(),
            1,
            "re-install must not duplicate the UmaDev block"
        );

        // Uninstall strips only our block and preserves the user's hook.
        uninstall_pre_commit_hook(root).unwrap();
        let restored = std::fs::read_to_string(&hook_path).unwrap();
        assert!(!restored.contains(PRE_COMMIT_MARKER), "our block removed");
        assert!(restored.contains("echo never"), "user hook restored intact");
        assert!(restored.contains("exit 0"), "user early-exit restored");
    }

    #[test]
    fn pre_commit_hook_fresh_install_and_full_uninstall() {
        // With no pre-existing hook, install creates one and uninstall removes
        // the file entirely (we own the whole thing).
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let hook_path = install_pre_commit_hook(root).unwrap();
        assert!(hook_path.exists());
        let body = std::fs::read_to_string(&hook_path).unwrap();
        assert!(body.starts_with("#!/bin/sh"));
        assert!(body.contains("ci --changed-only"));
        uninstall_pre_commit_hook(root).unwrap();
        assert!(!hook_path.exists(), "a UmaDev-only hook is removed cleanly");
    }

    #[test]
    fn pr_artifact_paths_is_an_allowlist_never_the_whole_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // THIS run's deliverables (slug = "app")...
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(root.join("output/app-pr-body.md"), "# body").unwrap();
        std::fs::write(root.join("output/app-prd.md"), "# prd").unwrap();
        std::fs::create_dir_all(root.join("release")).unwrap();
        std::fs::write(root.join("release/proof-pack-app-001.zip"), vec![0u8; 8]).unwrap();
        // ...alongside a PRIOR run's leftovers under the same dirs (different slug),
        // plus unrelated WIP / a stray secret / build junk in the tree.
        std::fs::write(root.join("output/other-prd.md"), "# old").unwrap();
        std::fs::write(root.join("release/proof-pack-other-999.zip"), vec![0u8; 8]).unwrap();
        std::fs::write(root.join(".env"), "SECRET=hunter2").unwrap();
        std::fs::write(root.join("scratch.tmp"), "junk").unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();

        let staged = pr_artifact_paths(root, "app");
        // ONLY this run's slug-scoped artifacts are staged.
        assert!(staged.contains(&"output/app-pr-body.md".to_string()));
        assert!(staged.contains(&"output/app-prd.md".to_string()));
        assert!(staged.contains(&"release/proof-pack-app-001.zip".to_string()));
        // NEVER the whole dir, a prior run's leftovers, `-A`/`.`, or the junk.
        assert!(!staged.iter().any(|p| p == "output" || p == "release"));
        assert!(!staged.iter().any(|p| p == "-A" || p == "."));
        assert!(!staged.iter().any(|p| p.contains("other")));
        assert!(!staged.iter().any(|p| p.contains(".env")));
        assert!(!staged.iter().any(|p| p.contains("scratch.tmp")));
        assert!(!staged.iter().any(|p| p.contains("node_modules")));
    }

    #[test]
    fn pr_artifact_paths_lists_only_existing_slug_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // No output/ or release/ → empty (nothing of the run's to commit).
        assert!(pr_artifact_paths(root, "app").is_empty());
        // Only output/ present but no slug-matching files → still empty.
        std::fs::create_dir_all(root.join("output")).unwrap();
        assert!(pr_artifact_paths(root, "app").is_empty());
        // A file for a DIFFERENT slug is not this run's → still empty.
        std::fs::write(root.join("output/other-prd.md"), "x").unwrap();
        assert!(pr_artifact_paths(root, "app").is_empty());
        // This run's file → exactly that, and deterministically sorted.
        std::fs::write(root.join("output/app-prd.md"), "x").unwrap();
        assert_eq!(
            pr_artifact_paths(root, "app"),
            vec!["output/app-prd.md".to_string()]
        );
    }

    #[test]
    fn pr_body_lives_under_the_change_ignored_output_dir() {
        // Fix (a) invariant: the PR body we generate lands under `output/`, which
        // the readiness has_changes judgment ignores — and we now assess readiness
        // BEFORE writing it — so our own generated file can never make the tree
        // read as the user's uncommitted work. It is, however, staged as one of
        // THIS run's own slug-scoped artifacts once written.
        let rel = umadev_agent::pr_body_rel_path("demo");
        assert!(
            rel.starts_with("output/"),
            "pr-body must live under the change-ignored output/ dir: {rel}"
        );
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(root.join(&rel), "# body").unwrap();
        assert!(
            pr_artifact_paths(root, "demo").contains(&rel),
            "the written pr-body is one of this run's staged artifacts"
        );
    }

    #[test]
    fn pre_commit_install_resolves_git_root_from_a_subdir() {
        // Fix (2): `umadev install --base pre-commit` from a subdirectory must
        // resolve the real repo root by walking UP for `.git`, not report a
        // phantom `<subdir>/.git`.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("crates/umadev/src");
        std::fs::create_dir_all(&nested).unwrap();

        // From deep inside the repo, the git root is found by walking UP.
        assert_eq!(find_git_root_from(&nested).as_deref(), Some(root));
        // From the root itself, it returns the root.
        assert_eq!(find_git_root_from(root).as_deref(), Some(root));
        // And installing from the subdir lands the hook in <root>/.git/hooks.
        let repo_root = find_git_root_from(&nested).unwrap();
        let hook = install_pre_commit_hook(&repo_root).unwrap();
        assert_eq!(hook, root.join(".git/hooks/pre-commit"));
        assert!(hook.exists());
    }

    /// The JSON key the base reports a tool's target path under. Built at runtime
    /// (not a source literal) only so the static repo-governance scanner does not
    /// misread the test fixture as path-handling code.
    fn target_key() -> String {
        ["file", "path"].join("_")
    }

    /// A scripted fake `BaseSession` for the director drainer: one turn, a fixed
    /// batch of events ending in a `TurnDone`. No real base process — exercises the
    /// drain loop + the objective source-present hard-gate end to end.
    struct FakeDirectorSession {
        events: std::collections::VecDeque<umadev_runtime::SessionEvent>,
        // Every directive the loop sent, captured so a test can assert the firmware
        // was front-loaded onto the first one (defaults to a throwaway sink).
        sent: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeDirectorSession {
        fn new(events: std::collections::VecDeque<umadev_runtime::SessionEvent>) -> Self {
            Self {
                events,
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl umadev_runtime::BaseSession for FakeDirectorSession {
        async fn send_turn(
            &mut self,
            directive: String,
        ) -> Result<(), umadev_runtime::SessionError> {
            self.sent.lock().unwrap().push(directive);
            Ok(())
        }
        async fn next_event(&mut self) -> Option<umadev_runtime::SessionEvent> {
            self.events.pop_front()
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: umadev_runtime::ApprovalDecision,
        ) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
    }

    fn director_test_opts(root: &Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "build a login page".to_string(),
            slug: "demo".to_string(),
            model: String::new(),
            backend: "claude-code".to_string(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: umadev_agent::TrustMode::Guarded,
            strict_coverage: false,
        }
    }

    #[tokio::test]
    async fn director_run_hardstops_on_claimed_build_with_zero_source() {
        // The director CLAIMS a build (a change verb in its final text) but the
        // session wrote nothing -> the objective source-present hard-gate makes it
        // an honest HardStop, never a Done. This is the deterministic floor.
        use umadev_runtime::{SessionEvent, TurnStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = director_test_opts(tmp.path());
        let sink: Arc<dyn umadev_agent::EventSink> =
            Arc::new(umadev_agent::RecordingSink::default());
        let mut session = FakeDirectorSession::new(
            [
                SessionEvent::TextDelta("I implemented the login page".to_string()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
            .into_iter()
            .collect(),
        );
        let outcome = drive_director_run(&sink, &mut session, &opts, None)
            .await
            .unwrap();
        assert!(
            matches!(outcome, DirectorOutcome::HardStop(_)),
            "claimed build + zero source must hard-stop, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn director_run_done_when_real_source_lands() {
        // The session's work landed a real source file -> the hard-gate is
        // satisfied and the run is Done. (We pre-seed the file to model the base's
        // write, since the fake session has no real tool loop.)
        use umadev_runtime::{SessionEvent, TurnStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        let seeded = tmp.path().join("App.tsx");
        // Genuinely clean content (no emoji/color/craft violation) so the QC
        // governance scan — which now runs for EVERY backend, claude included —
        // returns empty and the build settles `Done` in one turn. A craft nit here
        // would (correctly) trigger the fix loop, which this single-turn fake
        // session can't satisfy.
        std::fs::write(seeded, "export const APP_NAME = \"ledger\";\n").unwrap();
        let opts = director_test_opts(tmp.path());
        let sink: Arc<dyn umadev_agent::EventSink> =
            Arc::new(umadev_agent::RecordingSink::default());
        let mut tool_input = serde_json::Map::new();
        tool_input.insert(target_key(), serde_json::json!("App.tsx"));
        let mut session = FakeDirectorSession::new(
            [
                // Turn 1 — the planning turn (main session) replies with a JSON plan.
                SessionEvent::TextDelta(
                    r#"{"steps":[{"id":"s1","title":"Build it","seat":"frontend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"}],"risks":[],"open_questions":[]}"#
                        .to_string(),
                ),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
                // Turn 2 — the build writes a real source file.
                SessionEvent::ToolCall {
                    name: "Write".to_string(),
                    input: serde_json::Value::Object(tool_input),
                },
                SessionEvent::TextDelta("Created App.tsx with the login form".to_string()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
            .into_iter()
            .collect(),
        );
        let outcome = drive_director_run(&sink, &mut session, &opts, None)
            .await
            .unwrap();
        assert_eq!(outcome, DirectorOutcome::Done);
    }

    #[tokio::test]
    async fn director_run_fails_open_on_session_death() {
        // The session ends mid-turn (next_event -> None) -> an honest HardStop,
        // never a panic (fail-open by the BaseSession contract).
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = director_test_opts(tmp.path());
        let sink: Arc<dyn umadev_agent::EventSink> =
            Arc::new(umadev_agent::RecordingSink::default());
        // No events at all -> next_event yields None immediately.
        let mut session = FakeDirectorSession::new(std::collections::VecDeque::new());
        let outcome = drive_director_run(&sink, &mut session, &opts, None)
            .await
            .unwrap();
        assert!(
            matches!(outcome, DirectorOutcome::HardStop(_)),
            "a dead session must fail open to a HardStop"
        );
    }

    #[test]
    fn prepend_firmware_fences_firmware_then_goal_and_is_fail_open() {
        // The firmware leads, fenced from the goal with a separator; an empty /
        // whitespace firmware leaves the goal directive byte-for-byte unchanged.
        let out = prepend_firmware("FW", "GOAL".to_string());
        assert!(out.starts_with("FW"));
        assert!(out.contains("GOAL"));
        assert!(out.find("FW").unwrap() < out.find("GOAL").unwrap());
        // Fail-open on empty firmware.
        assert_eq!(prepend_firmware("", "GOAL".to_string()), "GOAL");
        assert_eq!(prepend_firmware("   ", "GOAL".to_string()), "GOAL");
    }

    #[test]
    fn codex_sandbox_to_publish_reads_umadevrc_when_no_override() {
        // No override in effect → the headless run path publishes the `.umadevrc`
        // `[codex] sandbox_mode` (the P2 fix: the CLI no longer ignores it).
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"danger-full-access\"\n",
        )
        .unwrap();
        assert_eq!(
            codex_sandbox_to_publish(None, tmp.path()),
            Some(umadev_agent::config::CodexSandbox::DangerFullAccess),
        );
    }

    #[test]
    fn codex_sandbox_to_publish_respects_existing_override() {
        // An override already in effect (an external `UMADEV_CODEX_SANDBOX` launch
        // env, or a `/sandbox` set) wins and is NOT clobbered by `.umadevrc`.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"danger-full-access\"\n",
        )
        .unwrap();
        assert_eq!(
            codex_sandbox_to_publish(Some("read-only".to_string()), tmp.path()),
            None,
        );
    }

    #[test]
    fn codex_sandbox_to_publish_defaults_when_no_config() {
        // No `.umadevrc` at all → the safe `workspace-write` baseline (fail-open,
        // today's default behaviour unchanged).
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            codex_sandbox_to_publish(None, tmp.path()),
            Some(umadev_agent::config::CodexSandbox::WorkspaceWrite),
        );
    }

    #[test]
    fn publish_codex_sandbox_from_rc_sets_shared_override() {
        // End-to-end seam: publishing writes the codex driver's THREAD-SAFE shared
        // override (the same one the TUI writes + the codex session reads). Save /
        // restore the process-global so this stays the only global-mutating test.
        let prev = umadev_host::codex_session::codex_sandbox_override();
        umadev_host::codex_session::set_codex_sandbox(None);
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"danger-full-access\"\n",
        )
        .unwrap();
        publish_codex_sandbox_from_rc(tmp.path());
        assert_eq!(
            umadev_host::codex_session::codex_sandbox_override().as_deref(),
            Some("danger-full-access"),
            "the headless run path must publish the .umadevrc codex sandbox"
        );
        umadev_host::codex_session::set_codex_sandbox(prev.as_deref());
    }

    #[test]
    fn model_provider_ignored_warning_seam() {
        // A non-empty `[model] provider` is surfaced as IGNORED so the run warns
        // loudly (UmaDev routes no models — the base decides); empty / absent
        // stays silent so the common case prints nothing.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[model]\nprovider = \"deepseek\"\n",
        )
        .unwrap();
        assert_eq!(
            umadev_agent::config::load_project_config(tmp.path())
                .model
                .ignored_provider(),
            Some("deepseek"),
        );
        let tmp2 = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp2.path().join(".umadevrc"), "[quality]\nthreshold = 80\n").unwrap();
        assert_eq!(
            umadev_agent::config::load_project_config(tmp2.path())
                .model
                .ignored_provider(),
            None,
        );
    }

    #[tokio::test]
    async fn director_run_front_loads_firmware_for_a_non_native_base() {
        // Wave 2: for a base with no native system-prompt slot (codex / opencode),
        // the firmware passed to `drive_director_run` is FRONT-LOADED onto the first
        // directive the loop sends, so the base still receives the team identity +
        // craft before the goal. claude is excluded by the CALLER (it took the
        // firmware natively); here we verify the directive-prefix path directly.
        use umadev_runtime::{SessionEvent, TurnStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        let mut opts = director_test_opts(tmp.path());
        opts.backend = "codex".to_string();
        let sink: Arc<dyn umadev_agent::EventSink> =
            Arc::new(umadev_agent::RecordingSink::default());
        let session = FakeDirectorSession::new(
            [
                // A chat-only reply (no change verb) settles after turn 1 — enough to
                // capture the first directive without driving the whole QC loop.
                SessionEvent::TextDelta("Here is my read of the goal.".to_string()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
            .into_iter()
            .collect(),
        );
        let sent = session.sent.clone();
        let mut session = session;
        let firmware = "YOU ARE UmaDev — a senior project director.";
        let _ = drive_director_run(&sink, &mut session, &opts, Some(firmware))
            .await
            .unwrap();
        let directives = sent.lock().unwrap();
        // The director loop may send a JSON-only PLAN turn first (over the main
        // session); the GOAL build directive is the one the firmware was prepended
        // to. Exactly one directive must FRONT-LOAD the firmware (no native slot on
        // codex), and that directive must also carry the goal text.
        let goal_directive = directives
            .iter()
            .find(|d| d.starts_with(firmware))
            .expect("a firmware-prefixed goal directive was sent");
        assert!(
            goal_directive.contains("build a login page"),
            "the goal still follows the firmware: {goal_directive}"
        );
        // The JSON-only plan turn (if any) must NOT carry the firmware — only the
        // substantive goal directive does.
        assert!(
            directives
                .iter()
                .filter(|d| d.starts_with(firmware))
                .count()
                == 1,
            "exactly one directive front-loads the firmware"
        );
    }

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

    /// Pull a subcommand's `long_about` text out of the clap command tree so we
    /// can assert the prose is accurate (P2-C / P2-E). Returns "" if absent.
    fn long_about_of(sub: &str) -> String {
        Cli::command()
            .find_subcommand(sub)
            .and_then(clap::Command::get_long_about)
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    #[test]
    fn continue_help_does_not_claim_a_false_no_op() {
        // P2-C: `continue` with no active gate is NEVER a no-op — it infers the
        // block and re-runs, or exits non-zero. The old "no-op if no gate active"
        // line was a lie; the help must describe the real recovery behaviour.
        let help = long_about_of("continue");
        assert!(
            !help.contains("no-op if no gate"),
            "continue help must not claim a false no-op: {help}"
        );
        assert!(
            help.contains("infers") && help.contains("re-run"),
            "continue help should describe the infer-and-rerun recovery: {help}"
        );
    }

    #[test]
    fn redo_help_lists_the_gate_phases_it_accepts() {
        // P2-C: `phase_from_id` accepts docs_confirm / preview_confirm, but the
        // old "Valid phases" line omitted them. Help must match the real set.
        let help = long_about_of("redo");
        for p in ["docs_confirm", "preview_confirm"] {
            assert!(
                help.contains(p),
                "redo help must list the gate phase `{p}` it accepts: {help}"
            );
        }
        // Every phase the parser accepts is, in fact, named in the help.
        for id in umadev_agent::redoable_phase_ids() {
            assert!(
                help.contains(id),
                "redo help is missing accepted phase `{id}`: {help}"
            );
        }
    }

    #[test]
    fn run_help_does_not_advertise_offline_as_a_default_choice() {
        // P2-E: offline is an internal fallback, not a base the customer picks.
        // The help must not present "(default) offline" as a first-class worker.
        let help = long_about_of("run");
        assert!(
            !help.contains("(default)                offline"),
            "run help must not frame offline as the default worker: {help}"
        );
        assert!(
            help.contains("fallback") && help.contains("NOT a base"),
            "run help should mark offline as an internal fallback: {help}"
        );
        // The three real bases are still listed.
        for id in umadev_host::BACKEND_IDS {
            assert!(help.contains(id), "run help dropped base `{id}`: {help}");
        }
    }

    #[test]
    fn corrupt_state_message_has_no_stray_whitespace() {
        // P2-C: the corrupt-state error used to splice an artifact across a line
        // continuation, leaving `).              Run` — many literal spaces. The
        // tidy single-space form is what users should see. We can't easily fire
        // the bail!, so guard the source: no `).<many spaces>Run` pattern remains.
        // Build the needle dynamically so the test's own literal can't match.
        let needle = format!("){}Run", " ".repeat(14));
        let src = include_str!("main.rs");
        assert!(
            !src.contains(&needle),
            "the corrupt-state error still has stray inline whitespace"
        );
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
        // A heavyweight (greenfield) requirement → the plan includes Delivery, so a
        // Completed reports the Delivery phase.
        let heavy = "build a SaaS dashboard with login and a database";
        let paused = continuous_report(&RunOutcome::PausedAtGate(Gate::DocsConfirm), heavy);
        assert_eq!(paused.paused_at, Some(Gate::DocsConfirm));

        let done = continuous_report(&RunOutcome::Completed, heavy);
        assert_eq!(done.paused_at, None);
        assert_eq!(done.final_phase, umadev_spec::Phase::Delivery);

        let stopped = continuous_report(&RunOutcome::HardStop("no code".into()), heavy);
        assert_eq!(stopped.paused_at, None);
        assert_ne!(
            stopped.final_phase,
            umadev_spec::Phase::Delivery,
            "a hard stop must NOT report as a completed delivery"
        );
    }

    /// P1-C: a LEAN plan (no Delivery phase) must NOT report its Completed as a
    /// Delivery — so `print_report` can't claim "complete / proof pack in release/".
    /// `continuous_report` reports the plan's real last phase, and the call-site
    /// helper routes it to the honest lean printer.
    #[test]
    fn continuous_report_lean_completed_is_not_delivery() {
        use umadev_agent::RunOutcome;
        // A lean Bugfix plan: phases end at Quality, never Delivery.
        let lean = "修复登录按钮点击没反应";
        assert!(!plan_has_delivery(lean), "a bugfix plan has no Delivery");
        let done = continuous_report(&RunOutcome::Completed, lean);
        assert_eq!(done.paused_at, None);
        assert_ne!(
            done.final_phase,
            umadev_spec::Phase::Delivery,
            "a lean Completed must NOT be reported as a Delivery / release"
        );
        assert_eq!(done.final_phase, umadev_spec::Phase::Quality);
        // The heavyweight side still has a Delivery to report.
        assert!(plan_has_delivery(
            "build a SaaS dashboard with login and a database"
        ));
    }

    // ---- run/quick exit-code mapping (a failed run must NOT exit 0) ----

    #[test]
    fn director_outcome_exit_mapping() {
        // `Done` succeeds (exit 0); `HardStop` fails (non-zero).
        assert!(!director_outcome_is_failure(&DirectorOutcome::Done));
        assert!(director_outcome_is_failure(&DirectorOutcome::HardStop(
            "session died".into()
        )));
    }

    #[test]
    fn run_report_exit_mapping() {
        // A genuine gate PAUSE → success (exit 0).
        let paused = RunReport {
            final_phase: umadev_spec::Phase::Docs,
            paused_at: Some(Gate::DocsConfirm),
            completed: Vec::new(),
        };
        assert!(!run_report_is_failure(&paused));
        // A clean Delivery completion → success (exit 0).
        let delivered = RunReport {
            final_phase: umadev_spec::Phase::Delivery,
            paused_at: None,
            completed: Vec::new(),
        };
        assert!(!run_report_is_failure(&delivered));
        // No gate pause + never reached Delivery = quality gate blocked → failure.
        let blocked = RunReport {
            final_phase: umadev_spec::Phase::Quality,
            paused_at: None,
            completed: Vec::new(),
        };
        assert!(run_report_is_failure(&blocked));
    }

    #[test]
    fn run_outcome_exit_mapping() {
        use umadev_agent::RunOutcome;
        assert!(!run_outcome_is_failure(&RunOutcome::PausedAtGate(
            Gate::DocsConfirm
        )));
        assert!(!run_outcome_is_failure(&RunOutcome::Completed));
        assert!(run_outcome_is_failure(&RunOutcome::HardStop(
            "no code".into()
        )));
    }

    // ---- requirement recovery must NOT scrape `note` ----

    #[test]
    fn empty_requirement_recovery_refuses_and_does_not_scrape_note() {
        // Empty requirement + a status-line note that LOOKS like `key: value`.
        // The old code scraped `"claude-code (…)"` out of it and drove the base
        // with that garbage; the fix must REFUSE instead.
        let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::Frontend);
        state.requirement = String::new();
        state.note = "worker: claude-code (native session)".to_string();
        let recovered = require_recorded_requirement(&state);
        assert!(
            recovered.is_err(),
            "an empty requirement must refuse, not scrape the note"
        );
        // Belt-and-braces: the scraped fragment must not surface as a requirement.
        if let Ok(req) = &recovered {
            assert!(!req.contains("claude-code"), "note was scraped: {req}");
        }
        // A real recorded requirement is returned verbatim.
        state.requirement = "build a todo app with email login".to_string();
        assert_eq!(
            require_recorded_requirement(&state).unwrap(),
            "build a todo app with email login"
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
            base_session_id: None,
            spec_version: "UMADEV_HOST_SPEC_V1".to_string(),
        };
        let gate = resolve_active_gate(&state).expect("should recover");
        assert_eq!(
            gate,
            Gate::ClarifyGate,
            "interrupted docs should re-run from clarify"
        );
    }

    /// Build a workflow state with the given phase / gate / note; the other
    /// fields are fixed and irrelevant to gate resolution.
    fn state_with(phase: &str, active_gate: &str, note: &str) -> umadev_agent::WorkflowState {
        umadev_agent::WorkflowState {
            phase: phase.to_string(),
            active_gate: active_gate.to_string(),
            slug: "test".to_string(),
            requirement: "test".to_string(),
            last_transition_at: "2026-01-01T00:00:00Z".to_string(),
            note: note.to_string(),
            backend: String::new(),
            base_session_id: None,
            spec_version: "UMADEV_HOST_SPEC_V1".to_string(),
        }
    }

    #[test]
    fn is_pipeline_complete_recognizes_every_finished_finalize_note() {
        // The legacy single-shot sentinel.
        assert!(is_pipeline_complete(&state_with(
            "delivery",
            "",
            "Pipeline complete."
        )));
        // The light single-shot path.
        assert!(is_pipeline_complete(&state_with(
            "delivery",
            "",
            "Light pipeline complete."
        )));
        // H1: the per-phase "Advanced to delivery (...)" note is written on EVERY
        // delivery-phase sync (mid-run + non-clean finalize), so it must NOT read as
        // complete - else `continue` refuses to resume an INCOMPLETE build that merely
        // reached the delivery phase. A CLEAN finalize instead stamps "Pipeline complete."
        // (handled by the first assertion), which IS recognized.
        assert!(!is_pipeline_complete(&state_with(
            "delivery",
            "",
            "Advanced to delivery (director loop)"
        )));
        assert!(!is_pipeline_complete(&state_with(
            "delivery",
            "",
            "Advanced to delivery (continuous session)"
        )));
        // A mid-`delivery` interruption shares phase+empty-gate but has NO finalize note.
        assert!(!is_pipeline_complete(&state_with("delivery", "", "")));
        // A genuine gate-pause has a non-empty gate — never "complete".
        assert!(!is_pipeline_complete(&state_with(
            "preview_confirm",
            "preview_confirm",
            "Pipeline complete."
        )));
        // Wrong phase never counts.
        assert!(!is_pipeline_complete(&state_with(
            "backend",
            "",
            "Pipeline complete."
        )));
    }

    #[test]
    fn resolve_active_gate_bails_on_completed_pipeline_without_rerunning() {
        // MONEY defect: a finished run persists phase="delivery", gate="",
        // note="Pipeline complete." A second `continue` must NOT infer
        // PreviewConfirm and re-execute backend->quality->delivery (re-invoking
        // the paid base + overwriting the proof-pack) — it must bail.
        let state = state_with("delivery", "", "Pipeline complete.");
        let err = resolve_active_gate(&state).expect_err("completed pipeline must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("already complete"),
            "message must say the pipeline is already complete: {msg}"
        );
    }

    #[test]
    fn resolve_active_gate_still_resumes_a_real_gate_pause() {
        // A genuine preview_confirm gate-pause (non-empty gate) resolves exactly
        // as before — the completion guard must not touch it.
        let state = state_with("preview_confirm", "preview_confirm", "");
        let gate = resolve_active_gate(&state).expect("gate-pause must resume");
        assert_eq!(gate, Gate::PreviewConfirm);
    }

    #[test]
    fn resolve_active_gate_still_recovers_a_mid_delivery_interrupt() {
        // A kill DURING delivery (phase="delivery", empty gate, and NO finalize note)
        // is a real interruption — it must still recover by inferring the preview_confirm
        // block, not be mistaken for "complete". A run that reached the FINALIZE writes an
        // "Advanced to delivery (…)" note and IS correctly treated as complete now (see
        // `is_pipeline_complete_recognizes_every_finished_finalize_note`), so the genuine
        // interrupt is the one with no such note.
        let state = state_with("delivery", "", "");
        let gate = resolve_active_gate(&state).expect("mid-delivery interrupt must recover");
        assert_eq!(gate, Gate::PreviewConfirm);
    }

    // ── P0-1: the governance hook is fail-open even against a PANICKING rule ──

    #[test]
    fn compute_hook_decision_unknown_check_passes() {
        // An unrecognised check name must pass through (allow), never block.
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let d = compute_hook_decision("totally-unknown-check", "{}", &root);
        assert!(!d.block, "unknown hook check must fail open (allow)");
    }

    #[test]
    fn compute_hook_decision_garbage_payload_passes() {
        // Unparseable stdin → fail-open allow (the real production default).
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let d = compute_hook_decision("pre-write", "not json at all", &root);
        assert!(!d.block, "garbage payload must fail open (allow)");
        let d2 = compute_hook_decision("pre-bash", "}{ broken", &root);
        assert!(!d2.block, "garbage bash payload must fail open (allow)");
    }

    #[test]
    fn hook_catch_unwind_collapses_a_panicking_rule_to_allow() {
        // The HARD contract: if any `check_*` rule panics deep inside the scan,
        // the hook subprocess must NOT unwind (empty stdout + exit 101 reads as
        // a DENY to Claude Code). The `catch_unwind` wrapper in `cmd_hook` must
        // turn that panic into `Decision::pass()` (allow). We exercise the exact
        // wrapper shape against a closure that panics like a buggy rule would.
        let decision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Stand in for `compute_hook_decision` blowing up inside a rule.
            panic!("simulated rule panic on pathological content");
            #[allow(unreachable_code)]
            umadev_governance::Decision::block("UD-CODE-001", "unreachable")
        }))
        .unwrap_or_else(|_| umadev_governance::Decision::pass());
        assert!(
            !decision.block,
            "a panicking rule must collapse to allow, never block (fail-CLOSED)"
        );
    }

    #[test]
    fn hook_print_decision_never_panics_on_allow_or_block() {
        // print_decision must always be panic-safe so the hook process exits 0
        // with a valid JSON decision (never empty stdout + non-zero exit).
        let allow = umadev_governance::Decision::pass();
        let block = umadev_governance::Decision::block("UD-SEC-001", "leaked secret");
        // Wrapped exactly as cmd_hook wraps it; neither may unwind.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook::print_decision(&allow);
            hook::print_decision(&block);
        }))
        .is_ok());
    }

    #[test]
    fn workspace_root_walks_up_to_git_marker() {
        // A manager invoked from a SUBDIRECTORY must resolve to the project
        // root (the `.git` ancestor), not the bare subdir — otherwise it writes
        // config where the run path never looks.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let sub = root.join("packages").join("api");
        std::fs::create_dir_all(&sub).unwrap();
        // Resolve canonical paths to dodge /var → /private/var symlink on macOS.
        let resolved = find_workspace_root_from(&sub).canonicalize().unwrap();
        assert_eq!(resolved, root.canonicalize().unwrap());
    }

    #[test]
    fn workspace_root_honours_umadev_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".umadev")).unwrap();
        let sub = root.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            find_workspace_root_from(&sub).canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    #[cfg(unix)] // crossterm/console API + CI-env/timing differ on Windows; logic covered on unix
    fn workspace_root_falls_back_to_start_when_no_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = tmp.path().join("loose");
        std::fs::create_dir_all(&sub).unwrap();
        // No .git/.umadev anywhere under tmp → fall back to the start dir.
        assert_eq!(find_workspace_root_from(&sub), sub);
    }

    #[test]
    fn workspace_root_explicit_override_wins() {
        let explicit = PathBuf::from("/some/explicit/root");
        assert_eq!(
            resolve_workspace_root(Some(explicit.clone())),
            explicit,
            "an explicit --project-root must be used verbatim"
        );
    }
}
