//! `umadev-tui` — Claude Code-style terminal app that drives the
//! UmaDev pipeline.
//!
//! Two screens:
//!
//! 1. **Picker** (first launch only) — `↑↓` to choose one of the three base
//!    CLIs (claude-code / codex / opencode), Enter to save to
//!    `~/.umadev/config.toml`. Offline is an internal demo / CI fallback, not
//!    a picker choice.
//! 2. **Chat** — persistent input box + scrolling conversation history.
//!    Type a requirement, watch the pipeline narrate. Slash commands
//!    (`/claude` `/codex` `/opencode` `/init` `/continue` `/revise`
//!    `/spec` `/verify` `/doctor` `/help` `/quit` `/clear`) switch
//!    base, drive gates, etc. (`/offline` exists as the same fallback.)
//!
//! Pipeline blocks run in background `tokio` tasks; each emits
//! [`EngineEvent`]s through a shared [`ChannelSink`]. The event loop
//! folds those events + key presses into [`App`] state and redraws.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::assigning_clones,
    clippy::format_push_string,
    clippy::doc_markdown
)]

pub mod app;
pub mod config;
pub mod ui;

use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use umadev_agent::{AgentRunner, ChannelSink, EngineEvent, EventSink, Gate, RunOptions};
use umadev_host::driver_for;
use umadev_runtime::{CompletionRequest, Message, OfflineRuntime, Runtime, RuntimeKind};

use crate::app::{Action, App};

/// Launch parameters for [`run`].
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Workspace root.
    pub project_root: PathBuf,
    /// Project slug (empty → inferred from workspace dir name).
    pub slug: String,
    /// Model identifier (host drivers may ignore).
    pub model: String,
}

impl LaunchOptions {
    /// Effective slug — uses cwd dir name when `slug` is empty.
    #[must_use]
    pub fn effective_slug(&self) -> String {
        if !self.slug.is_empty() {
            return self.slug.clone();
        }
        self.project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string()
    }
}

/// Launch the TUI. Blocks until the user quits.
pub async fn run(opts: LaunchOptions) -> Result<()> {
    let config_path = config::default_path();
    let cfg = config::load_from(&config_path);
    let mut app = App::new(
        opts.effective_slug(),
        cfg,
        config_path,
        opts.project_root.clone(),
    );

    // Install a panic hook BEFORE entering raw mode. If anything in the
    // event loop panics, the default hook would print the backtrace but
    // LEAVE THE TERMINAL IN RAW MODE — the user's shell becomes unusable
    // (no echo, no line buffering) until they run `reset`. Our hook
    // restores the terminal first, then forwards to the original hook so
    // the panic message + backtrace still print normally.
    install_panic_hook();
    let mut terminal = setup_terminal().context("failed to set up terminal")?;
    // Name the terminal window/tab `UmaDev — <backend>` so a user juggling
    // several tabs can tell which one drives which base. Uses the configured
    // backend (offline until the first-run picker resolves one); cleared on
    // exit below.
    set_terminal_title(app.backend.as_deref().unwrap_or("offline"));
    let result = event_loop(&mut terminal, &mut app, opts).await;
    // Graceful cleanup: kill any preview dev server the user started via
    // /preview, so quitting UmaDev never leaves an orphaned process.
    if let Ok(mut g) = app.preview_server.lock() {
        if let Some(mut child) = g.take() {
            let _ = child.start_kill();
        }
    }
    restore_terminal(&mut terminal);
    // Reset terminal window title on exit.
    {
        use std::io::Write;
        let _ = write!(std::io::stdout(), "\x1b]0;\x07");
        let _ = std::io::stdout().flush();
    }
    result
}

/// Replace the global panic hook with one that restores the terminal
/// (disable raw mode, leave the alternate screen, show the cursor) before
/// the panic unwinds. Idempotent: the prior hook is chained so repeated
/// installs don't stack indefinitely.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort restoration — ignore errors, we're panicking anyway.
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(DisableBracketedPaste);
        let _ = std::io::stdout().execute(DisableMouseCapture);
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        let _ = std::io::stdout().execute(crossterm::cursor::Show);
        // Print a visible marker so the user knows it was a panic, not a
        // clean exit, then defer to the previous hook for the backtrace.
        eprintln!("\n\numadev: panic — terminal restored.\n");
        prev(info);
    }));
}

/// Resolved decision of which "brain" runs the pipeline, captured up-front so
/// the spawn path has everything it needs without re-reading config. Produced
/// by [`App::brain_spec`]; consumed by [`build_brain`] / [`spawn_block`].
///
/// Precedence: the selected base CLI backend, else the offline template fallback.
#[derive(Debug, Clone)]
pub enum BrainSpec {
    /// Drive a logged-in base CLI subprocess (Claude Code / Codex / `OpenCode`).
    HostCli(String),
    /// Deterministic templates, no AI — internal CI / no-base fallback only.
    Offline,
}

impl BrainSpec {
    /// Human-facing label for status / error messages.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::HostCli(id) => id.clone(),
            Self::Offline => "offline".to_string(),
        }
    }

    /// `true` when this brain is a real AI (a base CLI), i.e. the pipeline should
    /// use the runtime path rather than offline templates.
    #[must_use]
    pub fn is_runtime(&self) -> bool {
        matches!(self, Self::HostCli(_))
    }
}

fn build_brain(
    spec: &BrainSpec,
    continue_session: bool,
    session_id: Option<String>,
    project_root: &std::path::Path,
) -> Result<Box<dyn Runtime>> {
    match spec {
        BrainSpec::Offline => Ok(Box::new(OfflineRuntime::new(RuntimeKind::Anthropic))),
        BrainSpec::HostCli(id) => {
            let mut driver =
                driver_for(id).ok_or_else(|| anyhow::anyhow!("unknown backend `{id}`"))?;
            // A host CLI persists its own conversation; resuming it on
            // follow-up turns is how chat gets real memory (vs. replaying text).
            // An explicit session id (claude) pins OUR conversation so a
            // parallel session in the same dir can't bleed in.
            driver.set_continue_session(continue_session);
            driver.set_session_id(session_id);
            // Drive the base IN the project root (it reads/writes files there).
            driver.set_workspace(project_root.to_path_buf());
            Ok(Box::new(driver))
        }
    }
}

const ROUTE_SYSTEM_PROMPT: &str = "\
You are the brain behind UmaDev. The user talks to a thin shell, but it is you they are talking to.
You are given the conversation so far. Respond to the user's LATEST message, using the earlier turns for context and continuity.
Classify that latest message into exactly one of THREE modes, then return exactly one JSON object and nothing else — no markdown, no code fence.

1. Normal conversation (greetings, small talk, follow-up questions, explanations, discussion — anything answerable by just talking, without looking at the repository):
{\"mode\":\"chat\",\"reply\":\"your direct reply, in the user's language, written as a natural continuation of the conversation\"}

2. A task that needs you to actually READ, INSPECT, or make a SMALL CHANGE to the code in THIS repository — but is NOT building a whole new project. Examples: review/audit a snippet or file for bugs, diagnose a failure, explain how existing code works, answer \"will this code break / leak / regress?\", trace a call path, apply a small fix or tweak, OR report what was actually changed / done or the repo's CURRENT state — e.g. \"what did you just change?\", \"did you do X?\", \"does Y exist?\", \"did the tests pass?\" (these MUST be read back from the real files + git, never recalled from memory):
{\"mode\":\"agentic\",\"task\":\"a cleaned, self-contained instruction in the user's language that folds in any relevant detail from earlier turns — what to look at and what to produce\"}

3. Concrete product/code work that should enter UmaDev's full 9-phase delivery pipeline (build, implement, create, design, or ship a whole feature / product / codebase from a requirement):
{\"mode\":\"run\",\"requirement\":\"a cleaned, self-contained requirement in the user's language that folds in any relevant detail from earlier turns\"}

Guidance: a plain greeting or opinion question is `chat`, never `agentic` — do not spend tool calls on small talk. If the user references THIS repo's code and wants it looked at, checked, explained, or minimally edited, that is `agentic`. Only a from-a-requirement build is `run`.
CRITICAL: any question about what was just changed or done, the repo's CURRENT state, or whether some code / file / result exists or passed → `agentic` (it gets verified against the real files + git), NEVER `chat`. Answering \"what did you change / did you do X\" from conversation memory fabricates changes that may not exist on disk — that failure mode is exactly what `agentic` exists to prevent.
When genuinely unsure between chat and agentic, prefer chat and ask a brief clarifying question.
In chat mode just reply conversationally — do NOT perform the task, edit files, run commands, call tools, or mutate the workspace. (In agentic mode the shell makes a SEPARATE call where you ARE free to use your tools; this classification call must still only return the JSON.)";

#[derive(Debug, Clone, Eq, PartialEq)]
enum RouteDecision {
    Chat(String),
    Run(String),
    /// The base classified the turn as needing real work in THIS repo (review,
    /// diagnose, explain, small fix) but NOT a full pipeline build. Carries the
    /// cleaned task to send back to the base in a SECOND, tools-enabled streaming
    /// call (no tool-ban prompt, no `max_tokens` cap) so the base runs its own
    /// agentic loop — reading files / running commands — with live streaming.
    Agentic(String),
    /// An agentic streaming turn finished. Carries the final assembled text so
    /// the event loop records it as the assistant turn (chat memory continuity);
    /// the body was ALREADY streamed live via `WorkerStream`, so it is NOT
    /// re-rendered. A terminal outcome → clears the "thinking…" status.
    AgenticDone(String),
    /// The route produced no usable reply (base init failed, an empty reply, or
    /// a hard error). Carries the human-readable reason. Routed through the same
    /// channel as `Chat` / `Run` — instead of a bare `EngineEvent::Note` — so
    /// the event loop clears the "thinking…" status on EVERY terminal route
    /// outcome, and a plain progress Note never has to (and no longer does).
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
enum Block {
    Initial,
    /// Run the clarify phase first (generates questions, pauses at `ClarifyGate`).
    /// On resume, `run_initial_block` runs.
    Clarify,
    Continue(Gate),
    /// Lightweight fast track — a lean single shot (spec-lite -> implement ->
    /// quality, no gates) for a trivial change. Drives `run_light`.
    Light,
    /// Re-run a single named phase using the prior run's context. Drives
    /// `redo_phase`.
    Redo(umadev_spec::Phase),
}

/// Set the terminal window/tab title to `UmaDev — <backend>` via an OSC
/// escape sequence (the same trick opencode uses), so a user with several
/// terminal tabs open can tell which one is driving which base. Cleared again
/// on exit in [`run`]. Best-effort: failures (e.g. a terminal that ignores
/// OSC 0) are swallowed — the title is cosmetic and must never block launch.
fn set_terminal_title(backend: &str) {
    // OSC 0 = set both the window title and the icon (tab) title. Safe to
    // write to stdout — crossterm raw mode is already on by this point, so the
    // sequence is consumed by the terminal rather than echoed to the screen.
    use std::io::Write;
    let _ = write!(std::io::stdout(), "\x1b]0;UmaDev \u{2014} {backend}\x07");
    let _ = std::io::stdout().flush();
}

/// Split a worker-recorded run command like `cd web && npm run dev` into
/// (`working_dir`, `program`, `args`). Falls back to running the whole string via
/// `sh -c` when it does not match the `cd X && ...` shape.
fn parse_run_command(
    command: &str,
    project_root: &std::path::Path,
) -> (std::path::PathBuf, String, Vec<String>) {
    // Strip a leading `cd <dir> &&` and resolve it relative to the workspace.
    if let Some(after_cd) = command.trim().strip_prefix("cd ") {
        if let Some((dir, rest)) = after_cd.split_once("&&") {
            let dir = dir.trim().trim_matches(|c| c == '\'' || c == '"');
            let resolved = if std::path::Path::new(dir).is_absolute() {
                std::path::PathBuf::from(dir)
            } else {
                project_root.join(dir)
            };
            let rest = rest.trim();
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if let Some((prog, args)) = parts.split_first() {
                let args: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
                return (resolved, prog.to_string(), args);
            }
        }
    }
    // Fallback: shell out with `sh -c "<command>"` in the workspace root.
    (
        project_root.to_path_buf(),
        "sh".to_string(),
        vec!["-c".to_string(), command.to_string()],
    )
}

/// Extract the host:port from a `http://host:port/...` URL, returning None
/// when parsing fails. Used by [`wait_for_port`] so we only open the browser
/// after the dev server is actually accepting connections — not 0ms after
/// spawn, when Vite is still compiling and the page would 404.
fn url_host_port(url: &str) -> Option<String> {
    let after_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let host_port = after_scheme.split('/').next()?;
    Some(host_port.to_string())
}

/// Poll a `host:port` with a TCP connect until it succeeds or `timeout`
/// elapses. Returns Ok(()) when the dev server is reachable. Mirrors what a
/// browser does — so opening the URL after this returns won't hit a 404 from
/// a half-started server. Runs in the async task so it never blocks the TUI.
async fn wait_for_port(url: &str, timeout: std::time::Duration) -> bool {
    let Some(addr) = url_host_port(url) else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Check whether the port in `url` is currently FREE (nothing listening). We
/// bind to it briefly — if binding fails the port is occupied (by the user's
/// other Vite/Node service), so spawning our dev server would either fail or
/// silently bind a different port while we open the wrong URL. Returning
/// false here tells the caller to NOT spawn and instead hint to the user.
fn port_is_free(url: &str) -> bool {
    let Some(addr) = url_host_port(url) else {
        return false; // can't parse → assume not free (conservative)
    };
    std::net::TcpListener::bind(&addr).is_ok()
}

/// Cross-platform best-effort browser open (sync variant for the event loop).
fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    Ok(())
}

/// Build the user-facing note for a failed `runner.start()`.
///
/// `WouldBlock` is NOT a hard error: it means THIS session already holds the
/// run lock (the previous run is still finishing up its drop). Re-launching the
/// run a beat later succeeds, so we surface a retriable hint instead of the
/// generic `pipeline.start_failed` shout — the lock guard from the just-aborted
/// run just hasn't been dropped yet.
fn start_failed_note(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::WouldBlock {
        umadev_i18n::tl("run.busy_reopen").to_string()
    } else {
        umadev_i18n::tlf("pipeline.start_failed", &[&e.to_string()])
    }
}

fn spawn_block(
    options: RunOptions,
    spec: BrainSpec,
    sink: Arc<ChannelSink>,
    block: Block,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let label = spec.label();
        // The pipeline drives its own multi-phase prompts; it does not share
        // the chat session, so it never resumes (continue_session = false,
        // no pinned session id).
        let brain = match build_brain(&spec, false, None, &options.project_root) {
            Ok(b) => b,
            Err(e) => {
                // build_brain failing (unknown backend / driver build error) is a
                // ZERO-PHASE abort: the run never starts. A bare `Note` here would
                // leave the bar reading idle "0/9" forever — so carry the
                // `ABORT_SENTINEL` (like the other two terminal-abort paths) to
                // flip `aborted` and paint an honest `[aborted]` status.
                sink.emit(EngineEvent::Note(format!(
                    "{ABORT_SENTINEL}{}",
                    umadev_i18n::tlf("worker.init_failed", &[&label, &e.to_string()])
                )));
                return;
            }
        };
        let use_runtime = spec.is_runtime();
        let runner = AgentRunner::new(brain, options).with_event_sink(sink.clone());
        // A failed `runner.start()` (workflow-state write failed before the block
        // even began) is itself a zero-phase abort — surface it through the SAME
        // explicit terminal-abort path, never a bare easily-missed note that
        // leaves the bar reading idle. `start_failed` builds the abort body and
        // signals "stop here".
        macro_rules! start_or_abort {
            () => {
                if let Err(e) = runner.start() {
                    sink.emit(EngineEvent::Note(format!(
                        "{ABORT_SENTINEL}{}",
                        start_failed_note(&e)
                    )));
                    return;
                }
            };
        }
        let outcome = match block {
            Block::Clarify => {
                start_or_abort!();
                runner.run_clarify(use_runtime).await
            }
            Block::Initial => {
                start_or_abort!();
                runner.run_initial_block(use_runtime, None).await
            }
            Block::Continue(gate) => runner.continue_from_gate(gate).await,
            Block::Light => {
                start_or_abort!();
                runner.run_light(use_runtime).await
            }
            // A redo reuses the prior run's persisted state — it must NOT call
            // `start()` (which would reset the workflow back to research).
            Block::Redo(phase) => runner.redo_phase(phase, use_runtime).await,
        };
        if let Err(e) = outcome {
            // A block that returned `Err` produced ZERO phases and is over. The
            // old path emitted one easily-missed progress Note and let the task
            // return, leaving the status bar reading "ready / 0/9" as if the run
            // were merely idle — the exact "looks wedged" bug. Surface an
            // EXPLICIT terminal-abort line (carrying the `ABORT_SENTINEL` so the
            // app renders a real "this round aborted" terminal state, not a
            // silent idle) with a cause + an actionable next step keyed to the
            // error kind. Build it from literals so it stays self-contained;
            // fail-open — emitting a note can never block anything.
            sink.emit(EngineEvent::Note(format!(
                "{ABORT_SENTINEL}{}",
                block_abort_note(&e, &label)
            )));
        }
    })
}

/// The director's persistent base session — ONE continuous brain held across
/// the whole TUI session so context flows research → docs → code → … without
/// re-priming (the long-session model, see
/// `docs/CONTINUOUS_SESSION_ARCHITECTURE.md` §1.5/1.6). `None` until the first
/// continuous run lazily opens it; parked back here at every gate pause so the
/// next `Continue` block reuses the SAME session. A `tokio::sync::Mutex` so the
/// spawned block task can take it across `.await` points; shared `Arc` with the
/// event loop. Empty (always `None`) unless the continuous path is enabled.
type SessionHolder = Arc<tokio::sync::Mutex<Option<Box<dyn umadev_runtime::BaseSession>>>>;

/// Decide whether the TUI's `run` intent flows through the **continuous
/// long-session path** (one persistent director session) or the legacy per-phase
/// single-shot path. The continuous path is now the DEFAULT (mirrors
/// [`umadev_agent::continuous_enabled_from_env`]); an explicit opt-out
/// (`UMADEV_CONTINUOUS=0` / `UMADEV_LEGACY_RUN=1`) selects the single-shot path.
/// The two paths coexist so this is reversible in the field with no code change.
/// Read at the spawn boundary so a run sees a stable snapshot. (At the call site
/// this is further gated on the brain actually being a host CLI — an offline /
/// non-host brain always stays on the single-shot path.)
fn tui_continuous_enabled() -> bool {
    umadev_agent::continuous_enabled_from_env()
}

/// The continuous start phase for the NEXT block after a gate pause — the same
/// gate-anchored block split the single-shot path uses (docs gate → spec block,
/// preview gate → backend block). A `ClarifyGate` is only produced by the
/// single-shot clarify phase, never on the continuous path, so it fails open to
/// the tail.
fn continuous_resume_phase(gate: Gate) -> umadev_spec::Phase {
    match gate {
        Gate::DocsConfirm => umadev_spec::Phase::Spec,
        // ClarifyGate never reaches the continuous path; fold it into the
        // post-preview tail so a stray value can't wedge.
        Gate::PreviewConfirm | Gate::ClarifyGate => umadev_spec::Phase::Backend,
    }
}

/// The continuous session's autonomy flag from the trust tier: only `auto` lets
/// the base write unattended; `guarded` / `plan` keep the human-in-the-loop
/// posture (gate pauses + the per-turn approval floor). Mirrors the binary.
fn continuous_autonomous(mode: umadev_agent::TrustMode) -> bool {
    mode.gates_auto_approve()
}

/// The continuous start phase for RE-DRIVING the block that PRODUCED a gate, when
/// the user revises / steers at that gate (P1-D). Unlike [`continuous_resume_phase`]
/// (which advances PAST the gate), this re-enters the producing block on the SAME
/// held session with the revision folded into the requirement, so the base reworks
/// the right artifacts in context instead of being orphaned onto a single-shot
/// re-feed:
///   - docs gate → re-drive from `Research` (regenerate the three docs)
///   - preview gate → re-drive from `Spec` (regenerate spec → frontend, keeping
///     the already-approved docs)
fn continuous_revise_phase(gate: Gate) -> umadev_spec::Phase {
    match gate {
        Gate::PreviewConfirm => umadev_spec::Phase::Spec,
        // DocsConfirm + the (never-on-continuous) ClarifyGate → regenerate from the
        // top of the producing block.
        Gate::DocsConfirm | Gate::ClarifyGate => umadev_spec::Phase::Research,
    }
}

/// Spawn ONE continuous-session block for the TUI's `run` intent — the long-
/// session counterpart of [`spawn_block`]. Drives the held persistent
/// [`umadev_runtime::BaseSession`] (the director's brain) over
/// [`AgentRunner::run_continuous_block`], lazily opening the session on the first
/// (`Research`) block and PARKING it back in `holder` at a gate pause so the next
/// `Continue` block resumes the SAME session with full context.
///
/// All the surrounding TUI machinery is UNCHANGED: the block emits the same
/// [`EngineEvent`]s (`PipelineStarted` / `PhaseStarted` / `GateOpened` /
/// `BlockCompleted`) over the shared sink, so the gate cards, auto-continue,
/// completion handling, queued-steer drain, run-lock single-writer guard, and
/// `Ctrl-C` task-abort all work exactly as on the single-shot path.
///
/// **Fail-open:** if the session can't open (or a `Continue` arrives with no
/// parked session and a fresh one can't open either), the block emits an
/// `ABORT_SENTINEL` note (the same honest terminal-abort the single-shot path
/// uses) and returns — the caller can retry, or the user falls back to the
/// single-shot path by opting out (`UMADEV_CONTINUOUS=0` / `UMADEV_LEGACY_RUN=1`).
/// It NEVER panics or wedges.
fn spawn_continuous_block(
    options: RunOptions,
    sink: Arc<ChannelSink>,
    holder: SessionHolder,
    start_after: umadev_spec::Phase,
    autonomous: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let backend = options.backend.clone();
        let model = options.model.clone();
        let root = options.project_root.clone();

        // Take the parked session (a resume), or lazily open a fresh one (a new
        // run, or a resume whose session was lost). The session is OWNED by this
        // task for the block's duration; it goes back into `holder` only on a
        // gate pause.
        let mut guard = holder.lock().await;
        let mut session = match guard.take() {
            Some(s) => s,
            None => match umadev_host::session_for(&backend, &root, &model, autonomous).await {
                Ok(s) => s,
                Err(e) => {
                    sink.emit(EngineEvent::Note(format!(
                        "{ABORT_SENTINEL}{}",
                        umadev_i18n::tlf("continuous.tui_session_unavailable", &[&e.to_string()])
                    )));
                    return;
                }
            },
        };
        drop(guard);

        // The runner is an options + event-sink carrier; the offline runtime is
        // never invoked (the continuous driver drives the session directly). It
        // owns the single-writer run lock + the deterministic moat.
        let runner = AgentRunner::new(OfflineRuntime::new(RuntimeKind::Anthropic), options)
            .with_event_sink(sink.clone());
        if let Err(e) = runner.start() {
            sink.emit(EngineEvent::Note(format!(
                "{ABORT_SENTINEL}{}",
                start_failed_note(&e)
            )));
            let _ = session.end().await;
            return;
        }

        match runner
            .run_continuous_block(session.as_mut(), start_after)
            .await
        {
            Ok(umadev_agent::RunOutcome::PausedAtGate(_)) => {
                // Natural pause point: park the LIVE session back so the next
                // `Continue` block resumes it with context retained. The
                // `GateOpened` event already drove the gate card.
                *holder.lock().await = Some(session);
            }
            Ok(umadev_agent::RunOutcome::Completed) => {
                // Run settled — close the session and clear the holder.
                let _ = session.end().await;
            }
            Ok(umadev_agent::RunOutcome::HardStop(reason)) => {
                // Honest terminal abort (zero real code / a failed phase) — the
                // run_block already emitted the detailed Note; flag the terminal
                // state so the bar shows an explicit abort, then close.
                sink.emit(EngineEvent::Note(format!("{ABORT_SENTINEL}{reason}")));
                let _ = session.end().await;
            }
            Err(e) => {
                // The only error path is the run lock (a different live run holds
                // the workspace). Surface it the same way the single-shot path
                // does, then close the session — fail-open, never a panic.
                sink.emit(EngineEvent::Note(format!(
                    "{ABORT_SENTINEL}{}",
                    block_abort_note(&e, &backend)
                )));
                let _ = session.end().await;
            }
        }
    })
}

/// Marker prefixed onto the terminal-abort note emitted by [`spawn_block`] when
/// a block ends with `Err` (zero phases produced). The TUI app recognises this
/// prefix to flip the run into an explicit **aborted** terminal state — clearing
/// the misleading "ready / 0/9" idle look — instead of treating it as an
/// ordinary progress note. A zero-width-ish, human-meaningless tag so it never
/// shows up as visible text after the app strips it.
pub(crate) const ABORT_SENTINEL: &str = "\u{2068}umadev-block-aborted\u{2069}";

/// Build the user-facing "this round aborted: <cause> — <next step>" body for a
/// block that ended with an error, classified by error kind. Self-contained (no
/// i18n key): a lock-residue/race gets a queue/retry/`rm` hint, a genuine
/// concurrent run gets the delete-the-lock hint, and any other IO error gets its
/// concrete cause (disk / permission) so the user knows it failed and why.
fn block_abort_note(e: &std::io::Error, label: &str) -> String {
    use std::io::ErrorKind;
    let detail = e.to_string();
    match e.kind() {
        // Same-session lock still held by our own in-flight block. With the
        // run-execution lock semantics this should no longer reach a real
        // execution path, but if it ever does it is a transient hand-off race,
        // not a dead end — tell the user to retry in a beat.
        ErrorKind::WouldBlock => umadev_i18n::tlf("continuous.block_aborted_busy", &[label]),
        // A different, still-live run holds the workspace lock. The underlying
        // message already carries the `rm <path>` hint.
        ErrorKind::AlreadyExists => {
            umadev_i18n::tlf("continuous.block_aborted_locked", &[label, &detail])
        }
        // Any other IO failure: surface the concrete cause (disk full / read-only
        // fs / permission) so the user sees a real reason, not a frozen bar.
        _ => {
            let hint = if detail.contains("timed out") {
                umadev_i18n::tlf("worker.timeout", &[label])
            } else if detail.contains("not found on PATH") {
                umadev_i18n::tlf("worker.not_on_path", &[label])
            } else if detail.contains("exited with code") {
                umadev_i18n::tl("worker.exited").to_string()
            } else {
                umadev_i18n::tl("pipeline.generic_error").to_string()
            };
            umadev_i18n::tlf("continuous.block_aborted_io", &[&detail, &hint])
        }
    }
}

/// Everything a single routed chat turn needs — bundled so `spawn_route`
/// stays within a sane argument count.
struct RouteTurn {
    /// The user's new message.
    text: String,
    /// Conversation memory for a base that cannot resume its own session
    /// (only the offline fallback today; `HostCli` bases resume natively).
    history: Vec<Message>,
    /// Which base to route to.
    spec: BrainSpec,
    /// Resume this base's prior session (host CLIs) on this turn.
    continue_session: bool,
    /// Explicit session id for bases that support it (claude).
    session_id: Option<String>,
    /// Fallback model id when the spec does not carry one.
    fallback_model: String,
    /// Project root — the cwd the base subprocess runs in.
    project_root: std::path::PathBuf,
}

fn spawn_route(
    turn: RouteTurn,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) {
    let RouteTurn {
        text,
        history,
        spec,
        continue_session,
        session_id,
        fallback_model,
        project_root,
    } = turn;
    tokio::spawn(async move {
        // Offline has no base to ask, so the shell falls back to a keyword
        // heuristic. Both outcomes flow through `route_tx` so the event loop is
        // the single place that records conversation memory.
        if !spec.is_runtime() {
            if App::looks_like_project_requirement(&text) {
                let _ = route_tx.send(RouteDecision::Run(text));
            } else {
                let _ = route_tx.send(RouteDecision::Chat(App::chitchat_reply(&text)));
            }
            return;
        }

        let label = spec.label();
        let request_model = route_model_for_spec(&spec, fallback_model);

        // Memory comes from two different places depending on the base:
        // - HostCli (claude/codex/opencode) persists its OWN session, so we
        //   resume it (`--continue` / `exec resume --last`) and send ONLY the
        //   new turn — the base already remembers the rest (incl. tool calls).
        // - A stateless base (no own session) would need the shell to replay
        //   the whole transcript each call; none of the three bases need this.
        let host_cli = matches!(spec, BrainSpec::HostCli(_));
        let brain = match build_brain(
            &spec,
            host_cli && continue_session,
            if host_cli { session_id.clone() } else { None },
            &project_root,
        ) {
            Ok(b) => b,
            Err(e) => {
                // Terminal route outcome → flow through `route_tx` so the event
                // loop clears `thinking` (a bare Note no longer does).
                let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                    "base.init_failed",
                    &[&label, &e.to_string()],
                )));
                return;
            }
        };

        // The base decides chat-vs-run itself; the shell only relays. `text` /
        // `request_model` are cloned so they survive for a possible retry below.
        let request = if host_cli {
            route_request_single(text.clone(), request_model.clone())
        } else {
            route_request(history, text.clone(), request_model.clone())
        };
        let mut result = brain.complete(request).await;

        // Resume-failure fallback: if a host-CLI session resume failed (the
        // session was pruned/expired, or the very first turn errored before
        // creating one), retry ONCE with a brand-new cold session so the user
        // still gets a reply. Safe because routing only chats — the route
        // prompt forbids tool use / file writes, so a retry has no side effect.
        let attempted_resume = host_cli && (continue_session || session_id.is_some());
        if result.is_err() && attempted_resume {
            if let Ok(fresh) = build_brain(&spec, false, None, &project_root) {
                sink.emit(EngineEvent::Note(
                    umadev_i18n::tl("route.resume_retry").to_string(),
                ));
                result = fresh
                    .complete(route_request_single(text, request_model))
                    .await;
            }
        }

        match result {
            Ok(response) => {
                // Chat and Run both go through the channel — the event loop
                // owns `&mut App`, so it records the turn into conversation
                // memory before reacting.
                if let Some(decision) = parse_route_decision(&response.text) {
                    let _ = route_tx.send(decision);
                } else {
                    let body = response.text.trim();
                    if body.is_empty() {
                        // Terminal route outcome → route channel (clears
                        // `thinking`), not a bare Note.
                        let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                            "base.empty_reply",
                            &[&label],
                        )));
                    } else {
                        // Non-JSON but non-empty → treat the raw text as a
                        // conversational reply rather than dropping it.
                        let _ = route_tx.send(RouteDecision::Chat(body.to_string()));
                    }
                }
            }
            Err(e) => {
                // Terminal route outcome → route channel (clears `thinking`).
                let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                    "route.failed",
                    &[&label, &e.to_string()],
                )));
            }
        }
    });
}

/// Everything the tools-enabled agentic execution call needs. Mirrors a subset
/// of [`RouteTurn`] but for the SECOND call — the one that actually lets the
/// base run its tool loop.
struct AgenticTurn {
    /// The cleaned task the base classified as agentic.
    task: String,
    /// Which base to drive (always a `HostCli` here — offline never reaches this).
    spec: BrainSpec,
    /// Resume the chat session so this turn shares memory with the conversation.
    continue_session: bool,
    /// Pinned chat session id (claude).
    session_id: Option<String>,
    /// Fallback model id when the spec carries none.
    fallback_model: String,
    /// Project root — the cwd the base subprocess runs in (it reads/writes here).
    project_root: std::path::PathBuf,
}

/// Spawn the tools-enabled agentic execution call. UNLIKE [`spawn_route`], this
/// sends the user's task to the base with NO tool-ban system prompt and NO
/// `max_tokens` cap, then drives [`Runtime::complete_streaming`] so the base
/// runs its OWN agentic loop (read files, `git diff`, run commands, analyse) and
/// the tool calls + text stream live into the transcript via
/// [`EngineEvent::WorkerStream`] — reusing the exact render pipeline the 9-phase
/// run uses. The turn ends only when the base's stream ends (subprocess exit),
/// not on the first preamble. The returned [`JoinHandle`] is parked in the event
/// loop's `run_task` slot so Ctrl-C / `Action::Cancel` can abort it.
///
/// Fail-open: a base-init error or a hard streaming error is downgraded to a
/// terminal [`RouteDecision::Failed`] (the user still gets a clear note, the
/// shell never blocks). A clean finish sends [`RouteDecision::AgenticDone`] so
/// the event loop records the assistant turn for chat-memory continuity.
fn spawn_agentic(
    turn: AgenticTurn,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) -> tokio::task::JoinHandle<()> {
    let AgenticTurn {
        task,
        spec,
        continue_session,
        session_id,
        fallback_model,
        project_root,
    } = turn;
    tokio::spawn(async move {
        let label = spec.label();
        let model = route_model_for_spec(&spec, fallback_model);
        // Resume the SAME chat session the conversation already uses, so the
        // agentic turn sees the prior dialogue (and leaves its work in the same
        // session for follow-up chat). Mirrors `spawn_route`'s resume wiring.
        let brain = match build_brain(&spec, continue_session, session_id, &project_root) {
            Ok(b) => b,
            Err(e) => {
                let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                    "base.init_failed",
                    &[&label, &e.to_string()],
                )));
                return;
            }
        };
        drive_agentic_stream(
            brain.as_ref(),
            &task,
            &model,
            &label,
            &project_root,
            &sink,
            &route_tx,
        )
        .await;
    })
}

/// Snapshot the working tree as a `git status --porcelain` string, run in
/// `root`. Returns the raw porcelain output (one `XY path` line per changed
/// path) so two snapshots can be diffed into the set of files THIS turn actually
/// touched, and so the live state can be injected into the agentic system
/// prompt.
///
/// **Fail-open**: a non-git directory, a missing `git`, a non-zero exit, or any
/// IO error returns `None` — the caller then SKIPS the reality enhancement
/// entirely (it must never block or break the agentic turn). This is the
/// load-bearing safety property; do not turn it into an error path.
fn git_status_porcelain(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// A compact `git diff --stat` of the working tree (unstaged changes), run in
/// `root`, used only to give the agentic system prompt a sense of what is
/// already modified. **Fail-open**: any failure returns `None` and the prompt
/// simply omits the diff-stat section.
fn git_diff_stat(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--stat"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The path token of a single `git status --porcelain` line. Porcelain v1 is
/// `XY <path>` (or `XY <old> -> <new>` for renames); we key on the FINAL path
/// (after `-> ` when present) so a rename is attributed to its new name. Returns
/// `None` for a blank line.
fn porcelain_path(line: &str) -> Option<String> {
    let trimmed = line.strip_prefix('\u{feff}').unwrap_or(line);
    if trimmed.trim().is_empty() {
        return None;
    }
    // Drop the two status columns + the single separating space (`XY `).
    let rest = trimmed.get(3..).unwrap_or("").trim();
    if rest.is_empty() {
        return None;
    }
    // A rename/copy is `old -> new`; attribute it to the new path.
    let path = rest.rsplit(" -> ").next().unwrap_or(rest).trim();
    // Porcelain quotes paths with special chars; strip the surrounding quotes
    // for display (best-effort — we de-quote, we do not un-escape).
    let path = path.trim_matches('"');
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// The set of paths that changed BETWEEN two `git status --porcelain` snapshots:
/// every path whose presence/status differs from `before` to `after`. A file
/// the base edited and then reverted (identical line in both) is correctly
/// reported as unchanged. Output is sorted for deterministic display and tests.
fn changed_files_between(before: &str, after: &str) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};
    // Map path -> full porcelain line, so a STATUS change (e.g. ` M` -> `MM`)
    // on the same path still counts as "changed this turn".
    let parse = |snap: &str| -> BTreeMap<String, String> {
        snap.lines()
            .filter_map(|l| porcelain_path(l).map(|p| (p, l.trim_end().to_string())))
            .collect()
    };
    let before = parse(before);
    let after = parse(after);
    let mut changed: BTreeSet<String> = BTreeSet::new();
    for (path, line) in &after {
        if before.get(path).map(String::as_str) != Some(line.as_str()) {
            changed.insert(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed.into_iter().collect()
}

/// Heuristic: does this base reply CLAIM it made code changes? Used to decide
/// whether to raise the "claimed-but-no-diff" warning after an agentic turn, and
/// to anchor a pure-chat reply that recites an edit it never made (a base that
/// misclassified a status question as chat). Deliberately broad and bilingual; a
/// false positive only adds an advisory note, never blocks anything.
pub(crate) fn claims_code_changes(text: &str) -> bool {
    // English change verbs.
    const EN: &[&str] = &[
        "refactor",
        "added",
        "changed",
        "edited",
        "created",
        "updated",
        "modified",
        "removed",
        "deleted",
        "implemented",
        "renamed",
        "rewrote",
        "replaced",
        "inserted",
    ];
    // Chinese change verbs (no case folding needed).
    const ZH: &[&str] = &[
        "重构",
        "新增",
        "删除",
        "修改",
        "实现",
        "修复",
        "改了",
        "改动",
        "更新",
        "增加",
        "移除",
        "重命名",
        "替换",
        "已添加",
        "已修改",
        "写入",
        "创建",
    ];
    let t = text.to_lowercase();
    if EN.iter().any(|k| t.contains(k)) {
        return true;
    }
    ZH.iter().any(|k| text.contains(k))
}

/// Build the reality-anchored fact line appended to the transcript AFTER each
/// agentic turn. Given the file set that ACTUALLY changed on disk this turn (per
/// the two git snapshots) and whether the base's reply CLAIMED changes, returns:
///
/// - `[note] 本轮无文件变更` when nothing changed,
/// - `[note] 本轮实际文件变更: a, b, …` when files changed,
/// - plus a prominent `[warn] …` warning line when the base claimed changes but
///   git shows none (likely a hallucinated / session-recited change, not a real
///   write).
///
/// (ASCII `[note]` / `[warn]` markers match the in-repo stream-note convention;
/// the governance emoji rule forbids glyph icons in `.rs` source.)
///
/// Returns `None` only when git was unavailable for EITHER snapshot
/// (`changed == None`) — the caller skips the fact line entirely (fail-open).
fn agentic_fact_line(changed: Option<&[String]>, claimed: bool) -> Option<String> {
    let changed = changed?;
    if changed.is_empty() {
        if claimed {
            Some(
                "[note] 本轮无文件变更\n[warn] 底座报告了改动,但工作区没有实际文件变更 —— \
                 可能未真正落盘或为复述,请核对 / base reported changes but the working \
                 tree is unchanged — verify before trusting"
                    .to_string(),
            )
        } else {
            Some("[note] 本轮无文件变更 / no file changes this turn".to_string())
        }
    } else {
        // Cap the listed files so a huge change set stays one readable line.
        const MAX: usize = 20;
        let shown: Vec<&str> = changed.iter().take(MAX).map(String::as_str).collect();
        let mut list = shown.join(", ");
        if changed.len() > MAX {
            list.push_str(&format!(" ... (+{})", changed.len() - MAX));
        }
        Some(format!("[note] 本轮实际文件变更: {list}"))
    }
}

/// The reality-anchored system prompt for an agentic turn. It UNLOCKS tools
/// (read/edit files, run commands — the whole point of the agentic path) and
/// injects the live git state, then hard-constrains the base to verify any
/// "what did I change" claim against the real disk/git state rather than
/// reciting unverified session intent. `status`/`diff_stat` are the live
/// snapshots (either may be `None`).
fn agentic_system_prompt(status: Option<&str>, diff_stat: Option<&str>) -> String {
    let mut p = String::from(
        "You are running inside the project's working directory with FULL tool access. \
         You MAY and SHOULD read files, edit files, and run commands to do the work — \
         do not refuse to use your tools.\n\n\
         REALITY CONTRACT (mandatory): every statement you make about WHICH FILES YOU \
         CHANGED or WHAT EDITS YOU MADE must be grounded in the real files on disk and \
         the real git state. Before claiming any change, verify it with `git diff` / \
         `git status` or by reading the file. NEVER recite intended or remembered \
         changes from earlier in the conversation as if they were already done — if you \
         did not just verify it on disk, do not claim it. If you did not actually write \
         a file this turn, say so plainly.\n",
    );
    match status {
        Some(s) if !s.trim().is_empty() => {
            p.push_str("\nCurrent `git status --porcelain` (the real, current working tree):\n");
            p.push_str(s.trim_end());
            p.push('\n');
        }
        _ => {
            p.push_str(
                "\nCurrent `git status --porcelain`: clean (no uncommitted changes right now).\n",
            );
        }
    }
    if let Some(d) = diff_stat {
        p.push_str("\nCurrent `git diff --stat`:\n");
        p.push_str(d.trim_end());
        p.push('\n');
    }
    p
}

/// Build the tools-unlocked execution request and drive the base's streaming
/// tool loop, forwarding every event to the live render pipeline and sending the
/// terminal [`RouteDecision`] when the stream ends. Split out of [`spawn_agentic`]
/// so a fake [`Runtime`] can exercise it in a unit test (the spawn wrapper only
/// adds the `tokio::spawn` + `build_brain`).
///
/// Three reality-anchoring guards wrap the raw streaming call (all **fail-open** —
/// if git is unavailable each guard silently no-ops):
///
/// 1. **Reality injection** — a `system` prompt that UNLOCKS tools and injects the
///    live `git status` so the base can't drift from the real tree, and forbids
///    reciting unverified session intent as done work.
/// 2. **Post-turn fact check** — a git snapshot before and after the turn; the
///    real changed-file set is appended to the transcript, with a prominent
///    `[warn]` line when the base CLAIMED changes the working tree does not show.
/// 3. **Truncation honesty** — a `Warning`/error stream finish carries an
///    "may be incomplete / not flushed to disk" caveat instead of reading as a
///    clean success.
async fn drive_agentic_stream(
    brain: &dyn Runtime,
    task: &str,
    model: &str,
    label: &str,
    project_root: &std::path::Path,
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) {
    // (1) Reality injection — snapshot the live git state BEFORE the turn so the
    // base is anchored to the real tree, and keep `before` for the post-turn
    // diff. Both are `Option` (fail-open: git missing -> None -> guards no-op).
    let before = git_status_porcelain(project_root);
    let diff_stat = git_diff_stat(project_root);
    let system = agentic_system_prompt(before.as_deref(), diff_stat.as_deref());

    // The execution request: the user's raw task, tools UNLOCKED, no max_tokens
    // (so the base isn't cut off mid-loop). The system prompt does NOT re-ban
    // tools — it unlocks them and only adds the reality contract.
    let request = CompletionRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: task.to_string(),
        }],
        max_tokens: None,
        temperature: None,
        system: Some(system),
    };
    // Forward every stream event straight into the existing WorkerStream
    // render pipeline (tool calls + text deltas show live). A `Warning` event is
    // also latched into `truncated` so the terminal note can flag an incomplete
    // finish (the base hit a rate limit / retry / cut-off mid-loop).
    let truncated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream_sink = Arc::clone(sink);
    let truncated_flag = Arc::clone(&truncated);
    let on_event = move |ev: umadev_runtime::StreamEvent| {
        if matches!(ev, umadev_runtime::StreamEvent::Warning { .. }) {
            truncated_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        stream_sink.emit(EngineEvent::WorkerStream { event: ev });
    };
    match brain.complete_streaming(request, &on_event).await {
        Ok(resp) => {
            // (2) Post-turn fact check — snapshot git AGAIN and diff against the
            // pre-turn snapshot to get the files THIS turn actually changed on
            // disk. Fail-open: if either snapshot is missing (non-git / git
            // unavailable), `changed` is `None` and the fact line is skipped.
            let changed = match (before.as_deref(), git_status_porcelain(project_root)) {
                (Some(b), Some(a)) => Some(changed_files_between(b, &a)),
                _ => None,
            };
            // Emit the reality-anchored fact line to the transcript: the real
            // changed-file set, plus a `[warn]` when the base CLAIMED changes the
            // working tree does not show (likely a recited / hallucinated edit).
            if let Some(line) =
                agentic_fact_line(changed.as_deref(), claims_code_changes(&resp.text))
            {
                sink.emit(EngineEvent::Note(line));
            }
            // (3) Truncation honesty — a `Warning` event mid-stream means the
            // turn likely ended early (rate limit / retry / cut-off). Append a
            // caveat to the recorded reply so a truncated turn does NOT read as a
            // clean, fully-flushed success.
            let mut reply = resp.text;
            if truncated.load(std::sync::atomic::Ordering::SeqCst) {
                if !reply.is_empty() {
                    reply.push('\n');
                }
                reply.push_str(
                    "[warn] 本轮可能未完成或未全部落盘(底座中途告警/截断),请核对实际文件状态 \
                     / turn may be incomplete or not fully written — verify the working tree",
                );
            }
            // The body already streamed live; hand the assembled text to the
            // event loop ONLY to record it as the assistant turn. An empty body
            // (the base emitted only tool calls / a side-effect) is still a clean
            // finish — send AgenticDone with what we have so `thinking` clears
            // uniformly.
            let _ = route_tx.send(RouteDecision::AgenticDone(reply));
        }
        Err(e) => {
            // Fail-open: the agentic call failed → a terminal Failed note (clears
            // `thinking`), never a stuck spinner. The chat session is untouched,
            // so the user can simply retry.
            let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                "route.failed",
                &[label, &e.to_string()],
            )));
        }
    }
}

/// Build a [`RouteTurn`] from the current app state and spawn it. The single
/// place a chat turn is dispatched — used both by the `Action::Route` key path
/// and by the queue-drain that fires the next parked turn once the current
/// route result lands. Marks `thinking` so the status animates immediately, and
/// flips `host_chat_session_active` so a host-CLI base resumes (not cold-starts)
/// the NEXT turn. Routing same-session turns is kept strictly serial: the only
/// callers fire one turn and wait for its `RouteDecision` before firing another.
fn fire_route(
    app: &mut App,
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    text: String,
) {
    let spec = app.brain_spec();
    let host_cli = matches!(spec, BrainSpec::HostCli(_));
    let continue_session = app.host_chat_session_active;
    // Pin a stable id so a host CLI (claude) resumes OUR chat session by id,
    // never "the most recent in this dir".
    let session_id = if host_cli {
        Some(app.ensure_chat_session_id())
    } else {
        None
    };
    // A re-fired queued turn starts a fresh in-flight route → animate again.
    // (The renderer reads `thinking` directly each frame, so no explicit
    // status refresh is needed here; the loop redraws on the next tick.)
    app.thinking = true;
    app.thinking_started = Some(std::time::Instant::now());
    app.last_output_at = None;
    app.tool_in_progress = false;
    spawn_route(
        RouteTurn {
            text,
            history: app.conversation_snapshot(),
            spec: spec.clone(),
            continue_session,
            session_id,
            fallback_model: app.effective_model(),
            project_root: app.project_root.clone(),
        },
        sink.clone(),
        route_tx.clone(),
    );
    // A host-CLI base persists its own session — mark it active so the NEXT
    // turn resumes instead of starting cold. HTTP / offline bases keep their
    // memory elsewhere and ignore this flag.
    if host_cli {
        app.host_chat_session_active = true;
    }
}

/// Fire the tools-enabled agentic execution call from current app state, and
/// return its `JoinHandle` so the event loop can park it in `run_task` (Ctrl-C
/// aborts it). Keeps `thinking` set — the stream feeds `WorkerStream` events,
/// which reset the stall clock just like a phase — and resumes the SAME chat
/// session as `fire_route`, so the agentic turn shares conversation memory.
/// Marks `agentic_in_flight` so Ctrl-C routes to a real task-abort instead of
/// the fire-and-forget route interrupt.
fn fire_agentic(
    app: &mut App,
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    task: String,
) -> tokio::task::JoinHandle<()> {
    let spec = app.brain_spec();
    let host_cli = matches!(spec, BrainSpec::HostCli(_));
    let continue_session = app.host_chat_session_active;
    let session_id = if host_cli {
        Some(app.ensure_chat_session_id())
    } else {
        None
    };
    // Keep the waiting state alive through the (potentially long) tool loop.
    app.thinking = true;
    app.thinking_started = Some(std::time::Instant::now());
    app.last_output_at = None;
    app.tool_in_progress = false;
    app.agentic_in_flight = true;
    let handle = spawn_agentic(
        AgenticTurn {
            task,
            spec,
            continue_session,
            session_id,
            fallback_model: app.effective_model(),
            project_root: app.project_root.clone(),
        },
        sink.clone(),
        route_tx.clone(),
    );
    if host_cli {
        app.host_chat_session_active = true;
    }
    handle
}

/// After a TERMINAL chat route outcome (`Chat` / `Failed`), fire the next turn
/// the user parked while the route was in flight, keeping same-session routing
/// serial. Returns `true` if a parked turn was dispatched.
fn drain_next_queued_chat(
    app: &mut App,
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) -> bool {
    if let Some(text) = app.take_next_queued_chat() {
        fire_route(app, sink, route_tx, text);
        true
    } else {
        false
    }
}

fn parse_route_decision(text: &str) -> Option<RouteDecision> {
    let value = parse_json_object(text)?;
    let mode = value.get("mode")?.as_str()?.trim().to_lowercase();
    match mode.as_str() {
        "run" => {
            let requirement = value
                .get("requirement")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            if requirement.is_empty() {
                None
            } else {
                Some(RouteDecision::Run(requirement.to_string()))
            }
        }
        "chat" => {
            let reply = value
                .get("reply")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            if reply.is_empty() {
                None
            } else {
                Some(RouteDecision::Chat(reply.to_string()))
            }
        }
        "agentic" => {
            let task = value
                .get("task")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            if task.is_empty() {
                None
            } else {
                Some(RouteDecision::Agentic(task.to_string()))
            }
        }
        _ => None,
    }
}

fn parse_json_object(text: &str) -> Option<serde_json::Value> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        return Some(value);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

fn route_model_for_spec(_spec: &BrainSpec, fallback_model: String) -> String {
    fallback_model
}

/// Read the model the BASE is configured to use, in the base's OWN resolution
/// order, so UmaDev can adopt it as the Agent's driving model — UmaDev owns no
/// model; the base's model IS the engine. Returns `None` when the base pins no
/// explicit model in config (it then runs on its login / server default, which
/// UmaDev does not override — see `App::effective_model`). Fail-open throughout.
#[must_use]
pub fn detect_base_model(backend_id: &str, project_root: &std::path::Path) -> Option<String> {
    let home = config::home_dir();
    match backend_id {
        // claude: --model > ANTHROPIC_MODEL > project/user .claude/settings.json.
        "claude-code" => {
            if let Ok(m) = std::env::var("ANTHROPIC_MODEL") {
                let m = m.trim();
                if !m.is_empty() {
                    return Some(m.to_string());
                }
            }
            json_top_string(&project_root.join(".claude/settings.json"), "model").or_else(|| {
                home.as_ref()
                    .and_then(|h| json_top_string(&h.join(".claude/settings.json"), "model"))
            })
        }
        // codex: project/user .codex/config.toml `model` (then `default_model`).
        "codex" => {
            let proj = project_root.join(".codex/config.toml");
            let user = home.as_ref().map(|h| h.join(".codex/config.toml"));
            ["model", "default_model"].into_iter().find_map(|k| {
                toml_top_string(&proj, k)
                    .or_else(|| user.as_ref().and_then(|u| toml_top_string(u, k)))
            })
        }
        // opencode: project/user opencode.json `model` (format provider/model).
        "opencode" => json_top_string(&project_root.join("opencode.json"), "model").or_else(|| {
            home.as_ref()
                .and_then(|h| json_top_string(&h.join(".config/opencode/opencode.json"), "model"))
        }),
        _ => None,
    }
}

/// Read the reasoning / thinking effort the BASE is configured with, so UmaDev
/// can SHOW it next to the driving model. UmaDev never overrides it — the base
/// runs at its own effort, just like its own model. `None` when the base pins no
/// explicit effort (opencode encodes effort in the model variant, so it has no
/// separate field). Fail-open throughout.
#[must_use]
pub fn detect_base_reasoning(backend_id: &str, project_root: &std::path::Path) -> Option<String> {
    let home = config::home_dir();
    match backend_id {
        // claude: settings.json `effortLevel` (project wins over user).
        "claude-code" => json_top_string(
            &project_root.join(".claude/settings.json"),
            "effortLevel",
        )
        .or_else(|| {
            home.as_ref()
                .and_then(|h| json_top_string(&h.join(".claude/settings.json"), "effortLevel"))
        }),
        // codex: config.toml `model_reasoning_effort`.
        "codex" => {
            let proj = project_root.join(".codex/config.toml");
            let user = home.as_ref().map(|h| h.join(".codex/config.toml"));
            toml_top_string(&proj, "model_reasoning_effort").or_else(|| {
                user.as_ref()
                    .and_then(|u| toml_top_string(u, "model_reasoning_effort"))
            })
        }
        // opencode: effort is baked into the model variant — no separate field.
        _ => None,
    }
}

/// Read a top-level string field from a JSON config file (fail-open `None`).
fn json_top_string(path: &std::path::Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get(key)?.as_str().map(str::to_string)
}

/// Read a root string field from a TOML config file (fail-open `None`).
fn toml_top_string(path: &std::path::Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    v.get(key)?.as_str().map(str::to_string)
}

/// Route request carrying ONLY the new user turn — for host CLIs, whose own
/// session (resumed via `continue_session`) already holds the prior context.
fn route_request_single(text: String, model: String) -> CompletionRequest {
    CompletionRequest {
        model,
        messages: vec![Message {
            role: "user".to_string(),
            content: text,
        }],
        max_tokens: Some(1024),
        temperature: Some(0.4),
        system: Some(ROUTE_SYSTEM_PROMPT.to_string()),
    }
}

fn route_request(mut history: Vec<Message>, text: String, model: String) -> CompletionRequest {
    // `history` already ends with the user's current turn (recorded by
    // `App::record_user_turn` before routing). The guard only covers the
    // defensive case of an empty transcript so the request is never message-less.
    if history.is_empty() {
        history.push(Message {
            role: "user".to_string(),
            content: text,
        });
    }
    CompletionRequest {
        model,
        messages: history,
        max_tokens: Some(1024),
        temperature: Some(0.4),
        system: Some(ROUTE_SYSTEM_PROMPT.to_string()),
    }
}

fn spawn_probe(sink: Arc<ChannelSink>) {
    tokio::spawn(async move {
        for status in umadev_host::probe_all().await {
            let (ready, detail) = match status.probe {
                umadev_host::ProbeResult::Ready { version } => (true, version),
                umadev_host::ProbeResult::NotInstalled { program } => {
                    (false, format!("`{program}` not on PATH"))
                }
                umadev_host::ProbeResult::Unhealthy { detail } => (false, detail),
            };
            sink.emit(EngineEvent::BackendProbed {
                backend_id: status.id.to_string(),
                ready,
                detail,
            });
        }
    });
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Detect whether the terminal has a light background.
///
/// Cross-platform, layered strategy (most-reliable first), mirroring how
/// `Claude Code` and `OpenCode` probe the terminal:
///
/// 1. **`$COLORFGBG`** — synchronous hint set by some terminals at launch
///    (rxvt-family, Konsole, iTerm2 with the option). rxvt convention:
///    bg ≤ 6 or 8 is dark; 7 / 9–15 are light.
/// 2. **Known-terminal allowlist** — `$TERM_PROGRAM` / `$WT_SESSION` /
///    `$COLORTERM` etc. Some terminals (Windows Terminal, Apple Terminal)
///    carry a known default or expose their theme via env vars. We only use
///    this for terminals we're confident ship a light default.
/// 3. **OSC 11 query** — send `\e]11;?\e\\`, read the terminal's actual
///    background RGB (`\e]11;rgb:RR/GG/BB\e\\`), classify by BT.709 luminance.
///    Run AFTER entering raw mode so the response isn't echoed to the screen.
///    Short timeout (200ms) so a non-responding terminal (Windows conhost,
///    dumb terminals, some SSH setups) never blocks launch.
/// 4. **Default dark** — the common case for developer terminals.
///
/// Returns `true` if light, `false` if dark or undetectable.
#[must_use]
pub fn detect_light_bg() -> bool {
    // 1. COLORFGBG synchronous hint.
    if let Some(theme) = theme_from_colorfgbg() {
        return theme;
    }

    // 2. Known-terminal allowlist (env-var based).
    if let Some(theme) = theme_from_known_terminal() {
        return theme;
    }

    // 3. OSC 11 query (must run in raw mode — see setup_terminal).
    if let Some(theme) = theme_from_osc11() {
        return theme;
    }

    // 4. Default: assume dark (the common developer setup).
    false
}

/// Parse `$COLORFGBG` ("fg;bg") via the rxvt convention.
fn theme_from_colorfgbg() -> Option<bool> {
    let fgbg = std::env::var("COLORFGBG").ok()?;
    // Format is "fg;bg" (or "fg;other;bg"). bg is the LAST field.
    let bg = fgbg.split(';').next_back()?.trim();
    let bg_num: u8 = bg.parse().ok()?;
    if bg_num > 15 {
        return None;
    }
    // 0–6 and 8 are dark ANSI colors; 7 (white) and 9–15 (bright) are light.
    Some(!(bg_num <= 6 || bg_num == 8))
}

/// Known-terminal allowlist. We only assert a theme here for terminals where
/// we're confident about the default OR where the terminal explicitly exposes
/// its current theme via an env var. Conservative: when in doubt, return None
/// and let OSC 11 decide.
fn theme_from_known_terminal() -> Option<bool> {
    // Apple Terminal exposes its background via COLORFGBG (handled above) but
    // doesn't set TERM_PROGRAM usefully. iTerm2, Ghostty, WezTerm, kitty all
    // respond to OSC 11 correctly, so we let that path handle them.
    //
    // Windows Terminal: sets WT_SESSION. Its default profile is a dark scheme,
    // but users can pick light — we still try OSC 11 first (Windows Terminal
    // 1.x responds). Only if OSC fails do we fall back here.
    if std::env::var_os("WT_SESSION").is_some() {
        // WT responds to OSC 11 on Windows 10+, so this is just a last-resort
        // default if the query timed out (older Windows / conhost).
        return None;
    }
    None
}

/// OSC 11 query: send the background-color query, read the RGB response,
/// classify by BT.709 luminance. Must run in raw mode (no echo).
fn theme_from_osc11() -> Option<bool> {
    use std::io::{Read, Write};
    use std::time::Instant;

    let mut stdout = std::io::stdout();
    // OSC 11 ? = "report background color". Terminate with BEL (\x07) which
    // more terminals accept than ST (ESC \); some respond with BEL too.
    stdout.write_all(b"\x1b]11;?\x07").ok()?;
    stdout.flush().ok()?;

    // Read the response: `\x1b]11;rgb:RRRR/GGGG/BBBB\x07` (or ESC \).
    // 200ms timeout — terminals respond in <50ms; non-responders (conhost,
    // dumb terms) time out cleanly without blocking the launch.
    let mut buf = [0u8; 64];
    let mut filled = 0;
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut stdin = std::io::stdin();
    while filled < buf.len() && Instant::now() < deadline {
        match stdin.read(&mut buf[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                filled += n;
                let s = String::from_utf8_lossy(&buf[..filled]);
                if let Some(theme) = parse_osc_bg(&s) {
                    return Some(theme);
                }
            }
        }
    }
    None
}

/// Parse an OSC 11 response (e.g. `\x1b]11;rgb:1a/1b/26\x1b\\`) and classify
/// light vs dark via ITU-R BT.709 relative luminance (same threshold Claude
/// Code uses: > 0.5 → light).
fn parse_osc_bg(s: &str) -> Option<bool> {
    /// Normalize a 1–4 digit hex channel to `[0.0, 1.0]`.
    fn norm(hex: &str) -> Option<f64> {
        let h: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let len = u32::try_from(h.len()).unwrap_or(4);
        let max = 16_u32.pow(len) - 1;
        let v = u32::from_str_radix(&h, 16).ok()?;
        Some(f64::from(v) / f64::from(max))
    }

    // Find "rgb:" then the three hex channels separated by '/'.
    let rgb_idx = s.find("rgb:")?;
    let rest = &s[rgb_idx + 4..];
    let parts: Vec<&str> = rest.split('/').take(3).collect();
    if parts.len() < 3 {
        return None;
    }
    let r = norm(parts[0])?;
    let g = norm(parts[1])?;
    let b = norm(parts[2])?;
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    Some(luminance > 0.5)
}

fn setup_terminal() -> Result<Term> {
    // Best-effort teardown for a MID-SETUP failure. If raw mode is already on
    // and a LATER step (alt screen, mouse capture, …) fails, a bare `?` would
    // return WITHOUT restoring the terminal — leaving the user's shell stuck
    // in raw/mouse-reporting mode until `reset`. So every fallible step routes
    // its error through this, which undoes whatever was switched on before
    // propagating. Errors during the undo are ignored (we're already failing).
    fn fail(e: impl Into<anyhow::Error>) -> anyhow::Error {
        let mut out = std::io::stdout();
        let _ = out.execute(DisableMouseCapture);
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(LeaveAlternateScreen);
        let _ = out.execute(crossterm::cursor::Show);
        let _ = disable_raw_mode();
        e.into()
    }

    // Enter raw mode FIRST so the OSC 11 response isn't echoed to the screen
    // (raw mode disables input echo + canonical processing — the response
    // bytes come back through stdin silently). Then probe the background
    // color, cache the result in the theme module, then enter the alt screen.
    enable_raw_mode()?;
    let is_light = detect_light_bg();
    ui::set_light_theme(is_light);

    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen).map_err(fail)?;
    // Turn on bracketed paste so multi-char bursts (clipboard paste AND CJK
    // IME commits, which most terminals deliver as a paste) arrive as one
    // atomic `Event::Paste` instead of a scrambled stream of `Char` events.
    stdout.execute(EnableBracketedPaste).map_err(fail)?;
    // Mouse capture is OFF by default: it would take over the terminal's native
    // click-drag text selection, and being able to select + copy text is the more
    // important default than wheel-scroll. The transcript still scrolls via the
    // keyboard (PageUp/PageDown, Home/End, Ctrl+Alt+U/D). A user who wants the
    // wheel to page the transcript turns it on with `/mouse` (which then issues
    // EnableMouseCapture); teardown + the panic hook DisableMouseCapture so the
    // terminal is never left in mouse-reporting mode either way.
    // Show the terminal cursor so the user sees a blinking caret in the
    // input box (positioned via frame.set_cursor_position in render_prompt).
    stdout.execute(crossterm::cursor::Show).map_err(fail)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout)).map_err(fail)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) {
    // Every step is best-effort: a failure in one (e.g. disable_raw_mode on a
    // half-closed TTY) must NOT short-circuit the rest, or the terminal could
    // be left in mouse-reporting / alt-screen mode. DisableMouseCapture in
    // particular must always run on the normal-exit path — it's the partner to
    // EnableMouseCapture in setup_terminal and the panic hook.
    let _ = disable_raw_mode();
    let _ = terminal.backend_mut().execute(DisableBracketedPaste);
    let _ = terminal.backend_mut().execute(DisableMouseCapture);
    let _ = terminal.backend_mut().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

async fn event_loop(terminal: &mut Term, app: &mut App, opts: LaunchOptions) -> Result<()> {
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // Probe in the background so the picker labels refresh as data arrives.
    spawn_probe(sink.clone());

    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    // Handle to the in-flight pipeline task, so `/cancel` can abort it.
    let mut run_task: Option<tokio::task::JoinHandle<()>> = None;
    // The director's persistent base session for the continuous run path — ONE
    // brain held across the whole TUI session so context flows across gate
    // blocks (see `spawn_continuous_block`). Always empty unless the continuous
    // path is enabled; a parked session here is what makes a `Continue` block
    // resume the SAME session rather than re-prime a fresh one.
    let session_holder: SessionHolder = Arc::new(tokio::sync::Mutex::new(None));
    // Whether the in-flight run is on the continuous path, so the `Continue`
    // (gate-approve) + auto-continue blocks resume the SAME persistent session
    // (via `spawn_continuous_block`) rather than spawning a fresh single-shot
    // `Block::Continue`. Set when a continuous `run` is dispatched; cleared on a
    // terminal outcome / cancel. Local to the loop — no extra `App` state.
    let mut continuous_run_active = false;

    loop {
        terminal.draw(|f| ui::render(f, app))?;

        tokio::select! {
            maybe_route = route_rx.recv() => {
                match maybe_route {
                    // The base chose to talk: render the reply and remember it
                    // as the assistant turn so the next message has continuity.
                    // Then fire the next turn the user parked while this route
                    // was in flight (serial — never two routes at once).
                    Some(RouteDecision::Chat(reply)) => {
                        app.record_chat_reply(reply);
                        drain_next_queued_chat(app, &sink, &route_tx);
                    }
                    // The base chose agentic work: read/inspect/edit THIS repo
                    // without entering the full pipeline. Fire the SECOND,
                    // tools-enabled streaming call — its tool calls + text show
                    // live, and it ends on the base's stream end (not the first
                    // preamble). Parked in `run_task` so Ctrl-C aborts it.
                    Some(RouteDecision::Agentic(task)) => {
                        app.record_agentic_started(&task);
                        run_task = Some(fire_agentic(app, &sink, &route_tx, task));
                    }
                    // An agentic turn finished cleanly: the body already streamed
                    // live, so we only record it as the assistant turn (chat
                    // memory), clear `thinking`, then drain the parked queue.
                    Some(RouteDecision::AgenticDone(reply)) => {
                        run_task = None;
                        app.record_agentic_done(reply);
                        drain_next_queued_chat(app, &sink, &route_tx);
                    }
                    // The route produced no usable reply. `record_route_failed`
                    // clears `thinking`; then drain the parked queue so a failed
                    // turn doesn't strand the messages typed behind it.
                    Some(RouteDecision::Failed(note)) => {
                        run_task = None;
                        app.record_route_failed(note);
                        drain_next_queued_chat(app, &sink, &route_tx);
                    }
                    // The base chose to build: note it in conversation memory,
                    // then kick off the 9-phase pipeline.
                    Some(RouteDecision::Run(requirement)) => {
                        app.record_run_started(&requirement);
                        app.prepare_worker_routed_run(&requirement);
                        // Any chat turns the user parked while routing now belong
                        // to this run — fold them into the pipeline steer queue so
                        // they fire at the first gate (instead of being dropped by
                        // the run reset). `prepare_worker_routed_run` already
                        // cleared the prior steer, so this is a clean handoff.
                        while let Some(parked) = app.take_next_queued_chat() {
                            app.queued_steer.push_back(parked);
                        }
                        let run_opts = RunOptions {
                            project_root: opts.project_root.clone(),
                            requirement,
                            slug: opts.slug.clone(),
                            model: app.effective_model(),
                            backend: app.backend.clone().unwrap_or_default(),
                            design_system: app.config.design_system.clone().unwrap_or_default(),
                            seed_template: app.config.seed_template.clone().unwrap_or_default(),
                            mode: app.effective_trust_mode(),
                            // Snapshot the strict-coverage opt-in once at the app
                            // boundary; the runner reads this, not the live env.
                            strict_coverage: umadev_agent::strict_coverage_from_env(),
                        };
                        // Continuous long-session path (opt-in): when enabled AND
                        // the brain is a host CLI, the director's `run` intent
                        // drives ONE persistent base session from research
                        // (`spawn_continuous_block`) instead of a fresh per-phase
                        // process. The gate cards / continue / completion all run
                        // off the SAME events, so the rest of the loop is unchanged.
                        // Offline / non-host brains stay on the single-shot path.
                        let host_cli = matches!(app.brain_spec(), BrainSpec::HostCli(_));
                        continuous_run_active = tui_continuous_enabled() && host_cli;
                        run_task = Some(if continuous_run_active {
                            let autonomous = continuous_autonomous(run_opts.mode);
                            spawn_continuous_block(
                                run_opts,
                                sink.clone(),
                                session_holder.clone(),
                                umadev_spec::Phase::Research,
                                autonomous,
                            )
                        } else {
                            spawn_block(run_opts, app.brain_spec(), sink.clone(), Block::Clarify)
                        });
                    }
                    None => {}
                }
            }
            maybe_event = engine_rx.recv() => {
                if let Some(ev) = maybe_event {
                    app.apply_engine(ev);
                    // P1-F: a continuous run that has reached a TERMINAL state
                    // (delivery completed, or an honest abort / hard-stop carrying
                    // the `ABORT_SENTINEL`) must drop the `continuous_run_active`
                    // flag AND close + clear the parked director session — otherwise
                    // the next free-text `run` intent would reuse a dead/settled
                    // session holder and a stale "still continuous" flag (residual
                    // state). `apply_engine` already flipped `finished` / `aborted`;
                    // we react to that here, mirroring the cancel path's cleanup.
                    if continuous_run_active && (app.finished || app.aborted) {
                        if let Ok(mut g) = session_holder.try_lock() {
                            if let Some(mut s) = g.take() {
                                let _ = s.end().await;
                            }
                        }
                        continuous_run_active = false;
                    }
                    // After processing the event, check if an auto-approve
                    // is pending (auto_approve_gates = true). If so, fire
                    // the Continue action immediately so the pipeline
                    // doesn't stall waiting for manual input.
                    if let Some(gate) = app.pending_auto_continue.take() {
                        app.active_gate = None;
                        let run_opts = current_run_options(app, &opts);
                        // A continuous run resumes the SAME parked session at the
                        // gate-anchored next phase; the single-shot path spawns a
                        // fresh `Block::Continue`.
                        run_task = Some(if continuous_run_active {
                            let autonomous = continuous_autonomous(run_opts.mode);
                            spawn_continuous_block(
                                run_opts,
                                sink.clone(),
                                session_holder.clone(),
                                continuous_resume_phase(gate),
                                autonomous,
                            )
                        } else {
                            spawn_block(
                                run_opts,
                                app.brain_spec(),
                                sink.clone(),
                                Block::Continue(gate),
                            )
                        });
                    }
                    // A message the user QUEUED mid-phase is ready to fire at
                    // this gap: re-run the producing block with it folded in as
                    // a revision (mirrors the Action::Revise path).
                    if let Some(text) = app.pending_steer.take() {
                        sink.emit(EngineEvent::Note(format!("queued steer: {text}")));
                        let gate = app.active_gate;
                        app.active_gate = None;
                        // P1-D: a continuous run must feed the steer back into the
                        // SAME held director session (re-driving the producing block
                        // on the continuous engine) — NOT spawn a single-shot block,
                        // which would orphan the held session (leaked, never
                        // `end()`-ed) and silently swap to the per-phase re-feed.
                        run_task = Some(if continuous_run_active {
                            let mut run_opts = current_run_options(app, &opts);
                            run_opts.requirement =
                                format!("{}\n\n## Revision request\n{text}", app.requirement);
                            let autonomous = continuous_autonomous(run_opts.mode);
                            let start_after =
                                continuous_revise_phase(gate.unwrap_or(Gate::DocsConfirm));
                            spawn_continuous_block(
                                run_opts,
                                sink.clone(),
                                session_holder.clone(),
                                start_after,
                                autonomous,
                            )
                        } else {
                            let mut run_opts = current_run_options(app, &opts);
                            run_opts.requirement =
                                format!("{}\n\n## Revision request\n{text}", app.requirement);
                            let block = match gate {
                                Some(Gate::PreviewConfirm) => Block::Continue(Gate::DocsConfirm),
                                Some(Gate::ClarifyGate) => Block::Clarify,
                                _ => Block::Initial,
                            };
                            spawn_block(run_opts, app.brain_spec(), sink.clone(), block)
                        });
                    }
                }
            }
            maybe_key = keys.next() => {
                if let Some(Ok(Event::Resize(..))) = &maybe_key {
                    // Resize: do nothing but fall through to the loop top, which
                    // redraws the whole frame at the new size. This makes a drag-
                    // resize repaint immediately instead of tearing until the next
                    // tick / keypress.
                } else if let Some(Ok(Event::Mouse(me))) = &maybe_key {
                    // Wheel → transcript scrollback (~3 rows per notch, the usual
                    // terminal step). Gated by `/mouse`; when off the wheel is
                    // ignored so the terminal's native selection/copy works. Only
                    // meaningful on the chat screen.
                    if app.mouse_scroll && matches!(app.mode, crate::app::AppMode::Chat) {
                        match me.kind {
                            MouseEventKind::ScrollUp => app.transcript_scroll_up(3),
                            MouseEventKind::ScrollDown => app.transcript_scroll_down(3),
                            _ => {}
                        }
                    }
                } else if let Some(Ok(Event::Paste(pasted))) = &maybe_key {
                    // Bracketed paste (and CJK IME commits, which most terminals
                    // deliver as a paste burst): insert the text atomically at the
                    // cursor instead of letting it arrive as a scrambled stream of
                    // raw `Char` events. Without this the buffer and the rendered
                    // cursor desync — the reported "打字乱串 / 输入框乱跳".
                    app.insert_str_at_cursor(pasted);
                } else if let Some(Ok(Event::Key(key))) = maybe_key {
                    // Accept Press AND Repeat. On terminals that negotiate the
                    // kitty / enhanced-keyboard protocol (Ghostty, recent iTerm2,
                    // WezTerm — or a base CLI like opencode that left the protocol
                    // enabled on the shared TTY), a held / fast-repeated key arrives
                    // as `Repeat`, not `Press`. Filtering for `Press` only silently
                    // DROPPED those keystrokes → missing / out-of-order characters.
                    // `Release` is still ignored so every key fires exactly once.
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        match app.apply_key_with_mods(key.code, key.modifiers) {
                            Action::Quit => break,
                            Action::None | Action::BackendChanged => {
                                // BackendChanged only affects later spawns;
                                // no immediate side-effect on running tasks.
                            }
                            Action::Reconfigure => {
                                // Re-opened the first-run guide — re-probe the
                                // host CLIs so their ready-state is current.
                                spawn_probe(sink.clone());
                            }
                            Action::Continue(gate) => {
                                let run_opts = current_run_options(app, &opts);
                                // Continuous run: resume the parked session at the
                                // next gate-anchored phase. Single-shot: fresh
                                // `Block::Continue`.
                                run_task = Some(if continuous_run_active {
                                    let autonomous = continuous_autonomous(run_opts.mode);
                                    spawn_continuous_block(
                                        run_opts,
                                        sink.clone(),
                                        session_holder.clone(),
                                        continuous_resume_phase(gate),
                                        autonomous,
                                    )
                                } else {
                                    spawn_block(
                                        run_opts,
                                        app.brain_spec(),
                                        sink.clone(),
                                        Block::Continue(gate),
                                    )
                                });
                            }
                            Action::Cancel => {
                                if let Some(h) = run_task.take() {
                                    h.abort();
                                }
                                // A continuous run was just cancelled: close + drop
                                // the parked director session so the NEXT run opens
                                // a fresh brain (an aborted mid-turn session may be
                                // wedged). Best-effort `end()` under a try_lock so
                                // cancel never blocks; clear the active flag.
                                if continuous_run_active {
                                    if let Ok(mut g) = session_holder.try_lock() {
                                        if let Some(mut s) = g.take() {
                                            let _ = s.end().await;
                                        }
                                    }
                                    continuous_run_active = false;
                                }
                                // Drain any events the aborted task already
                                // queued (e.g. a buffered PipelineStarted /
                                // GateOpened) so they can't resurrect run state
                                // after the reset below.
                                while engine_rx.try_recv().is_ok() {}
                                // Same for a route decision the aborted agentic
                                // turn already emitted: a late `AgenticDone` /
                                // `Failed` waiting in `route_rx` would otherwise
                                // be picked up next tick and append a stale reply
                                // AFTER the cancel reset.
                                while route_rx.try_recv().is_ok() {}
                                app.cancel_run();
                            }
                            Action::StartRun(req) => {
                                let run_opts = RunOptions {
                                    project_root: opts.project_root.clone(),
                                    requirement: req,
                                    slug: opts.slug.clone(),
                                    model: app.effective_model(),
                                    backend: app.backend.clone().unwrap_or_default(),
                                    design_system: app.config.design_system.clone().unwrap_or_default(),
                                    seed_template: app.config.seed_template.clone().unwrap_or_default(),
                                    mode: app.effective_trust_mode(),
                                    // Snapshot the strict-coverage opt-in once at
                                    // the app boundary; the runner reads this, not
                                    // the live env (which races in parallel).
                                    strict_coverage: umadev_agent::strict_coverage_from_env(),
                                };
                                // P1-E: an explicit `/run` must drive the SAME engine
                                // a direct-input run does — when continuous is on and
                                // the brain is a host CLI, the director run goes
                                // through the persistent session (`spawn_continuous_block`),
                                // NOT a single-shot `Block::Clarify`. Otherwise the
                                // same intent typed two ways (chat vs `/run`) would
                                // run two different engines with two different gate
                                // sets. Offline / non-host brains stay single-shot.
                                let host_cli = matches!(app.brain_spec(), BrainSpec::HostCli(_));
                                continuous_run_active = tui_continuous_enabled() && host_cli;
                                run_task = Some(if continuous_run_active {
                                    let autonomous = continuous_autonomous(run_opts.mode);
                                    spawn_continuous_block(
                                        run_opts,
                                        sink.clone(),
                                        session_holder.clone(),
                                        umadev_spec::Phase::Research,
                                        autonomous,
                                    )
                                } else {
                                    spawn_block(
                                        run_opts,
                                        app.brain_spec(),
                                        sink.clone(),
                                        Block::Clarify,
                                    )
                                });
                            }
                            Action::StartQuick(task) => {
                                // Lightweight fast track — same RunOptions as a
                                // normal start, but driven through the lean
                                // single-shot Light block (no gates). P1-E note:
                                // `/quick` deliberately stays on the single-shot
                                // lean engine on BOTH invocation forms (there is only
                                // one `/quick`), so it is already self-consistent —
                                // the divergence P1-E fixes is `/run` vs direct-input
                                // general runs, which now share the continuous engine.
                                // The continuous path classifies via `planner::plan`
                                // (not `plan_light`), so routing `/quick` through it
                                // could silently run the FULL pipeline and break the
                                // forced-lean promise; we keep the forced-Light block.
                                let run_opts = RunOptions {
                                    project_root: opts.project_root.clone(),
                                    requirement: task,
                                    slug: opts.slug.clone(),
                                    model: app.effective_model(),
                                    backend: app.backend.clone().unwrap_or_default(),
                                    design_system: app.config.design_system.clone().unwrap_or_default(),
                                    seed_template: app.config.seed_template.clone().unwrap_or_default(),
                                    mode: app.effective_trust_mode(),
                                    // Snapshot the strict-coverage opt-in once at
                                    // the app boundary; the runner reads this, not
                                    // the live env (which races in parallel).
                                    strict_coverage: umadev_agent::strict_coverage_from_env(),
                                };
                                run_task = Some(spawn_block(
                                    run_opts,
                                    app.brain_spec(),
                                    sink.clone(),
                                    Block::Light,
                                ));
                            }
                            Action::RedoPhase(phase) => {
                                // Re-run a single phase with the prior run's
                                // context (current_run_options carries the
                                // persisted requirement / slug / backend).
                                let run_opts = current_run_options(app, &opts);
                                run_task = Some(spawn_block(
                                    run_opts,
                                    app.brain_spec(),
                                    sink.clone(),
                                    Block::Redo(phase),
                                ));
                            }
                            Action::Route(text) => {
                                fire_route(app, &sink, &route_tx, text);
                            }
                            Action::Revise(text) => {
                                // Re-run the block that PRODUCED the current
                                // gate, with the revision feedback folded into
                                // the requirement so the worker actually
                                // incorporates it. Branch on the active gate:
                                //   - docs_confirm  → re-run Initial (regen docs)
                                //   - preview_confirm→ re-run Continue(DocsConfirm)
                                //     (regen spec → frontend), NOT the docs.
                                // Re-running Initial unconditionally was a bug:
                                // a UI revision at preview_confirm would have
                                // thrown away the approved docs and regenerated
                                // them instead of redoing the frontend.
                                sink.emit(EngineEvent::Note(format!("user revision: {text}")));
                                let revised_requirement = format!(
                                    "{}\n\n## Revision request\n{text}",
                                    app.requirement
                                );
                                let run_opts = RunOptions {
                                    project_root: opts.project_root.clone(),
                                    requirement: revised_requirement,
                                    slug: opts.slug.clone(),
                                    model: app.effective_model(),
                                    backend: app.backend.clone().unwrap_or_default(),
                                    design_system: app.config.design_system.clone().unwrap_or_default(),
                                    seed_template: app.config.seed_template.clone().unwrap_or_default(),
                                    mode: app.effective_trust_mode(),
                                    // Snapshot the strict-coverage opt-in once at
                                    // the app boundary; the runner reads this, not
                                    // the live env (which races in parallel).
                                    strict_coverage: umadev_agent::strict_coverage_from_env(),
                                };
                                let gate = app.active_gate;
                                // The producing block is re-running, so the gate
                                // is no longer active — clear it so the status
                                // bar / prompt don't keep showing the old gate
                                // (and its timers) during the rework.
                                app.active_gate = None;
                                // P1-D: on a continuous run, feed the revision back
                                // into the SAME held director session by re-driving
                                // the producing block on the continuous engine —
                                // NOT a single-shot `spawn_block`, which would orphan
                                // the held session (leaked, never `end()`-ed) and
                                // silently swap to the per-phase re-feed engine.
                                run_task = Some(if continuous_run_active {
                                    let autonomous = continuous_autonomous(run_opts.mode);
                                    let start_after =
                                        continuous_revise_phase(gate.unwrap_or(Gate::DocsConfirm));
                                    spawn_continuous_block(
                                        run_opts,
                                        sink.clone(),
                                        session_holder.clone(),
                                        start_after,
                                        autonomous,
                                    )
                                } else {
                                    let block = match gate {
                                        Some(Gate::PreviewConfirm) => {
                                            Block::Continue(Gate::DocsConfirm)
                                        }
                                        // A revise AT the clarify gate re-asks the
                                        // clarifying questions with the new info —
                                        // NOT a jump straight to research/docs
                                        // (Block::Initial skips clarify entirely).
                                        Some(Gate::ClarifyGate) => Block::Clarify,
                                        // docs_confirm or unknown → regenerate docs
                                        _ => Block::Initial,
                                    };
                                    spawn_block(run_opts, app.brain_spec(), sink.clone(), block)
                                });
                            }
                            Action::StartPreview { url, command } => {
                                let (dir, prog, args) = parse_run_command(&command, &opts.project_root);
                                let mut cmd = tokio::process::Command::new(prog);
                                cmd.args(&args)
                                    .current_dir(&dir)
                                    .stdin(std::process::Stdio::null())
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .kill_on_drop(true);
                                // Port-conflict guard: if the port is already bound
                                // (the user's own Vite/Next/Express), DON'T spawn a
                                // second server — it would either fail or bind a
                                // different port while we open the wrong URL. Open
                                // what's already running instead.
                                if port_is_free(&url) {
                                    match cmd.spawn() {
                                        Ok(child) => {
                                            if let Ok(mut g) = app.preview_server.lock() {
                                                *g = Some(child);
                                            }
                                            sink.emit(EngineEvent::Note(
                                                umadev_i18n::tl("preview.dev_starting").into(),
                                            ));
                                            let url2 = url.clone();
                                            let sink3 = sink.clone();
                                            tokio::spawn(async move {
                                                let up = wait_for_port(
                                                    &url2,
                                                    std::time::Duration::from_secs(15),
                                                )
                                                .await;
                                                if up {
                                                    let _ = open_url(&url2);
                                                    sink3.emit(EngineEvent::Note(
                                                        umadev_i18n::tlf(
                                                            "preview.dev_ready",
                                                            &[&url2],
                                                        ),
                                                    ));
                                                } else {
                                                    sink3.emit(EngineEvent::Note(
                                                        umadev_i18n::tlf(
                                                            "preview.dev_not_ready",
                                                            &[&url2],
                                                        ),
                                                    ));
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                                                "preview.dev_spawn_failed",
                                                &[&command, &e.to_string(), &url],
                                            )));
                                        }
                                    }
                                } else {
                                    let _ = open_url(&url);
                                    sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                                        "preview.port_busy",
                                        &[&url],
                                    )));
                                }
                            }
                            Action::RunDeploy { command } => {
                                // Deploy runs in a background task: the deploy
                                // adapter (fail-open) runs the command in the
                                // workspace, captures the live URL + log tail
                                // into a structured DeployProof, and writes
                                // `.umadev/audit/deploy-proof.json` so the deploy
                                // is folded into the next proof-pack. We surface
                                // success/failure + the live URL to the user.
                                let sink2 = sink.clone();
                                let root = opts.project_root.clone();
                                tokio::spawn(async move {
                                    sink2.emit(EngineEvent::Note(umadev_i18n::tlf(
                                        "deploy.running",
                                        &[&command],
                                    )));
                                    let login_hint = umadev_i18n::tl("deploy.login_hint");
                                    // stdin = /dev/null inside run_deploy: the TUI
                                    // owns the real terminal, so a deploy CLI that
                                    // wants an interactive login must FAIL FAST on
                                    // EOF rather than hang invisibly behind the
                                    // alt-screen. A timeout is the final backstop.
                                    let proof =
                                        umadev_agent::run_deploy(&root, Some(&command)).await;
                                    match &proof.status {
                                        umadev_agent::DeployStatus::Deployed => {
                                            let addr = proof.url.clone().unwrap_or_else(|| {
                                                umadev_i18n::tl("deploy.done_no_url").into()
                                            });
                                            sink2.emit(EngineEvent::Note(umadev_i18n::tlf(
                                                "deploy.done",
                                                &[&addr],
                                            )));
                                        }
                                        umadev_agent::DeployStatus::NotDeployed(reason) => {
                                            let exit = proof
                                                .exit_code
                                                .map_or_else(|| "-".to_string(), |c| c.to_string());
                                            sink2.emit(EngineEvent::Note(umadev_i18n::tlf(
                                                "deploy.failed",
                                                &[&exit, reason, login_hint],
                                            )));
                                        }
                                    }
                                    // Persist the proof (fail-open: a write error
                                    // never blocks — just no proof-pack capture).
                                    if let Ok(path) =
                                        umadev_agent::write_deploy_proof(&root, &proof)
                                    {
                                        sink2.emit(EngineEvent::Note(umadev_i18n::tlf(
                                            "deploy.proof_written",
                                            &[&path.display().to_string()],
                                        )));
                                    }
                                });
                            }
                            Action::SetMouseCapture(on) => {
                                // `/mouse` toggle: actually flip mouse capture on
                                // the LIVE terminal. ON re-enables the wheel→scroll
                                // capture; OFF issues DisableMouseCapture so the
                                // terminal's native click-drag text selection works
                                // again — what the app message promised. Fail-open:
                                // a write error is ignored, never blocking the loop.
                                let backend = terminal.backend_mut();
                                let _ = if on {
                                    backend.execute(EnableMouseCapture)
                                } else {
                                    backend.execute(DisableMouseCapture)
                                };
                            }
                        }
                    }
                }
            }
            _ = tick.tick() => app.tick(),
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn current_run_options(app: &App, opts: &LaunchOptions) -> RunOptions {
    RunOptions {
        project_root: opts.project_root.clone(),
        requirement: app.requirement.clone(),
        slug: opts.slug.clone(),
        model: app.effective_model(),
        backend: app.backend.clone().unwrap_or_default(),
        design_system: app.config.design_system.clone().unwrap_or_default(),
        seed_template: app.config.seed_template.clone().unwrap_or_default(),
        mode: app.effective_trust_mode(),
        // Snapshot the strict-coverage opt-in once at the app boundary; the runner
        // reads this captured flag, never the live env (which races in parallel).
        strict_coverage: umadev_agent::strict_coverage_from_env(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> LaunchOptions {
        LaunchOptions {
            project_root: std::env::temp_dir(),
            slug: "demo".into(),
            model: "claude-sonnet-4-6".into(),
        }
    }

    #[test]
    fn route_request_asks_base_to_classify_without_workspace_mutation() {
        // Empty history → the guard seeds a single user message from `text`.
        let request = route_request(Vec::new(), "你好".to_string(), "test-model".to_string());

        assert_eq!(request.model, "test-model");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(request.messages[0].content, "你好");
        let system = request.system.unwrap();
        assert!(system.contains("brain behind UmaDev"));
        assert!(system.contains("conversation so far"));
        assert!(system.contains("edit files"));
        assert!(system.contains("\"mode\":\"run\""));
    }

    #[test]
    fn detect_base_model_reads_each_base_config() {
        // The base's OWN model is read from its own config, in the base's order.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        std::fs::write(root.join(".codex/config.toml"), "model = \"gpt-5.5\"\n").unwrap();
        assert_eq!(detect_base_model("codex", root).as_deref(), Some("gpt-5.5"));
        std::fs::write(root.join("opencode.json"), "{\"model\":\"zhipuai/glm-5\"}").unwrap();
        assert_eq!(
            detect_base_model("opencode", root).as_deref(),
            Some("zhipuai/glm-5")
        );
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            "{\"model\":\"claude-opus-4-8\"}",
        )
        .unwrap();
        if std::env::var("ANTHROPIC_MODEL").is_err() {
            assert_eq!(
                detect_base_model("claude-code", root).as_deref(),
                Some("claude-opus-4-8")
            );
        }
        // Unknown / offline base pins nothing -> base default (None).
        assert_eq!(detect_base_model("offline", root), None);
    }

    #[test]
    fn detect_base_reasoning_reads_each_base_config() {
        // The base's reasoning/thinking effort is read from its own config too.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        std::fs::write(
            root.join(".codex/config.toml"),
            "model_reasoning_effort = \"high\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_base_reasoning("codex", root).as_deref(),
            Some("high")
        );
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            "{\"effortLevel\":\"xhigh\"}",
        )
        .unwrap();
        assert_eq!(
            detect_base_reasoning("claude-code", root).as_deref(),
            Some("xhigh")
        );
        // opencode encodes effort in the model variant -> no separate field.
        assert_eq!(detect_base_reasoning("opencode", root), None);
        assert_eq!(detect_base_reasoning("offline", root), None);
    }

    #[test]
    fn route_request_preserves_conversation_history() {
        // A real routed turn passes the full transcript (which already ends
        // with the current user message); `text` is only a fallback and must
        // NOT be appended on top of a non-empty history.
        let history = vec![
            Message {
                role: "user".to_string(),
                content: "你好".to_string(),
            },
            Message {
                role: "assistant".to_string(),
                content: "你好,我是底座".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: "我刚才说了什么?".to_string(),
            },
        ];
        let request = route_request(history, "ignored-fallback".to_string(), "m".to_string());

        assert_eq!(request.messages.len(), 3);
        assert_eq!(request.messages[0].content, "你好");
        assert_eq!(request.messages[1].role, "assistant");
        assert_eq!(request.messages[2].content, "我刚才说了什么?");
        assert!(request
            .messages
            .iter()
            .all(|m| m.content != "ignored-fallback"));
    }

    #[test]
    fn route_model_uses_launch_model_for_host_cli() {
        let spec = BrainSpec::HostCli("codex".to_string());

        assert_eq!(
            route_model_for_spec(&spec, "fallback-model".to_string()),
            "fallback-model"
        );
    }

    #[tokio::test]
    async fn spawn_route_offline_emits_local_fallback_for_chat() {
        let (sink, mut rx) = ChannelSink::new();
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_route(
            RouteTurn {
                text: "你好".to_string(),
                history: Vec::new(),
                spec: BrainSpec::Offline,
                continue_session: false,
                session_id: None,
                fallback_model: "fallback-model".to_string(),
                project_root: std::path::PathBuf::from("."),
            },
            std::sync::Arc::new(sink),
            route_tx,
        );

        // Offline chat now flows through the route channel as a Chat decision
        // so the event loop records it into conversation memory uniformly.
        let route = tokio::time::timeout(std::time::Duration::from_secs(2), route_rx.recv())
            .await
            .expect("offline chat task should route")
            .expect("route channel should stay open until event");
        match route {
            RouteDecision::Chat(body) => assert!(body.contains("UmaDev")),
            other @ (RouteDecision::Run(_)
            | RouteDecision::Agentic(_)
            | RouteDecision::AgenticDone(_)
            | RouteDecision::Failed(_)) => {
                panic!("expected local chat fallback, got {other:?}")
            }
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn spawn_route_offline_routes_requirements_to_pipeline() {
        let (sink, mut rx) = ChannelSink::new();
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_route(
            RouteTurn {
                text: "build a login app".to_string(),
                history: Vec::new(),
                spec: BrainSpec::Offline,
                continue_session: false,
                session_id: None,
                fallback_model: "fallback-model".to_string(),
                project_root: std::path::PathBuf::from("."),
            },
            std::sync::Arc::new(sink),
            route_tx,
        );

        let route = tokio::time::timeout(std::time::Duration::from_secs(2), route_rx.recv())
            .await
            .expect("offline requirement should route")
            .expect("route channel should stay open until event");
        assert_eq!(route, RouteDecision::Run("build a login app".to_string()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn parse_route_decision_reads_chat_json() {
        assert_eq!(
            parse_route_decision(r#"{"mode":"chat","reply":"你好，有什么想聊的？"}"#),
            Some(RouteDecision::Chat("你好，有什么想聊的？".to_string()))
        );
    }

    #[test]
    fn parse_route_decision_reads_run_json_inside_text() {
        assert_eq!(
            parse_route_decision(
                "```json\n{\"mode\":\"run\",\"requirement\":\"做一个登录系统\"}\n```"
            ),
            Some(RouteDecision::Run("做一个登录系统".to_string()))
        );
    }

    #[test]
    fn parse_route_decision_reads_agentic_json() {
        // The new third mode: "look at THIS repo's code" (review/diagnose/explain
        // /small-fix) routes to Agentic, carrying the cleaned task.
        assert_eq!(
            parse_route_decision(r#"{"mode":"agentic","task":"审查 app.rs 看会不会出 bug"}"#),
            Some(RouteDecision::Agentic(
                "审查 app.rs 看会不会出 bug".to_string()
            ))
        );
        // An agentic verdict with an empty task is not usable → None (falls back
        // to a non-JSON / chat path upstream).
        assert_eq!(
            parse_route_decision(r#"{"mode":"agentic","task":""}"#),
            None
        );
    }

    #[test]
    fn route_prompt_offers_all_three_modes() {
        // The classifier prompt must teach the base the three-way split so it can
        // ever emit `agentic` (the W3-b fix). Lock the mode tokens in the prompt.
        assert!(ROUTE_SYSTEM_PROMPT.contains("\"mode\":\"chat\""));
        assert!(ROUTE_SYSTEM_PROMPT.contains("\"mode\":\"agentic\""));
        assert!(ROUTE_SYSTEM_PROMPT.contains("\"mode\":\"run\""));
        // L2 anti-confabulation: "what did you change / did you do X / current
        // state" questions must route to agentic (verified against git), never
        // chat (which would answer from memory and fabricate changes).
        assert!(ROUTE_SYSTEM_PROMPT.contains("what did you just change"));
        assert!(ROUTE_SYSTEM_PROMPT.contains("fabricates changes that may not exist"));
    }

    /// A fake runtime that records which entry point the agentic path used.
    /// `complete` must NEVER be called by the agentic path (it would be a
    /// one-shot, non-streaming, preamble-only turn — the exact bug being fixed);
    /// `complete_streaming` is the contract. When `fail` is set, the streaming
    /// call errors so the fail-open downgrade can be asserted.
    struct StreamSpy {
        complete_calls: Arc<std::sync::atomic::AtomicUsize>,
        streaming_calls: Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Runtime for StreamSpy {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            self.complete_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(umadev_runtime::CompletionResponse {
                text: "ONE-SHOT".to_string(),
                id: "spy".to_string(),
                model: "spy".to_string(),
                usage: umadev_runtime::Usage::default(),
            })
        }
        async fn complete_streaming(
            &self,
            _req: CompletionRequest,
            on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            self.streaming_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(umadev_runtime::RuntimeError::HostProcess(
                    "boom".to_string(),
                ));
            }
            // Emit a tool call + a text delta so the live render path is exercised.
            on_event(umadev_runtime::StreamEvent::ToolUse {
                name: "Read".to_string(),
                detail: "app.rs".to_string(),
            });
            on_event(umadev_runtime::StreamEvent::Text {
                delta: "no bug found".to_string(),
            });
            Ok(umadev_runtime::CompletionResponse {
                text: "no bug found".to_string(),
                id: "spy".to_string(),
                model: "spy".to_string(),
                usage: umadev_runtime::Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn agentic_path_uses_streaming_not_one_shot() {
        // The whole point of the W3-b fix: an agentic turn must drive the base's
        // STREAMING tool loop — never the one-shot `complete` (which would stop
        // at the first preamble without reading the code).
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let streaming_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spy = StreamSpy {
            complete_calls: Arc::clone(&complete_calls),
            streaming_calls: Arc::clone(&streaming_calls),
            fail: false,
        };

        // A non-git temp dir → the reality guards fail-open (no fact line),
        // keeping this test focused on the streaming-vs-one-shot contract.
        let tmp = tempfile::TempDir::new().unwrap();
        drive_agentic_stream(
            &spy,
            "审一下",
            "m",
            "claude-code",
            tmp.path(),
            &sink,
            &route_tx,
        )
        .await;

        assert_eq!(
            complete_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "agentic must NOT use one-shot complete"
        );
        assert_eq!(
            streaming_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "agentic must drive complete_streaming"
        );
        // The stream events reached the live render pipeline as WorkerStream.
        let mut saw_tool = false;
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::ToolUse { .. },
            } = ev
            {
                saw_tool = true;
            }
        }
        assert!(saw_tool, "tool calls must stream live as WorkerStream");
        // The terminal outcome records the assistant text for chat memory.
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone(text)) => assert_eq!(text, "no bug found"),
            other => panic!("expected AgenticDone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agentic_failure_fails_open_to_downgrade() {
        // Fail-open: a streaming error must downgrade to a terminal `Failed`
        // note (which clears `thinking` upstream), never hang or panic.
        let (sink, _engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let spy = StreamSpy {
            complete_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            streaming_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail: true,
        };

        let tmp = tempfile::TempDir::new().unwrap();
        drive_agentic_stream(
            &spy,
            "审一下",
            "m",
            "claude-code",
            tmp.path(),
            &sink,
            &route_tx,
        )
        .await;

        match route_rx.try_recv() {
            Ok(RouteDecision::Failed(note)) => {
                assert!(note.contains("boom") || !note.is_empty());
            }
            other => panic!("expected fail-open Failed downgrade, got {other:?}"),
        }
    }

    // ---- agentic reality-anchoring (hallucinated-change defence) -----------
    // (these tests use Atomic/streaming concurrency primitives below.)

    #[test]
    fn system_prompt_injects_git_state_and_unlocks_tools() {
        // The reality-injection prompt must keep tools UNLOCKED — never re-add
        // the chat-route tool ban — and embed the live git status plus a
        // no-recitation contract.
        let status = concat!(" M crates/umadev-tui/src/lib.rs\n", "?? new.rs\n");
        let p = agentic_system_prompt(Some(status), Some("1 file changed"));
        // Tools stay unlocked (the whole point of the agentic path).
        assert!(p.contains("FULL tool access"));
        assert!(p.to_lowercase().contains("edit files"));
        // The real git state is injected verbatim.
        assert!(p.contains("crates/umadev-tui/src/lib.rs"));
        assert!(p.contains("git status --porcelain"));
        assert!(p.contains("1 file changed"));
        // The anti-recitation reality contract is present.
        assert!(p.contains("REALITY CONTRACT"));
        assert!(p.to_lowercase().contains("git diff"));
    }

    #[test]
    fn changed_files_between_diffs_two_snapshots() {
        // A file newly appearing, a file whose status changed, and a file that
        // disappeared all count; an identical line in both is unchanged.
        let before = concat!(" M a.rs\n", "?? keep.rs\n");
        let after = concat!(" M a.rs\n", "MM a.rs2\n", "?? new.rs\n");
        // a.rs identical -> not changed; a.rs2 new; new.rs new; keep.rs vanished.
        let changed = changed_files_between(before, after);
        assert_eq!(changed, vec!["a.rs2", "keep.rs", "new.rs"]);
        // Rename: attributed to the new path.
        let renamed = changed_files_between("", "R  old.rs -> new2.rs\n");
        assert_eq!(renamed, vec!["new2.rs"]);
        // Identical snapshots -> nothing changed.
        assert!(changed_files_between(before, before).is_empty());
    }

    #[test]
    fn fact_line_warns_on_claimed_but_absent_change() {
        // No files changed but the reply CLAIMS work -> the loud warning fires.
        let line = agentic_fact_line(Some(&[]), true).unwrap();
        assert!(line.contains("[warn]"));
        assert!(line.contains("没有实际文件变更") || line.contains("unchanged"));
        // No files changed and no claim -> a calm note, no warning.
        let calm = agentic_fact_line(Some(&[]), false).unwrap();
        assert!(calm.contains("无文件变更"));
        assert!(!calm.contains("[warn]"));
    }

    #[test]
    fn fact_line_lists_real_changes() {
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let line = agentic_fact_line(Some(&files), true).unwrap();
        // Real changes present -> list them, NEVER warn (the claim is backed).
        assert!(line.contains("src/a.rs"));
        assert!(line.contains("src/b.rs"));
        assert!(!line.contains("[warn]"));
    }

    #[test]
    fn fact_line_fails_open_when_git_unavailable() {
        // changed == None models git being unavailable -> no fact line at all,
        // even when the reply loudly claims changes. The enhancement must never
        // fabricate a verdict it cannot back.
        assert!(agentic_fact_line(None, true).is_none());
        assert!(agentic_fact_line(None, false).is_none());
    }

    #[test]
    fn claims_heuristic_spots_change_language_bilingually() {
        assert!(claims_code_changes(
            "I refactored the parser and added a test"
        ));
        assert!(claims_code_changes("已修改 app.rs 并新增了一个函数"));
        assert!(claims_code_changes("Created src/new.rs"));
        // A pure read/answer with no change verb does not trip the heuristic.
        assert!(!claims_code_changes("这段代码看起来没有问题,逻辑正确"));
        assert!(!claims_code_changes(
            "The function returns the sum; nothing to do."
        ));
    }

    /// A runtime spy that, before finishing, runs a caller-supplied side effect
    /// against the real working tree (e.g. writes a file) and returns a fixed
    /// reply — so the post-turn git fact check can be exercised end to end. Set
    /// `warn` to emit a `Warning` event (truncation-honesty path).
    struct EffectSpy {
        reply: String,
        warn: bool,
        effect: Box<dyn Fn() + Send + Sync>,
    }

    #[async_trait::async_trait]
    impl Runtime for EffectSpy {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            unreachable!("agentic path must stream")
        }
        async fn complete_streaming(
            &self,
            _req: CompletionRequest,
            on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            if self.warn {
                on_event(umadev_runtime::StreamEvent::Warning {
                    message: "rate limited, partial".to_string(),
                });
            }
            // Mutate the working tree mid-turn so the post-turn snapshot differs.
            (self.effect)();
            Ok(umadev_runtime::CompletionResponse {
                text: self.reply.clone(),
                id: "spy".to_string(),
                model: "spy".to_string(),
                usage: umadev_runtime::Usage::default(),
            })
        }
    }

    /// Initialise a throwaway git repo and return its temp dir.
    fn init_git_repo() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap();
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t.t"]);
        run(&["config", "user.name", "t"]);
        tmp
    }

    #[tokio::test]
    async fn agentic_fact_check_lists_real_file_change() {
        // A real write inside a git repo -> the post-turn note lists the file
        // and does NOT warn (the change is backed by the working tree).
        let tmp = init_git_repo();
        let path = tmp.path().to_path_buf();
        let target = path.join("touched.rs");
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let spy = EffectSpy {
            reply: "I created touched.rs".to_string(),
            warn: false,
            effect: Box::new(move || std::fs::write(&target, "fn x").unwrap()),
        };
        drive_agentic_stream(&spy, "do it", "m", "claude-code", &path, &sink, &route_tx).await;

        let mut fact = None;
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains("文件变更") || n.contains("touched.rs") {
                    fact = Some(n);
                }
            }
        }
        let fact = fact.expect("a fact line must be emitted for a real change");
        assert!(fact.contains("touched.rs"), "must name the changed file");
        assert!(!fact.contains("[warn]"), "a real change must not warn");
    }

    #[tokio::test]
    async fn agentic_fact_check_warns_on_claimed_phantom_change() {
        // The core bug: the base CLAIMS a change but the working tree is
        // untouched -> the loud warning must fire so the user never trusts a
        // phantom edit.
        let tmp = init_git_repo();
        let path = tmp.path().to_path_buf();
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let spy = EffectSpy {
            reply: "我已经重构了 app.rs 并新增了三个函数".to_string(),
            warn: false,
            effect: Box::new(|| ()),
        };
        drive_agentic_stream(
            &spy,
            "重构一下",
            "m",
            "claude-code",
            &path,
            &sink,
            &route_tx,
        )
        .await;

        let mut warned = false;
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains("[warn]") {
                    warned = true;
                }
            }
        }
        assert!(
            warned,
            "a claimed-but-absent change must raise the phantom-change warning"
        );
    }

    #[tokio::test]
    async fn agentic_truncation_marks_reply_incomplete() {
        // A Warning event mid-stream -> the recorded reply carries an
        // "incomplete / verify" caveat rather than reading as clean success.
        let tmp = init_git_repo();
        let path = tmp.path().to_path_buf();
        let (sink, _engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let spy = EffectSpy {
            reply: "done".to_string(),
            warn: true,
            effect: Box::new(|| ()),
        };
        drive_agentic_stream(&spy, "go", "m", "claude-code", &path, &sink, &route_tx).await;

        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone(text)) => {
                let incomplete = text.contains("未完成") || text.contains("incomplete");
                assert!(
                    text.contains("[warn]") && incomplete,
                    "a truncated turn must flag possible incompleteness, got: {text}"
                );
            }
            other => panic!("expected AgenticDone with caveat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agentic_fact_check_fails_open_outside_git() {
        // Outside any git repo the fact check must SILENTLY skip — no fact line,
        // no warning, no panic — and the turn still completes cleanly.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let spy = EffectSpy {
            reply: "I refactored everything".to_string(),
            warn: false,
            effect: Box::new(|| ()),
        };
        drive_agentic_stream(&spy, "go", "m", "claude-code", &path, &sink, &route_tx).await;

        // No [warn]/fact Note despite a loud claim — git was unavailable.
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                let leaked = n.contains("[warn]") || n.contains("文件变更");
                assert!(!leaked, "fail-open: no fact/warn line outside a git repo");
            }
        }
        // The turn still finishes cleanly.
        assert!(matches!(
            route_rx.try_recv(),
            Ok(RouteDecision::AgenticDone(_))
        ));
    }

    #[test]
    fn port_is_free_on_ephemeral() {
        // Bind to an ephemeral port, close it, then check it's free.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        // Brief retry — the OS may take a moment to release the socket.
        let url = format!("http://127.0.0.1:{port}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            port_is_free(&url),
            "ephemeral port should be free after drop"
        );
    }

    #[test]
    fn port_is_free_false_when_occupied() {
        // Bind a listener and keep it open — port_is_free must return false.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        assert!(!port_is_free(&url), "occupied port must report not-free");
        drop(listener);
    }

    #[test]
    fn url_host_port_extracts_localhost_5173() {
        assert_eq!(
            url_host_port("http://localhost:5173/foo"),
            Some("localhost:5173".into())
        );
    }

    #[test]
    fn url_host_port_extracts_127_0_0_1_3000() {
        assert_eq!(
            url_host_port("http://127.0.0.1:3000"),
            Some("127.0.0.1:3000".into())
        );
    }

    #[test]
    fn url_host_port_none_for_garbage() {
        assert_eq!(url_host_port("not a url"), None);
        assert_eq!(url_host_port("ftp://example.com"), None);
    }

    #[tokio::test]
    async fn wait_for_port_times_out_on_closed() {
        // Nothing listening on :1 — must time out quickly.
        let start = std::time::Instant::now();
        let up = wait_for_port("http://127.0.0.1:1", std::time::Duration::from_millis(600)).await;
        assert!(!up, "should time out, nothing on :1");
        assert!(start.elapsed() >= std::time::Duration::from_millis(400));
    }

    #[tokio::test]
    async fn wait_for_port_succeeds_on_open_listener() {
        // Bind a real listener on an ephemeral port, then wait_for_port it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let up = wait_for_port(&url, std::time::Duration::from_secs(2)).await;
        assert!(up, "should connect to the bound listener");
        drop(listener);
    }

    #[test]
    fn parse_run_command_cd_form() {
        let root = std::path::PathBuf::from("/proj");
        let (dir, prog, args) = parse_run_command("cd web && npm run dev", &root);
        assert_eq!(dir, std::path::PathBuf::from("/proj/web"));
        assert_eq!(prog, "npm");
        assert_eq!(args, vec!["run".to_string(), "dev".into()]);
    }

    #[test]
    fn parse_run_command_absolute_dir() {
        let root = std::path::PathBuf::from("/proj");
        let (dir, prog, args) = parse_run_command("cd /abs/app && pnpm dev", &root);
        assert_eq!(dir, std::path::PathBuf::from("/abs/app"));
        assert_eq!(prog, "pnpm");
        assert_eq!(args, vec!["dev".to_string()]);
    }

    #[test]
    fn parse_run_command_fallback_shells() {
        let root = std::path::PathBuf::from("/proj");
        let (dir, prog, args) = parse_run_command("npm run dev", &root);
        // No `cd &&` prefix → fallback to sh -c in the workspace root.
        assert_eq!(dir, root);
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c".to_string(), "npm run dev".into()]);
    }

    #[test]
    fn parse_run_command_npx_vercel_deploy() {
        // The canonical /deploy command. No `cd &&` → sh -c fallback,
        // preserving the full command (flags included).
        let root = std::path::PathBuf::from("/proj");
        let (dir, prog, args) = parse_run_command("npx vercel --prod", &root);
        assert_eq!(dir, root);
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c".to_string(), "npx vercel --prod".into()]);
    }

    #[test]
    fn parse_run_command_cd_with_npm_exec_flags() {
        // `cd web && npm exec -- vite` — flags after the program must survive.
        let root = std::path::PathBuf::from("/proj");
        let (dir, prog, args) = parse_run_command("cd web && npm exec -- vite", &root);
        assert_eq!(dir, std::path::PathBuf::from("/proj/web"));
        assert_eq!(prog, "npm");
        assert_eq!(args, vec!["exec".to_string(), "--".into(), "vite".into()]);
    }

    #[test]
    fn parse_run_command_trims_whitespace() {
        let root = std::path::PathBuf::from("/proj");
        let (dir, _, _) = parse_run_command("   cd app   &&   npm run dev   ", &root);
        assert_eq!(dir, std::path::PathBuf::from("/proj/app"));
    }

    #[test]
    fn parse_run_command_single_quoted_dir() {
        // Quoted directory names should be unquoted.
        let root = std::path::PathBuf::from("/proj");
        let (dir, prog, _) = parse_run_command("cd 'my app' && npm run dev", &root);
        assert_eq!(dir, std::path::PathBuf::from("/proj/my app"));
        assert_eq!(prog, "npm");
    }

    #[test]
    fn build_brain_offline_default() {
        let brain =
            build_brain(&BrainSpec::Offline, false, None, std::path::Path::new(".")).unwrap();
        assert_eq!(brain.kind(), RuntimeKind::Anthropic);
    }

    #[test]
    fn build_brain_accepts_every_registered_backend() {
        // Lock the TUI ↔ umadev-host wiring. If `BACKEND_IDS` adds an
        // entry but the TUI dispatch (`build_brain` → `driver_for`)
        // doesn't reach it, the user picks the backend in the picker and
        // it silently falls back to offline — this test makes that
        // mismatch loud at test time.
        for id in umadev_host::BACKEND_IDS {
            assert!(
                build_brain(
                    &BrainSpec::HostCli((*id).to_string()),
                    false,
                    None,
                    std::path::Path::new(".")
                )
                .is_ok(),
                "TUI cannot build brain for registered backend {id}"
            );
        }
    }

    #[test]
    fn build_brain_rejects_unknown_host_cli() {
        assert!(build_brain(
            &BrainSpec::HostCli("not-a-host".into()),
            false,
            None,
            std::path::Path::new(".")
        )
        .is_err());
    }

    #[test]
    fn launch_options_effective_slug_uses_explicit_first() {
        assert_eq!(opts().effective_slug(), "demo");
    }

    #[test]
    fn launch_options_effective_slug_falls_back_to_dir_name() {
        let mut o = opts();
        o.slug.clear();
        o.project_root = PathBuf::from("/tmp/my-project");
        assert_eq!(o.effective_slug(), "my-project");
    }

    #[test]
    fn start_failed_note_treats_would_block_as_retriable() {
        // `WouldBlock` = this session's previous run still holds the lock (its
        // guard hasn't dropped yet). Surface the retriable "a pipeline is
        // running" hint, NOT the generic start-failed shout.
        let e = std::io::Error::new(std::io::ErrorKind::WouldBlock, "self holds lock");
        let note = start_failed_note(&e);
        assert_eq!(note, umadev_i18n::tl("run.busy_reopen"));
        assert_ne!(
            note,
            umadev_i18n::tlf("pipeline.start_failed", &["self holds lock"]),
            "WouldBlock must not fall through to the hard-error note"
        );
    }

    #[test]
    fn start_failed_note_passes_through_real_errors() {
        // A genuine start failure (not the same-session lock race) keeps the
        // generic note with the underlying error text.
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "boom");
        let note = start_failed_note(&e);
        assert_eq!(note, umadev_i18n::tlf("pipeline.start_failed", &["boom"]));
    }

    // ── Continuous long-session run path (TUI `run` intent unification) ──────

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

    /// P1-D: a revise re-drives the PRODUCING block on the held session — the docs
    /// gate regenerates from Research, the preview gate from Spec (NOT the approved
    /// docs). Distinct from `continuous_resume_phase` (which advances PAST the gate).
    #[test]
    fn continuous_revise_phase_re_enters_the_producing_block() {
        // Docs gate revise → regenerate the three docs from the top (Research).
        assert_eq!(
            continuous_revise_phase(Gate::DocsConfirm),
            umadev_spec::Phase::Research
        );
        // Preview gate revise → regenerate spec → frontend (Spec), keeping docs.
        assert_eq!(
            continuous_revise_phase(Gate::PreviewConfirm),
            umadev_spec::Phase::Spec
        );
        // It is the INVERSE direction of the resume phase at the preview gate:
        // resume advances to Backend, revise re-enters at Spec.
        assert_ne!(
            continuous_revise_phase(Gate::PreviewConfirm),
            continuous_resume_phase(Gate::PreviewConfirm)
        );
    }

    /// Only `auto` makes the continuous session autonomous; `guarded` / `plan`
    /// keep the human-in-the-loop posture.
    #[test]
    fn continuous_autonomous_only_for_auto() {
        assert!(continuous_autonomous(umadev_agent::TrustMode::Auto));
        assert!(!continuous_autonomous(umadev_agent::TrustMode::Guarded));
        assert!(!continuous_autonomous(umadev_agent::TrustMode::Plan));
    }

    /// The continuous path is now the DEFAULT (the architecture has closed on it):
    /// with nothing set, the TUI selects continuous; an explicit opt-out
    /// (`UMADEV_CONTINUOUS=0` / `UMADEV_LEGACY_RUN=1`) selects the legacy
    /// single-shot path. Serial: saves + restores both vars (the process env is
    /// shared) so it never leaves global state mutated.
    #[test]
    fn tui_continuous_default_on_with_opt_out() {
        let saved_c = std::env::var("UMADEV_CONTINUOUS").ok();
        let saved_l = std::env::var("UMADEV_LEGACY_RUN").ok();

        // Unset → DEFAULT ON.
        std::env::remove_var("UMADEV_CONTINUOUS");
        std::env::remove_var("UMADEV_LEGACY_RUN");
        assert!(tui_continuous_enabled(), "continuous is the default");

        // Explicit opt-out → single-shot.
        std::env::set_var("UMADEV_CONTINUOUS", "0");
        assert!(!tui_continuous_enabled(), "UMADEV_CONTINUOUS=0 opts out");
        std::env::set_var("UMADEV_CONTINUOUS", "1");
        std::env::set_var("UMADEV_LEGACY_RUN", "1");
        assert!(!tui_continuous_enabled(), "UMADEV_LEGACY_RUN=1 opts out");

        // Restore.
        std::env::remove_var("UMADEV_LEGACY_RUN");
        match saved_c {
            Some(v) => std::env::set_var("UMADEV_CONTINUOUS", v),
            None => std::env::remove_var("UMADEV_CONTINUOUS"),
        }
        match saved_l {
            Some(v) => std::env::set_var("UMADEV_LEGACY_RUN", v),
            None => std::env::remove_var("UMADEV_LEGACY_RUN"),
        }
    }

    /// Fail-open: when the persistent session can't open (an unknown backend id
    /// → `session_for` errors deterministically, no real base process spawned),
    /// `spawn_continuous_block` emits ONE honest terminal-abort note and the task
    /// returns — never a panic, never a wedge, and the holder stays empty so a
    /// retry can open fresh.
    #[tokio::test]
    async fn continuous_block_fails_open_when_session_cannot_start() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, mut rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let holder: SessionHolder = Arc::new(tokio::sync::Mutex::new(None));
        let options = RunOptions {
            project_root: tmp.path().to_path_buf(),
            requirement: "build a dashboard".into(),
            slug: "demo".into(),
            model: String::new(),
            // An id `session_for` rejects → deterministic `SessionError`, with NO
            // real subprocess, so the test is hermetic on any machine.
            backend: "nonexistent-backend".into(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: umadev_agent::TrustMode::Guarded,
            strict_coverage: false,
        };

        let handle = spawn_continuous_block(
            options,
            sink.clone(),
            holder.clone(),
            umadev_spec::Phase::Research,
            false,
        );
        // The task must FINISH (no hang) and not panic.
        handle.await.expect("continuous block task must not panic");

        // It emitted exactly the honest terminal-abort note (carrying the
        // sentinel) — the same fail-open shape the single-shot path uses.
        let mut saw_abort = false;
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains(ABORT_SENTINEL) {
                    saw_abort = true;
                }
            }
        }
        assert!(
            saw_abort,
            "a failed session start emits a terminal-abort note"
        );
        // The holder stays empty (no half-open session parked) → a retry opens fresh.
        assert!(
            holder.lock().await.is_none(),
            "no session parked after a failed start"
        );
    }
}
