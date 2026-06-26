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
pub mod selection;
pub mod ui;

use std::io::Stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyEventKind, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use umadev_agent::{AgentRunner, ChannelSink, EngineEvent, EventSink, Gate, RoutePlan, RunOptions};
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

/// Terminal signal from a brain-driven turn back to the event loop. UmaDev no
/// longer classifies the user's intent (chat vs run) up front — every non-slash
/// message goes straight to the tools-enabled base session, which decides for
/// itself whether to reply or to act. So this only carries the two terminal
/// outcomes the streaming turn can end with.
#[derive(Debug, Clone, Eq, PartialEq)]
enum RouteDecision {
    /// A brain-driven streaming turn finished. Carries the final assembled text so
    /// the event loop records it as the assistant turn (chat memory continuity);
    /// the body was ALREADY streamed live via `WorkerStream`, so it is NOT
    /// re-rendered. A terminal outcome → clears the "thinking…" status.
    ///
    /// `director_build` carries whether THIS turn was a Build-class director build
    /// (run-lock + branch isolation + finalize). The chat surface now classifies
    /// INSIDE the spawned task (the brain-router consult is 1-3s, so it cannot run
    /// inline on the UI thread), which means the event loop no longer knows the
    /// class before dispatch — so the build-ness is carried back HERE and drives
    /// the Wave-5 session hand-back (`record_agentic_done`) instead of the
    /// pre-spawn `director_run_in_flight` flag.
    AgenticDone {
        /// The final assembled assistant text (already streamed live).
        reply: String,
        /// `true` when this was a director build → hand the session back to chat.
        director_build: bool,
    },
    /// The turn produced no usable reply (base init failed, an empty reply, or a
    /// hard error). Carries the human-readable reason, routed through the same
    /// channel so the event loop clears the "thinking…" status on EVERY terminal
    /// outcome, and a plain progress Note never has to.
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
    // REAP the launcher on a detached thread. The OS URL-launcher (`open` /
    // `xdg-open` / `cmd start`) hands off to the browser and exits within ms;
    // dropping the `Child` without `wait()` leaves a defunct (zombie) process on
    // Unix that accumulates over every `/preview` / auto-open (P1).
    #[allow(dead_code)]
    fn reap(child: std::process::Child) {
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
    }
    #[cfg(target_os = "macos")]
    {
        reap(std::process::Command::new("open").arg(url).spawn()?);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        reap(std::process::Command::new("xdg-open").arg(url).spawn()?);
    }
    #[cfg(target_os = "windows")]
    {
        reap(
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .spawn()?,
        );
    }
    Ok(())
}

/// Start a preview dev server in the background and surface its URL once the
/// port is up. Shared by the manual `/preview` ([`Action::StartPreview`]) path
/// and the automatic post-build preview, so both behave identically: the
/// port-conflict guard, the background `wait_for_port` + browser-open, the
/// `preview_server` child handle (parked for exit-cleanup), and all the
/// `preview.*` notes are defined exactly once here.
///
/// **Fail-open / non-blocking by contract**: spawning the dev server is
/// best-effort and never blocks the TUI — `wait_for_port` runs in a detached
/// task, a spawn failure only emits a hint, and a busy port opens what is
/// already running instead of starting a second server. The child is stored in
/// `preview_server` so the run-exit cleanup (`run()`) kills it and no process
/// leaks. `open_browser` controls whether the URL is auto-opened in a browser
/// (the manual `/preview` opens it; the automatic post-build preview does NOT —
/// it only surfaces the clickable URL so the build flow never steals focus).
fn start_preview_server(
    preview_server: &std::sync::Arc<std::sync::Mutex<Option<tokio::process::Child>>>,
    sink: &Arc<ChannelSink>,
    url: &str,
    command: &str,
    project_root: &std::path::Path,
    open_browser: bool,
) {
    let (dir, prog, args) = parse_run_command(command, project_root);
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(&args)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // Port-conflict guard: if the port is already bound (the user's own
    // Vite/Next/Express), DON'T spawn a second server — it would either fail or
    // bind a different port while we open the wrong URL. Open / surface what's
    // already running instead.
    if port_is_free(url) {
        match cmd.spawn() {
            Ok(child) => {
                if let Ok(mut g) = preview_server.lock() {
                    *g = Some(child);
                }
                sink.emit(EngineEvent::Note(
                    umadev_i18n::tl("preview.dev_starting").into(),
                ));
                let url2 = url.to_string();
                let sink3 = sink.clone();
                tokio::spawn(async move {
                    let up = wait_for_port(&url2, std::time::Duration::from_secs(15)).await;
                    if up {
                        if open_browser {
                            let _ = open_url(&url2);
                        }
                        sink3.emit(EngineEvent::Note(umadev_i18n::tlf(
                            "preview.dev_ready",
                            &[&url2],
                        )));
                    } else {
                        sink3.emit(EngineEvent::Note(umadev_i18n::tlf(
                            "preview.dev_not_ready",
                            &[&url2],
                        )));
                    }
                });
            }
            Err(e) => {
                sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "preview.dev_spawn_failed",
                    &[command, &e.to_string(), url],
                )));
            }
        }
    } else {
        if open_browser {
            let _ = open_url(url);
        }
        sink.emit(EngineEvent::Note(umadev_i18n::tlf(
            "preview.port_busy",
            &[url],
        )));
    }
}

/// Post the **build-complete card** for a finished build and, for a web
/// project, auto-start its dev server so a clickable preview URL surfaces — the
/// "✅ done + what changed + here's the demo" finish that previously only the
/// heavyweight Delivery path produced (and even there, without a localhost URL).
/// Shared by the chat/Fast `AgenticDone` build path and the Delivery
/// `BlockCompleted` banner.
///
/// **Fail-open / non-blocking**: a non-web project (no dev server detected) just
/// gets the card with no preview line and starts no server; the dev server is
/// best-effort and detached (`start_preview_server`), so nothing here blocks the
/// completion or the TUI. The browser is NOT auto-opened on this path (only the
/// explicit `/preview` opens it) — the URL is surfaced for the user to click.
///
/// **Honesty guard**: a celebratory "✅ build complete" card only fires when the
/// workspace actually holds real source (`acceptance::source_files`). A phantom
/// build — the director CLAIMED a build but produced zero source — already gets a
/// loud `ABORT_SENTINEL` note from [`director_source_hardgate`]; we must NOT also
/// crown it "done". This is the SAME bounded source scan the hardgate uses, so
/// the two reality checks agree.
fn finalize_build_completion(app: &mut App, sink: &Arc<ChannelSink>) {
    if umadev_agent::acceptance::source_files(&app.project_root).is_empty() {
        // Nothing real landed — leave the honest abort/fact notes as the record,
        // don't paint a success card over a no-op build.
        return;
    }
    // `post_build_completion_card` pushes the card (preview line is a "starting…"
    // placeholder when a dev server was detected; the real URL is appended by
    // `start_preview_server`'s `preview.dev_ready` note once up) and hands back
    // the dev-server target — `None` for a non-web project (no server started).
    let preview = app.post_build_completion_card();
    if let Some((url, command)) = preview {
        start_preview_server(
            &app.preview_server,
            sink,
            &url,
            &command,
            &app.project_root,
            false,
        );
    }
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

/// A resident chat session parked in the [`ChatSessionHolder`], tagged with whether
/// it has taken a turn yet. The distinction is load-bearing for the FIRST directive:
/// a **warm** session (the background pre-load just spawned it, or a fresh lazy-open)
/// has the firmware injected but has seen NO user turn, so the first message must
/// front-load the conversation transcript (and re-prefix firmware for a non-claude
/// base); a **primed** session already took a turn, so its own native memory carries
/// the dialogue and the next message is sent bare.
enum ResidentChat {
    /// Spawned + firmware-injected, but no turn taken yet (pre-loaded or lazy-opened).
    /// Carries the firmware so the first directive can re-prefix it for a base with
    /// no native system slot (codex / opencode); claude already has it natively.
    Warm(WarmChatSession),
    /// Already drove at least one turn — reuse it bare (native memory holds context).
    Primed(Box<dyn umadev_runtime::BaseSession>),
}

impl ResidentChat {
    /// End the underlying base session (best-effort), whichever state it is in. Used
    /// on `/clear` / a backend switch / quit / cancel to release the subprocess.
    async fn end(self) {
        let mut session = match self {
            ResidentChat::Warm(w) => w.session,
            ResidentChat::Primed(s) => s,
        };
        let _ = session.end().await;
    }
}

/// The RESIDENT chat session holder — ONE base session kept alive across the whole
/// conversation on the host-CLI chat path (the latency fix). A `tokio::sync::Mutex`
/// so a spawned turn task can take it across `.await`; shared `Arc` with the event
/// loop and the background pre-load task. `None` until the pre-load (or the first
/// turn's lazy-open) lands a [`ResidentChat::Warm`]; parked back as
/// [`ResidentChat::Primed`] after every turn so the next message reuses the SAME
/// process. Distinct type from [`SessionHolder`] (the director-run session) because
/// chat tracks the warm/primed state the director path does not need.
type ChatSessionHolder = Arc<tokio::sync::Mutex<Option<ResidentChat>>>;

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
            None => match umadev_host::session_for(&backend, &root, &model, autonomous, None).await
            {
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

/// Spawn the **director build loop** for an explicit `/run` (the USB / smart-
/// hardware model of `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md`, simplified — no marker
/// protocol) — the TUI counterpart of the CLI's `drive_director_run`. It opens ONE
/// live [`umadev_runtime::BaseSession`] (the director's brain) and drives
/// [`umadev_agent::drive_director_loop`]: the firmware (team identity + craft) is
/// injected, the base's body builds the goal end to end with its own tools, then
/// UmaDev runs a read-only honesty/QC pass and feeds any blocking findings back as a
/// bounded fix directive the base acts on. Floor preserved exactly as the CLI path:
/// the single-writer run-lock is held for the whole loop, the always-on irreversible
/// floor + the governance hook still apply, and the objective source-present hard-
/// gate runs after the loop reports done.
///
/// All the surrounding TUI machinery is UNCHANGED: tool calls + text stream live
/// via [`EngineEvent::WorkerStream`] (the same render path), and a terminal
/// [`RouteDecision::AgenticDone`] / [`RouteDecision::Failed`] clears `thinking` and
/// records the assistant turn for chat-memory continuity — identical to
/// [`spawn_agentic`]. A NEW base session is opened per `/run` (a director build is
/// a standalone, run-to-settle orchestration, not a gate-anchored block sequence),
/// and `end()`-ed when the loop settles.
///
/// **Fail-open:** a session that can't open / the run-lock held by a DIFFERENT live
/// run emits the honest `ABORT_SENTINEL` note and a terminal `Failed`; a session
/// that dies mid-loop is a `Failed` outcome. It NEVER panics or wedges, and a base
/// whose first build passes QC clean settles immediately (no fix pass).
///
/// **Chat-originated build (`conversation` non-empty):** when a plain chat message
/// auto-promotes into a director build (Blocker #2 — a "build me X" said in chat
/// must get the same plan / step scheduling / finalize / acceptance the `/run` and
/// CLI paths get, NOT the single-turn `drive_agentic_stream`), the caller passes
/// UmaDev's OWN bounded conversation transcript. It is front-loaded onto the first
/// directive so the director's brain sees the prior dialogue (Wave 5 / G11 memory),
/// exactly the way `drive_agentic_stream` threads it for a light chat turn — the
/// base's `--resume` is belt-and-suspenders, this transcript is the load-bearing
/// memory across a restart / switched base. An explicit `/run` passes an empty
/// transcript (no prior chat to inherit) and is unchanged. The session hand-back to
/// chat is driven by the caller's `app.director_run_in_flight` + the terminal
/// `RouteDecision::AgenticDone`, identical for both origins.
fn spawn_director_loop(
    options: RunOptions,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    autonomous: bool,
    conversation: Vec<Message>,
    route_override: Option<RoutePlan>,
    goal_mode: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_director_loop(
        options,
        sink,
        route_tx,
        autonomous,
        conversation,
        route_override,
        goal_mode,
    ))
}

/// The director build loop body — the non-spawning core of [`spawn_director_loop`].
///
/// Split out so the brain-routed chat dispatcher ([`run_routed_turn`]) can drive
/// the director build INLINE from inside its OWN already-spawned classification
/// task (a chat message classified `Build` must reuse this exact path — run-lock,
/// branch isolation, firmware, the routed plan/step/finalize loop, source hard-gate
/// — not a second copy). The `/run` entry + the queued-chat drain keep calling the
/// spawning wrapper. The body is byte-for-byte the original; only the outer
/// `tokio::spawn(async move { … })` moved up into the wrapper.
async fn run_director_loop(
    options: RunOptions,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    autonomous: bool,
    conversation: Vec<Message>,
    route_override: Option<RoutePlan>,
    goal_mode: bool,
) {
    {
        let backend = options.backend.clone();
        let model = options.model.clone();
        let root = options.project_root.clone();

        // Single-writer run-lock for the whole director loop — the SAME guard the
        // CLI `drive_director_run` + the legacy pipeline hold, so a director build
        // serializes with any other workspace-mutating run. A lock held by a
        // DIFFERENT live run is an honest terminal abort; any other lock IO fails
        // open inside `acquire_for_run` to an un-owned guard (a lock bug never
        // blocks a legitimate build). The guard lives for the task's scope.
        let _run_lock = match umadev_agent::run_lock::RunLock::acquire_for_run(&root) {
            Ok(g) => g,
            Err(e) => {
                sink.emit(EngineEvent::Note(format!(
                    "{ABORT_SENTINEL}{}",
                    block_abort_note(&e, &backend)
                )));
                let _ = route_tx.send(RouteDecision::Failed(start_failed_note(&e)));
                return;
            }
        };

        // Git-as-trust (Wave 6): isolate this director build onto `umadev/<slug>`
        // and snapshot the run baseline before the base writes anything — never on
        // the user's default/working branch, never auto-merged/pushed. Fail-open:
        // a non-git dir / dirty tree / any error just runs in the working tree.
        if let Some((branch, from)) =
            umadev_agent::setup_run_isolation(&root, &options.effective_slug())
        {
            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.branch_isolated",
                &[&branch, &from],
            )));
        }

        // MEDIUM #7: write the WorkflowState baseline, exactly like the CLI's
        // `AgentRunner::start` does (`.umadev/workflow-state.json`, phase `research`,
        // slug + requirement + backend). Without this a TUI-originated director build
        // left no state on disk, so `umadev status` / `umadev continue` against a
        // build STARTED in the chat TUI read `Missing` and bailed — the run was
        // invisible to the CLI surfaces. Written here (after the run-lock + isolation,
        // before the base writes anything) so the baseline reflects this run. Fail-open
        // by contract: a disk/permission error is swallowed (`let _ =`) — a state-write
        // bug must NEVER block an otherwise-healthy build.
        let baseline = {
            // `WorkflowState::new` fills `last_transition_at` (now) + `spec_version`;
            // override the run-specific carry-through fields the CLI's `start` sets.
            let mut s = umadev_agent::WorkflowState::new(umadev_spec::Phase::Research);
            s.slug = options.effective_slug();
            s.requirement.clone_from(&options.requirement);
            s.backend.clone_from(&backend);
            s.note = format!("Started director build (TUI) with {backend}");
            s
        };
        let _ = umadev_agent::write_workflow_state(&root, &baseline);

        // Wave 2 (firmware): compose UmaDev's identity + craft + JIT knowledge +
        // pitfall memory once (the `/run` route is deterministic, no session needed)
        // so claude can take it NATIVELY as a system prompt via `session_for`'s
        // `--append-system-prompt`. Fail-open: an empty firmware just leaves the base
        // un-primed beyond the directive, exactly as before.
        //
        // Route source: an explicit `/run` passes `None` → `for_run` FORCES a Build
        // (a bare goal still builds). A chat-originated build (Blocker #2) passes the
        // route it was ALREADY classified with — the honest Tier-0 route the intent
        // card showed — so the build drives with that exact route, never a re-forced
        // one. Either way the route is deterministic; no session needed.
        let route =
            route_override.unwrap_or_else(|| umadev_agent::router::for_run(&options.requirement));
        let firmware = umadev_agent::compose_firmware(&root, &route, &options.requirement).await;
        let firmware = (!firmware.trim().is_empty()).then_some(firmware);

        // Open the director's live base session. Fail-open: a session that can't
        // open emits the honest terminal abort + a terminal Failed (the user can
        // retry, or opt into the legacy pipeline with `UMADEV_LEGACY_PIPELINE=1`).
        let mut session = match umadev_host::session_for(
            &backend,
            &root,
            &model,
            autonomous,
            firmware.as_deref(),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                sink.emit(EngineEvent::Note(format!(
                    "{ABORT_SENTINEL}{}",
                    umadev_i18n::tlf("continuous.tui_session_unavailable", &[&e.to_string()])
                )));
                let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                    "continuous.tui_session_unavailable",
                    &[&e.to_string()],
                )));
                return;
            }
        };

        // Frame the goal for the director (the firmware framing), then drive the
        // build loop: the base builds end to end, UmaDev runs its honesty/QC read.
        // claude already took the firmware NATIVELY (system prompt) above; codex /
        // opencode have no native slot, so for THEM we front-load the same firmware
        // onto the first directive (the universal fail-open path) — never restating
        // it on claude. Fail-open: no firmware → the goal directive is unchanged.
        let goal = umadev_agent::experts::director_build_directive(&options.requirement);
        // Chat-originated build (Blocker #2): front-load UmaDev's OWN bounded
        // conversation transcript so the director's brain inherits the prior dialogue
        // — the SAME Wave 5 / G11 memory `drive_agentic_stream` threads for a light
        // chat turn, so a build promoted out of a conversation keeps that context
        // instead of starting cold. Empty for an explicit `/run` (no prior chat) →
        // the directive is unchanged. See `director_directive_with_history`.
        let goal = director_directive_with_history(&conversation, &options.requirement, goal);
        let directive = match firmware.as_deref() {
            Some(fw) if backend != "claude-code" => format!("{fw}\n\n---\n\n{goal}"),
            _ => goal,
        };
        // GOAL MODE (mirrors the legacy pipeline's `with_goal_mode`): front-load a
        // persistent-`/goal` framing so the base keeps working until the objective is
        // met instead of stopping early. `goal_mode` is set by the `/goal` command
        // (and defaulted on for every director build — Claude Code's native persistent
        // mode is strictly stronger than a plain prompt loop). The ENCODING follows the
        // borrowed brain's CAPABILITY: a native-`/goal` base (claude) gets a real
        // `/goal` command, codex / opencode get the same intent as a prompt fallback
        // (the director loop drives them to completion regardless). It MUST be the very
        // first thing the base reads, so it prepends ahead of the firmware block too.
        // Fail-open: `UMADEV_NO_GOAL_MODE=1`, or a backend whose capabilities can't be
        // read, leaves the directive exactly as before.
        let directive = match resolve_goal_mode(&backend, goal_mode) {
            Some(persistent_goal) => format!(
                "{}{directive}",
                umadev_agent::experts::goal_mode_prefix(&options.requirement, persistent_goal)
            ),
            None => directive,
        };
        let sink_dyn: Arc<dyn EventSink> = sink.clone();
        // Drive the loop ROUTED (Blocker #1 fix): pass the route computed at the top
        // of this task so the director loop emits the visible intent card, synthesises
        // + posts the owned plan (`PlanPosted`), drives the plan step-by-step
        // (`PlanStepStatus`), runs per-step acceptance on the deterministic floor, and
        // finalizes — exactly as the CLI `umadev run` path does. The unrouted entry
        // (route=None) skipped `synthesize_and_post_plan` and step scheduling, which is
        // why the flagship plan/schedule/finalize/acceptance machinery was DEAD on the
        // TUI `/run`. Fail-open: an unparseable/empty plan inside the routed entry just
        // degrades to the single-turn loop, so this never loses a build.
        let outcome = umadev_agent::drive_director_loop_routed(
            session.as_mut(),
            &options,
            &sink_dyn,
            directive,
            Some(&route),
        )
        .await;
        // Always end the session (release the process / server).
        let _ = session.end().await;

        match outcome {
            umadev_agent::DirectorLoopOutcome::Done { reply } => {
                // Objective source-present hard-gate (the deterministic reality
                // floor) — the SAME check the free-text agentic path + the CLI run
                // apply. A `/run` that CLAIMED a build but produced zero real source
                // is reported honestly (an `ABORT_SENTINEL` note), never celebrated.
                if let Some(note) = director_source_hardgate(&root, &reply) {
                    sink.emit(EngineEvent::Note(note));
                }
                // The body already streamed live; hand the assembled text to the
                // event loop to record as the assistant turn + clear `thinking`. A
                // director loop is ALWAYS a Build → the hand-back fires.
                let _ = route_tx.send(RouteDecision::AgenticDone {
                    reply,
                    director_build: true,
                });
            }
            umadev_agent::DirectorLoopOutcome::Failed(reason) => {
                // An honest terminal abort (session died / a turn failed). Flag the
                // terminal state (so the bar shows a real aborted state) + clear
                // `thinking` via the terminal Failed decision.
                sink.emit(EngineEvent::Note(format!("{ABORT_SENTINEL}{reason}")));
                let _ = route_tx.send(RouteDecision::Failed(reason));
            }
        }
    }
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

/// Everything the tools-enabled brain-driven turn needs — bundled so the spawn
/// stays within a sane argument count. The base runs its own tool loop.
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
    /// **Director-build mode** (Wave 1 of `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md`
    /// §5): set when this turn is an explicit `/run` routed through the director
    /// agentic path instead of the legacy fixed pipeline. When `true` the turn
    /// (a) holds the single-writer run-lock for its whole duration (so a director
    /// build serializes with any other workspace-mutating run), and (b) runs the
    /// objective **source-present hard-gate** after the stream — if the director
    /// reported a build but the workspace has zero real source files, an honest
    /// abort note is emitted (the deterministic floor verifying reality, never
    /// disguising "claimed done" as success). A normal free-text turn leaves this
    /// `false` and keeps the lighter git-diff fact line only.
    director_build: bool,
    /// Whether a real **host CLI** is driving this turn (vs. the offline runtime).
    /// LOW fix (tui-dispatch): the single-writer run-lock + branch isolation are a
    /// HOST director-build concern — a `Build`-class verdict against a NON-host
    /// brain (offline) stays on this light streaming path and must NOT grab the
    /// workspace lock or isolate a branch (it writes nothing the lock protects). So
    /// the lock/isolation gate below is `director_build && host_cli`, not
    /// `director_build` alone. The source hard-gate still runs on `director_build`
    /// (it only READS the tree to verify reality).
    host_cli: bool,
    /// The turn's typed [`RoutePlan`] — drives the firmware tier in
    /// [`compose_firmware`] (HIGH #3 / MEDIUM #6): pure chat carries only the
    /// identity, a quick edit adds the craft law, a build gets every layer. This
    /// REPLACES the old `looks_like_work_request` keyword decision on the light
    /// path, so "固件每路径 + 大脑判档" holds — the brain-router's class sizes the
    /// firmware, not a hardcoded keyword list.
    ///
    /// `Some` on the brain-routed dispatch ([`run_routed_turn`]); `None` on the
    /// **queued-drain** path ([`fire_agentic`]), which deliberately does NOT
    /// re-consult the brain — [`run_agentic`] then resolves it to a deterministic
    /// Tier-0 floor route (the same fail-open floor the router itself falls back
    /// to), so a drained turn still gets proportional firmware without a second
    /// base call.
    route: Option<RoutePlan>,
    /// **UmaDev's own bounded conversation transcript** (Wave 5 / G11) — the
    /// multi-turn dialogue, oldest → newest, INCLUDING the current user turn the
    /// caller just recorded. Threaded into the request so the base sees the
    /// dialogue from UmaDev's side rather than relying solely on its `--resume`.
    /// Bounded to a token budget inside [`drive_agentic_stream`]; empty means the
    /// single-message request form (today's behaviour) is used.
    conversation: Vec<Message>,
}

/// Spawn the tools-enabled agentic execution call. This is the live default
/// path: it sends the user's task to the base with NO tool-ban system prompt and NO
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
    tokio::spawn(run_agentic(turn, sink, route_tx))
}

/// A throwaway [`RunOptions`] used ONLY to compute a deterministic Tier-0 floor
/// route via [`umadev_agent::route`]`(None, …)` on the queued-drain path (where no
/// brain consult ran). The `None`-session branch of `route` never touches its
/// `options` argument beyond carrying the workspace, so every field except
/// `project_root` / `requirement` is a harmless placeholder — this exists purely to
/// satisfy the function signature without re-consulting the base. Fail-open by
/// construction: it builds, it never errors.
fn route_floor_options(project_root: &std::path::Path, requirement: &str) -> RunOptions {
    RunOptions {
        project_root: project_root.to_path_buf(),
        requirement: requirement.to_string(),
        slug: String::new(),
        model: String::new(),
        backend: String::new(),
        design_system: String::new(),
        seed_template: String::new(),
        mode: umadev_agent::TrustMode::Guarded,
        strict_coverage: false,
    }
}

/// Reactive-build context for the light chat path — lets [`drive_agentic_stream`]
/// turn a chat turn into a build the MOMENT the base writes its first real file,
/// without any up-front classification. `None` disables the whole reaction (the
/// explicit `/run` director path, the queued-drain, and every unit test pass
/// `None`, so their behaviour is byte-for-byte unchanged).
///
/// **Why react instead of pre-classify:** the base has tools and is the brain — it
/// judges chat-vs-build by ACTING (a reply is chat; a file write is a build). So
/// the chat surface opens the session ONCE (fast, no cold triage subprocess) and
/// watches: the first `Write`/`Edit`-family tool call (see
/// [`is_workspace_write_tool`]) flips the turn into a build — grab the
/// single-writer run-lock (if not already held), isolate onto `umadev/<slug>`
/// (`setup_run_isolation`: a `switch -c` carries the just-written change onto the
/// branch and leaves the user's branch alone — best-effort; a tree already dirty
/// from the write fails open to running in place), and surface the `Build` intent
/// card + the trust note. A pure-reply turn never trips it and stays a fast chat.
///
/// **Fail-open throughout:** a lock that can't be taken / an isolation that skips
/// just leaves the turn running in place (it never aborts a turn the way the
/// up-front `/run` lock does — a chat-build that loses the race to a concurrent
/// run is still better completed than killed). Idempotent: it fires its reaction
/// exactly once (`reacted` latches), so a 200-file build isolates one time.
struct ReactiveBuild {
    /// Whether a real **host CLI** drives this turn — only a host build mutates a
    /// workspace the lock/isolation protect (an offline turn writes nothing real).
    /// The whole reaction no-ops when this is false (mirrors the `director_build &&
    /// host_cli` gate the up-front `/run` lock uses).
    host_cli: bool,
    /// Latched the first time a write tool is seen, so the lock + isolation + intent
    /// card fire exactly once for the rest of the (possibly hundreds-of-write) turn.
    reacted: std::sync::atomic::AtomicBool,
    /// Set true once a write was observed — read after the stream to carry
    /// `director_build: true` on the terminal `AgenticDone` (drives the Wave-5
    /// session hand-back + the objective source-present hard-gate, exactly as a
    /// pre-classified build would).
    became_build: std::sync::atomic::AtomicBool,
    /// Holds the run-lock guard for the rest of the turn once the reaction grabs it
    /// (dropped when the `Arc` is dropped at the end of [`drive_agentic_stream`]).
    /// `Mutex` for interior mutability from the shared `Fn` stream closure.
    lock: std::sync::Mutex<Option<umadev_agent::run_lock::RunLock>>,
}

impl ReactiveBuild {
    /// A fresh, un-triggered reactive context for a host-or-not chat turn.
    fn new(host_cli: bool) -> Self {
        Self {
            host_cli,
            reacted: std::sync::atomic::AtomicBool::new(false),
            became_build: std::sync::atomic::AtomicBool::new(false),
            lock: std::sync::Mutex::new(None),
        }
    }
}

/// The **proportional default route** the chat surface drives the light path with
/// — used because the chat dispatcher NO LONGER pre-classifies each message with a
/// slow one-shot brain consult (that cold `claude --print` was the ~30s
/// first-reply latency this whole change removes). Instead, every chat turn opens
/// the persistent session ONCE on the light path with this fixed "可干活" route,
/// and the base — which has tools — decides for itself whether to chat (reply with
/// text) or to build (write files); UmaDev reacts to that behaviour (see
/// [`drive_agentic_stream`]'s reactive write detection).
///
/// A `QuickEdit` / `Fast` route is the deliberately-proportional firmware tier in
/// [`umadev_agent::compose_firmware`]: it injects the identity, the compact craft
/// law, and the repo-map slice of the user's code, but NOT the heavy full-build
/// layers (JIT knowledge + pitfall memory). So day-to-day chat carries enough
/// firmware to actually do small work without paying the full-build prompt cost on
/// every message. It is NOT shown as the intent card — the card is derived from the
/// base's behaviour (text reply = chat, a write = build), see [`run_routed_turn`].
///
/// Deterministic + allocation-light; fail-open by construction (it always builds).
#[must_use]
fn light_default_route() -> RoutePlan {
    use umadev_agent::{Budget, Depth, RouteClass, TaskKind};
    RoutePlan {
        class: RouteClass::QuickEdit,
        kind: TaskKind::Light,
        depth: Depth::Fast,
        // No pre-sized team: the base runs its OWN internal PM → design → code → QA
        // for a chat-build; the full schedulable team lives on the explicit `/run`
        // director loop. An empty team keeps the identity layer compact.
        team: Vec::new(),
        scope: Vec::new(),
        needs_clarify: None,
        est_budget: Budget::for_route(RouteClass::QuickEdit, Depth::Fast),
        confidence: 0.5,
    }
}

// NOTE: there is intentionally NO "chat intent card" route any more. A chat turn
// at t=0 emits no intent card at all (the user asked to remove the "this is
// conversation — replying directly" card — pure noise). The ONLY intent card the
// chat surface shows is the behaviour-derived "构建中" (`reactive_build_route`)
// surfaced the instant the base writes its first real file. So a pure reply shows
// no card; a turn that turns out to build re-surfaces a `Build` card reactively.

/// The `Build` intent card surfaced REACTIVELY the first time the base writes a
/// real file on the light chat path — the behaviour-derived "构建中" signal. A
/// `Fast` build (the chat surface never auto-schedules the heavy team — that is
/// `/run`), so the card reads "full build, fast" with no pre-committed roster.
#[must_use]
fn reactive_build_route() -> RoutePlan {
    use umadev_agent::{Budget, Depth, RouteClass, Seat, TaskKind};
    RoutePlan {
        class: RouteClass::Build,
        kind: TaskKind::Light,
        depth: Depth::Fast,
        // A chat-promoted build is a delivery: convene the MINIMAL UI review team
        // (designer + frontend + QA) so the post-build QC (`run_post_build_qc`)
        // actually forks critics over the output — not an empty roster that reviews
        // nothing. The full kind-sized roster stays on a deliberate /run build.
        team: vec![Seat::UiuxDesigner, Seat::FrontendEngineer, Seat::QaEngineer],
        scope: Vec::new(),
        needs_clarify: None,
        est_budget: Budget::for_route(RouteClass::Build, Depth::Fast),
        confidence: 0.6,
    }
}

/// `true` iff a base tool-call NAME mutates the workspace (creates / edits a
/// file) — the signal that turns a chat turn into a build on the light path.
///
/// All three bases normalise their write tools to these names in their stream
/// parsers (`umadev_host::claude` / `codex` / `opencode` emit `Write` for a new
/// file, `Edit` for an in-place change; a multi-edit / notebook-edit variant maps
/// onto the same family). A `Read` / `Grep` / `Bash` / `Glob` call is NOT a
/// workspace write (a `Bash` may technically write, but the deterministic
/// post-turn git fact-check is the floor for that — we only react to an EXPLICIT
/// file-write tool so a pure read/inspect/answer turn stays light). Case-folded so
/// a base that lower-cases tool names still matches. Pure + cheap.
#[must_use]
fn is_workspace_write_tool(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(
        n.as_str(),
        "write" | "edit" | "multiedit" | "notebookedit" | "create" | "apply_patch" | "applypatch"
    )
}

/// React to the FIRST workspace write on the light chat path: flip the turn into a
/// build. Called from the stream closure the instant a `Write`/`Edit`-family tool
/// call is seen. Fires its side-effects exactly ONCE (the `reacted` latch), so a
/// build that writes 200 files isolates one time. **Returns immediately + no-ops**
/// when reactive build is disabled (`None`), when the brain is not a host CLI
/// (nothing real to lock/isolate), or when it has already reacted this turn.
///
/// On the first real write it, in order and all **fail-open**:
/// 1. marks `became_build` (so the terminal `AgenticDone` carries
///    `director_build: true` → Wave-5 hand-back + the source hard-gate);
/// 2. surfaces the `Build` intent card (the behaviour-derived "构建中" signal) and
///    a one-line note that the turn is now a build (`chat.build_detected`);
/// 3. takes the single-writer run-lock (a chat-build serializes with other
///    workspace-mutating runs); a lock that can't be taken is swallowed (NOT the
///    `/run` hard-abort: a chat-build losing the race to another run is better
///    finished than killed — fail-open), the guard parks in `reactive.lock`;
/// 4. isolates onto `umadev/<slug>` + run-baseline (`setup_run_isolation`), emitting
///    the trust note ONLY when a fresh isolation branch was actually created.
///
/// Idempotent + fail-open by contract: any failure leaves the turn running in
/// place, exactly the pre-change behaviour for a chat-build.
fn react_to_first_write(
    reactive: Option<&ReactiveBuild>,
    project_root: &std::path::Path,
    sink: &Arc<ChannelSink>,
) {
    use std::sync::atomic::Ordering;
    let Some(reactive) = reactive else { return };
    // Only a real HOST build mutates a workspace the lock/isolation protect.
    if !reactive.host_cli {
        return;
    }
    // One-shot: `swap` returns the PREVIOUS value — if it was already `true`, a
    // prior write already reacted, so bail (no second lock / isolation / card).
    if reactive.reacted.swap(true, Ordering::SeqCst) {
        return;
    }
    // (1) This turn is now a build — the terminal decision will carry it back.
    reactive.became_build.store(true, Ordering::SeqCst);
    // (2) Behaviour-derived intent card ("构建中") + the one-line build note.
    sink.emit(EngineEvent::intent_decided(&reactive_build_route()));
    sink.emit(EngineEvent::Note(
        umadev_i18n::tl("chat.build_detected").to_string(),
    ));
    // (3) Single-writer run-lock for the rest of the turn — fail-open: a lock held
    // by a DIFFERENT live run is swallowed (the chat-build proceeds in place rather
    // than hard-aborting); any other IO fails open inside `acquire_for_run` to an
    // un-owned guard. The guard parks in `reactive.lock` for the turn's lifetime.
    if let Ok(guard) = umadev_agent::run_lock::RunLock::acquire_for_run(project_root) {
        if let Ok(mut slot) = reactive.lock.lock() {
            *slot = Some(guard);
        }
    }
    // (4) Branch isolation: `switch -c umadev/<slug>` carries the just-written change
    // onto the isolation branch and leaves the user's branch untouched (fail-open: a
    // non-git dir / a tree already dirty from the write / any error skips silently
    // and the turn runs in place). Emit the trust note only on a fresh isolation.
    let slug = project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    if let Some((branch, from)) = umadev_agent::setup_run_isolation(project_root, slug) {
        sink.emit(EngineEvent::Note(umadev_i18n::tlf(
            "trust.branch_isolated",
            &[&branch, &from],
        )));
    }
}

/// The light agentic turn body — the non-spawning core of [`spawn_agentic`].
///
/// Split out so the brain-routed chat dispatcher ([`run_routed_turn`]) can drive a
/// non-build turn (chat / explain / quick-edit / debug) INLINE from inside its OWN
/// already-spawned classification task, reusing this exact streaming path rather
/// than a second copy. The body is byte-for-byte the original; only the outer
/// `tokio::spawn(async move { … })` moved up into the wrapper.
async fn run_agentic(
    turn: AgenticTurn,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) {
    let AgenticTurn {
        task,
        spec,
        continue_session,
        session_id,
        fallback_model,
        project_root,
        director_build,
        host_cli,
        route,
        conversation,
    } = turn;
    {
        let label = spec.label();
        let model = route_model_for_spec(&spec, fallback_model);
        // Resolve the firmware route (HIGH #3 / MEDIUM #6). The brain-routed dispatch
        // already classified the turn and passes `Some(route)`; the queued-drain path
        // (`fire_agentic`) passes `None` — it deliberately does NOT re-consult the
        // brain — so we fall back to the router's OWN deterministic Tier-0 floor here
        // (`route(None, …)`), which sizes the firmware proportionally without a second
        // base call. Fail-open by contract: `route(None, …)` never errors and ignores
        // its `options` on the no-session path, so the throwaway `RunOptions` below is
        // only a carrier (its non-`requirement`/`project_root` fields are unused).
        let route = match route {
            Some(r) => r,
            None => {
                umadev_agent::route(None, &route_floor_options(&project_root, &task), &task).await
            }
        };
        // A workspace-mutating director build (`/run` or a chat 'build me X') that is
        // really driven by a HOST CLI: take the single-writer run-lock for the whole
        // turn so a full product build serializes with any other workspace-mutating
        // run, exactly like the legacy pipeline does (`run_continuous_block` /
        // `run_initial_block` both hold it). The guard lives for the task's scope
        // and drops on return. Fail-open: a lock held by a DIFFERENT live run is an
        // honest terminal abort (the same `ABORT_SENTINEL` the pipeline uses); any
        // other lock IO fails open inside `acquire_for_run` to an un-owned guard, so
        // a lock bug never blocks a legitimate build.
        //
        // LOW fix (tui-dispatch): the gate is `director_build && host_cli`, NOT
        // `director_build` alone. A `Build`-class verdict against a NON-host brain
        // (offline) stays on THIS light streaming path — it writes nothing the lock
        // protects, so grabbing the workspace lock / isolating a branch was pure
        // overhead (and could spuriously serialize against a real host run). A
        // normal free-text turn (chat / explain / quick-edit) takes NO lock either.
        let lock_and_isolate = director_build && host_cli;
        let _run_lock = if lock_and_isolate {
            match umadev_agent::run_lock::RunLock::acquire_for_run(&project_root) {
                Ok(g) => Some(g),
                Err(e) => {
                    sink.emit(EngineEvent::Note(format!(
                        "{ABORT_SENTINEL}{}",
                        block_abort_note(&e, &label)
                    )));
                    let _ = route_tx.send(RouteDecision::Failed(start_failed_note(&e)));
                    return;
                }
            }
        } else {
            None
        };
        // Git-as-trust (Wave 6): a HOST director build mutates the workspace →
        // isolate onto `umadev/<slug>` + baseline before any write (only for the
        // lock-holding host director path; a normal free-text turn AND a non-host
        // would-be build take no lock and are not isolated). Fail-open; idempotent.
        if lock_and_isolate {
            let slug = project_root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("project");
            if let Some((branch, from)) = umadev_agent::setup_run_isolation(&project_root, slug) {
                sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "trust.branch_isolated",
                    &[&branch, &from],
                )));
            }
        }
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
        // Reactive build for the light chat path: a chat turn that was NOT
        // dispatched as a build can still BECOME one if the base writes a file —
        // enable the reactive detector so the first write grabs the lock + isolates
        // + shows the `Build` intent card (see [`ReactiveBuild`] /
        // [`react_to_first_write`]). Disabled when the turn was ALREADY dispatched
        // as a build (`director_build` true) — that path grabbed the lock + isolated
        // up-front above, so a second reaction would be redundant. The context
        // internally no-ops for a non-host brain, so passing it is always safe.
        let reactive = (!director_build).then(|| Arc::new(ReactiveBuild::new(host_cli)));
        drive_agentic_stream(
            brain.as_ref(),
            &task,
            &model,
            &label,
            &project_root,
            director_build,
            &route,
            &conversation,
            &sink,
            &route_tx,
            reactive.as_ref(),
        )
        .await;
    }
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
///
/// Thin re-export of the canonical classifier in the agent crate
/// ([`umadev_agent::claims_code_changes`]) so the director build loop and this TUI
/// boundary share ONE source of truth — the wording lives in one place.
#[must_use]
pub fn claims_code_changes(text: &str) -> bool {
    umadev_agent::claims_code_changes(text)
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

/// The objective **source-present hard-gate** for a director-build (`/run`) turn —
/// Wave 1 of `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md` §5.
///
/// An explicit `/run` told the director to BUILD a full product. After it reports
/// done, this verifies the RESULT against reality: count the real source files in
/// the workspace (`acceptance::source_files` — the SAME bounded scan the pipeline's
/// no-source hard stop uses). When the director's reply CLAIMS it built / changed
/// code (`claims_code_changes`) but the workspace has **zero** real source files,
/// that is an honest failure — return a loud terminal-abort note (prefixed with
/// [`ABORT_SENTINEL`] so the bar paints a real aborted state) instead of letting a
/// no-op read as a clean success.
///
/// This checks RESULT, never route: a director that legitimately just answered
/// (no build claim) returns `None` (no gate fires), and a build that produced even
/// one real source file passes. **Fail-open:** never panics; the worst case is a
/// missing advisory, never a blocked turn. Returns `None` when the gate is
/// satisfied (or not applicable).
fn director_source_hardgate(project_root: &std::path::Path, reply: &str) -> Option<String> {
    // Only judge a reply that CLAIMS a build/change — a director that just
    // answered (e.g. "this is already implemented") is not failing by producing
    // no new source. This mirrors the agentic fact-line's claim heuristic so the
    // two reality checks agree on what "claimed work" means.
    if !claims_code_changes(reply) {
        return None;
    }
    let source = umadev_agent::acceptance::source_files(project_root);
    if source.is_empty() {
        Some(format!(
            "{ABORT_SENTINEL}[warn] 总监声称完成了构建,但工作区没有任何真实源码文件 —— \
             按客观核对这一轮判为未完成,请核对 / the director reported a build but the \
             workspace has ZERO real source files — objectively this run did not \
             produce code; treated as not done"
        ))
    } else {
        None
    }
}

/// The **reality scaffold** for an agentic turn — the part of the system prompt
/// that is NOT firmware: it UNLOCKS tools (read/edit files, run commands — the
/// whole point of the agentic path), hands the chat-vs-act judgement to the base
/// itself, injects the live git state, and hard-constrains the base to verify any
/// "what did I change" claim against the real disk/git state rather than reciting
/// unverified session intent.
///
/// **Why this is split from the firmware (HIGH #3 / MEDIUM #6):** the team
/// IDENTITY + craft/taste + JIT knowledge + pitfall memory + the repo-map slice
/// now come from [`umadev_agent::compose_firmware`], which sizes them by the
/// turn's typed [`RoutePlan`] (pure chat = identity only; a quick edit = + craft;
/// a build = every layer). The light agentic path PREPENDS that route-tiered
/// firmware, then appends THIS scaffold — so "固件每路径 + 大脑判档" holds without
/// the old `looks_like_work_request` keyword table deciding firmware richness.
///
/// `status`/`diff_stat` are the live git snapshots (either may be `None`). The
/// scaffold itself is constant (no work-class branch): the firmware above already
/// carries (or omits) the craft/knowledge by tier. Fail-open: a missing git state
/// just renders the "clean" line; never errors.
fn agentic_reality_scaffold(status: Option<&str>, diff_stat: Option<&str>) -> String {
    let mut p = String::from(
        "You are running inside the project's working \
         directory with FULL tool access.\n\n\
         DECIDE FOR YOURSELF how to handle the user's latest message — that judgement is \
         yours, not the shell's:\n\
         - If it is just conversation (a greeting, an opinion, a question you can answer \
           by talking, a follow-up) — simply REPLY, naturally, in the user's language. Do \
           NOT use tools or touch files for small talk.\n\
         - If it asks you to look at, inspect, explain, debug, review, change, or BUILD \
           something — actually DO it: read files, edit files, run commands. Do not refuse \
           to use your tools, and do not just describe what you would do — do the work.\n\
         You are one continuous session: keep the context of the whole conversation.\n\n\
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

/// Token budget for the conversation transcript UmaDev threads into each agentic
/// turn (Wave 5 / G11). Bounds prompt growth so a long chat can't blow the base's
/// context: the most-recent turns within this budget are kept, older ones drop
/// off, and the base's own `--resume` belt-and-suspenders still carries deeper
/// history on its side. Roughly six thousand tokens is a generous multi-turn
/// window without crowding out the system prompt and the user's current task.
const TRANSCRIPT_TOKEN_BUDGET: usize = 6_000;

/// Build the bounded prior-transcript to prepend to an agentic request: the
/// `conversation` messages, oldest to newest, EXCLUDING a trailing message that
/// duplicates the current `task` (the caller records the current user turn into
/// `conversation` BEFORE this runs, so its last entry is usually the task itself,
/// and sending it twice would double the ask). The kept window is the most-recent
/// suffix whose estimated token cost (chars over four, the project-wide heuristic)
/// fits `budget`. Fail-open: an empty conversation, or one that is only the
/// current task, yields an empty `Vec` so the request is the single-message form
/// exactly as before this wave.
fn bounded_transcript(conversation: &[Message], task: &str, budget: usize) -> Vec<Message> {
    // Drop a trailing user turn equal to the current task (avoid sending it twice).
    let task_trim = task.trim();
    let mut prior: &[Message] = conversation;
    if let Some(last) = prior.last() {
        if last.role == "user" && last.content.trim() == task_trim {
            prior = &prior[..prior.len() - 1];
        }
    }
    if prior.is_empty() {
        return Vec::new();
    }
    // Walk newest to oldest accumulating a token estimate; keep the suffix that
    // fits the budget. chars/4 mirrors `coach`/`director_loop::approx_tokens`.
    let mut kept_rev: Vec<Message> = Vec::new();
    let mut est: usize = 0;
    for m in prior.iter().rev() {
        let cost = (m.role.len() + m.content.len()) / 4 + 1;
        if est + cost > budget && !kept_rev.is_empty() {
            break;
        }
        est += cost;
        kept_rev.push(m.clone());
    }
    kept_rev.reverse();
    kept_rev
}

/// Front-load UmaDev's bounded conversation transcript onto a director-build
/// directive (Blocker #2 — a chat-originated build must inherit the conversation,
/// the same Wave 5 / G11 memory `drive_agentic_stream` threads for a light turn).
/// The bounded prior dialogue is rendered `role: content` oldest → newest and
/// prepended to `goal` via the trilingual `chat.director_build_with_history`
/// template. **Fail-open / unchanged for `/run`:** an empty `conversation` (or one
/// that is only the current task) yields the original `goal` byte-for-byte, so the
/// explicit-run directive is exactly as before. Pure + deterministic so the memory
/// fusion is unit-tested without opening a base session.
fn director_directive_with_history(
    conversation: &[Message],
    requirement: &str,
    goal: String,
) -> String {
    let prior = bounded_transcript(conversation, requirement, TRANSCRIPT_TOKEN_BUDGET);
    if prior.is_empty() {
        return goal;
    }
    let mut transcript = String::new();
    for m in &prior {
        transcript.push_str(&m.role);
        transcript.push_str(": ");
        transcript.push_str(&m.content);
        transcript.push_str("\n\n");
    }
    umadev_i18n::tlf(
        "chat.director_build_with_history",
        &[transcript.trim_end(), &goal],
    )
}

/// Whether THIS director build should front-load a goal-mode framing — and, if so,
/// whether the borrowed brain has NATIVE persistent-`/goal` support.
///
/// `goal_mode` is the `/goal`-command flag: every director build can OPT INTO the
/// goal framing (the universal enhancement — Claude Code's native persistent mode
/// is strictly stronger than a plain prompt loop), but the explicit `/goal` command
/// is the one that always carries it. The `UMADEV_NO_GOAL_MODE=1` opt-out (shared
/// verbatim with the legacy pipeline's `with_goal_mode`) suppresses it on every
/// path. When framing IS applied, the borrowed brain's
/// [`BrainCapabilities::persistent_goal`](umadev_runtime::BrainCapabilities) is read
/// from the backend id via [`umadev_host::driver_for`] — claude → native `/goal`,
/// codex / opencode → the prompt-level fallback.
///
/// **Fail-open by contract:** an unknown / unbuildable backend id (offline, a typo)
/// can't report capabilities → `None`, so NO goal framing is prepended and the
/// directive degrades to exactly today's behaviour. Reads the env once at the call
/// boundary (the build is about to start), never mid-loop.
fn resolve_goal_mode(backend: &str, goal_mode: bool) -> Option<bool> {
    if !goal_mode || std::env::var("UMADEV_NO_GOAL_MODE").as_deref() == Ok("1") {
        return None;
    }
    // Capability, not a host-id string: ask the driver what the brain can do. An
    // offline / unknown backend has no driver → no goal framing (fail-open).
    umadev_host::driver_for(backend).map(|d| d.capabilities().persistent_goal)
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
/// 4. **Director-build hard-gate** (only when `director_build`) — after the turn,
///    an objective source-present check (`acceptance::source_files`): a `/run`
///    that claimed a build but produced zero real source is reported honestly.
// A cohesive internal driver: every parameter is load-bearing context for the
// one streaming call + its four reality guards. Splitting it into a struct would
// only obscure the data flow, so the arg-count lint is allowed here.
#[allow(clippy::too_many_arguments)]
async fn drive_agentic_stream(
    brain: &dyn Runtime,
    task: &str,
    model: &str,
    label: &str,
    project_root: &std::path::Path,
    director_build: bool,
    route: &RoutePlan,
    conversation: &[Message],
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    // Reactive build context for the LIGHT CHAT path (see [`ReactiveBuild`]): the
    // first `Write`/`Edit`-family tool call flips this chat turn into a build
    // (run-lock + isolation + a `Build` intent card), without any up-front
    // classification. `None` disables the reaction entirely — the explicit `/run`
    // path, the queued-drain, and every unit test pass `None`, so they are
    // byte-for-byte unchanged.
    reactive: Option<&Arc<ReactiveBuild>>,
) {
    // (1) Reality injection — snapshot the live git state BEFORE the turn so the
    // base is anchored to the real tree, and keep `before` for the post-turn
    // diff. Both are `Option` (fail-open: git missing -> None -> guards no-op).
    let before = git_status_porcelain(project_root);
    let diff_stat = git_diff_stat(project_root);
    // Firmware (HIGH #3 / MEDIUM #6): the LIGHT path now injects UmaDev's firmware
    // through the SAME `compose_firmware` the director-build path uses, sized by
    // THIS turn's typed route — pure chat carries only the identity, a quick edit
    // adds the craft law, a build gets every layer (identity + craft + repo-map +
    // pitfall memory + JIT knowledge). This REPLACES the old
    // `looks_like_work_request` keyword decision + the ad-hoc `agentic_knowledge_
    // digest` retrieval, so "固件每路径 + 大脑判档" holds: the brain-router's class
    // (or a deterministic Tier-0 floor on the queued-drain path) sizes the firmware,
    // not a hardcoded keyword list. Fail-open: any retrieval failure degrades that
    // layer to empty (in the limit, just the identity).
    let firmware = umadev_agent::compose_firmware(project_root, route, task).await;
    // The reality scaffold (tool-unlock + chat-vs-act judgement + live git state +
    // the no-recitation contract) is appended AFTER the firmware. For ALL three
    // bases the light streaming path merges `request.system` into the one prompt
    // (`merge_prompt`), so prepending the firmware here is the light-path analogue
    // of how the director path injects it (claude `--append-system-prompt` natively;
    // codex/opencode front-loaded onto the directive) — the firmware always leads,
    // the scaffold's reality contract follows. Fail-open: an empty firmware leaves
    // just the scaffold, exactly the pre-firmware light-path behaviour.
    let scaffold = agentic_reality_scaffold(before.as_deref(), diff_stat.as_deref());
    let system = if firmware.trim().is_empty() {
        scaffold
    } else {
        format!("{}\n\n{scaffold}", firmware.trim_end())
    };

    // Wave 5 / G11: thread UmaDev's OWN bounded conversation transcript into the
    // request, oldest → newest, so the base sees the multi-turn dialogue from
    // UmaDev's side — not just whatever the base's `--resume` happens to hold. The
    // base `--resume` becomes belt-and-suspenders, not the only memory: a restart,
    // a switched base, or a host that forgot its session still carries forward this
    // transcript. `prior` is the conversation EXCLUDING the current user turn (the
    // caller already appended it before recording), bounded to a token budget so a
    // long history can't blow the prompt. Fail-open: an empty transcript yields the
    // single-message request exactly as before.
    let prior = bounded_transcript(conversation, task, TRANSCRIPT_TOKEN_BUDGET);
    let mut messages = prior;
    messages.push(Message {
        role: "user".to_string(),
        content: task.to_string(),
    });
    // The execution request: the bounded transcript + the user's raw task, tools
    // UNLOCKED, no max_tokens (so the base isn't cut off mid-loop). The system
    // prompt does NOT re-ban tools — it unlocks them and only adds the reality
    // contract. Keep a clone for the offline-empty fallback echo below.
    let request = CompletionRequest {
        model: model.to_string(),
        messages,
        max_tokens: None,
        temperature: None,
        system: Some(system),
    };
    // Keep a clone of the request ONLY for an offline brain, so the empty-body
    // fallback below can echo the user's ask — host-CLI turns (the hot path) skip
    // the clone entirely.
    let request_echo = brain.is_offline().then(|| request.clone());
    // Forward every stream event straight into the existing WorkerStream
    // render pipeline (tool calls + text deltas show live). A `Warning` event is
    // also latched into `truncated` so the terminal note can flag an incomplete
    // finish (the base hit a rate limit / retry / cut-off mid-loop).
    let truncated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream_sink = Arc::clone(sink);
    let truncated_flag = Arc::clone(&truncated);
    // Reactive-build captures for the stream closure (cloned `Arc`s so the closure
    // is `'static + Send + Sync`, as `complete_streaming` requires). `None` when
    // reactive build is disabled (non-chat path) — the write check then no-ops.
    let reactive_ctx = reactive.cloned();
    let reactive_root = project_root.to_path_buf();
    let reactive_sink = Arc::clone(sink);
    let on_event = move |ev: umadev_runtime::StreamEvent| {
        if matches!(ev, umadev_runtime::StreamEvent::Warning { .. }) {
            truncated_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // Reactive build: the FIRST `Write`/`Edit`-family tool call flips this chat
        // turn into a build (lock + isolate + a `Build` intent card). `react_to_
        // first_write` is one-shot + fail-open, so this is cheap on every later
        // write and a no-op when reactive build is disabled / already triggered.
        if let umadev_runtime::StreamEvent::ToolUse { name, .. } = &ev {
            if is_workspace_write_tool(name) {
                react_to_first_write(reactive_ctx.as_deref(), &reactive_root, &reactive_sink);
            }
        }
        stream_sink.emit(EngineEvent::WorkerStream { event: ev });
    };
    match brain.complete_streaming(request, &on_event).await {
        Ok(resp) => {
            // (0) Surface the base's REAL token usage for this turn so the UI's live
            // session total reflects true consumption (the base's own numbers), not
            // an estimate or the all-time ledger.
            sink.emit(EngineEvent::TurnUsage {
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
            });
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
            // The effective build-ness of this turn: a turn dispatched AS a build
            // (`director_build`), OR a light chat turn the reactive detector turned
            // into one when the base wrote its first file (`became_build`). Drives
            // the source hard-gate below AND the terminal `AgenticDone` (so the
            // Wave-5 session hand-back fires for a chat-promoted build too). Fail-open:
            // no reactive context → just the original `director_build`.
            let effective_build = director_build
                || reactive
                    .is_some_and(|r| r.became_build.load(std::sync::atomic::Ordering::SeqCst));
            // (4) Director-build hard-gate — the deterministic reality floor for
            // an explicit `/run` (Wave 1). The director was told to BUILD a full
            // product; after it reports done we OBJECTIVELY check whether real
            // source actually landed on disk (`acceptance::source_files`). If the
            // director claimed a build but the workspace has zero real source
            // files, that is an honest FAILURE — not a success to celebrate — so we
            // append a loud terminal abort note (carrying `ABORT_SENTINEL` so the
            // bar shows a real aborted state, like the pipeline's no-source hard
            // stop). This verifies RESULT, it does not dictate the route: a
            // director that legitimately only answered a question (claimed no build)
            // is left alone. Fail-open: skipped entirely for a non-build turn.
            if effective_build {
                if let Some(note) = director_source_hardgate(project_root, &reply) {
                    sink.emit(EngineEvent::Note(note));
                }
            }
            // Wave 5 / G11: offline chat must never read as silence. When the brain
            // owns no model (offline) and the streamed body came back empty, the
            // pipeline's empty-body template contract does NOT apply to a chat turn
            // — synthesize a context-aware, non-silent reply (echo the ask + the
            // concrete next step) and stream it so the transcript shows a real
            // answer instead of the bare "[agentic] done." marker. Fail-open: this
            // only fires offline + empty; a host-CLI turn is untouched.
            if let Some(echo) = request_echo.filter(|_| reply.trim().is_empty()) {
                let fallback = umadev_runtime::offline_chat_reply(&echo);
                sink.emit(EngineEvent::WorkerStream {
                    event: umadev_runtime::StreamEvent::Text {
                        delta: fallback.clone(),
                    },
                });
                reply = fallback;
            }
            // The body already streamed live; hand the assembled text to the
            // event loop ONLY to record it as the assistant turn. An empty body
            // (the base emitted only tool calls / a side-effect) is still a clean
            // finish — send AgenticDone with what we have so `thinking` clears
            // uniformly. Carry the turn's EFFECTIVE build-ness (dispatched as a build
            // OR reactively promoted into one) so the event loop drives the Wave-5
            // hand-back without a pre-spawn flag.
            let _ = route_tx.send(RouteDecision::AgenticDone {
                reply,
                director_build: effective_build,
            });
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

/// Fire a light tools-enabled agentic turn from current app state, and return its
/// `JoinHandle` so the event loop can park it in `run_task` (Ctrl-C aborts it).
/// Keeps `thinking` set — the stream feeds `WorkerStream` events, which reset the
/// stall clock just like a phase — and resumes the SAME chat session, so the turn
/// shares conversation memory. Marks `agentic_in_flight` so Ctrl-C routes to a real
/// task-abort instead of the fire-and-forget route interrupt.
///
/// This is the **queued-chat drain** path: a turn the user parked while a previous
/// turn was in flight. It is NOT re-classified through the brain-router (the parked
/// text fires straight as a light turn — `director_build: false`); a fresh message
/// the user types goes through `Action::Route` → [`run_routed_turn`], which DOES
/// classify. Keeping the drain light avoids a second brain consult on already-queued
/// input and matches the prior behaviour.
fn fire_agentic(
    app: &mut App,
    chat_session: &ChatSessionHolder,
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    task: String,
) -> tokio::task::JoinHandle<()> {
    let spec = app.brain_spec();
    let host_cli = matches!(spec, BrainSpec::HostCli(_));
    // Wave 5 deliverable 2: if a finished director session was just handed back to
    // chat, the FIRST follow-up chat turn resumes the base's MOST-RECENT session in
    // this dir (`--continue`) — that session IS the build, so "why did you build it
    // that way?" continues the same session with full context. `--continue` needs
    // `session_id = None` + `continue_session = true` (the driver maps no-id + resume
    // → `--continue`), so we DON'T mint a fresh chat id this turn. Consumed here
    // (one-shot). Fail-open: if the base can't `--continue`, it starts fresh.
    let handing_back = host_cli && app.run_session_handed_to_chat;
    let continue_session = app.host_chat_session_active || handing_back;
    let session_id = if host_cli && !handing_back {
        Some(app.ensure_chat_session_id())
    } else {
        None
    };
    app.run_session_handed_to_chat = false;
    // Wave 5 / G11: hand the base UmaDev's OWN bounded conversation transcript so
    // memory no longer depends solely on the base's `--resume` (a restart, a
    // switched base, or a host that lost its session would otherwise be amnesia).
    let conversation = app.conversation_snapshot();
    // Keep the waiting state alive through the (potentially long) tool loop.
    app.thinking = true;
    app.thinking_started = Some(std::time::Instant::now());
    app.last_output_at = None;
    app.tool_in_progress = false;
    app.agentic_in_flight = true;
    // A drained queued turn is light (never a director build) → no hand-back.
    app.director_run_in_flight = false;
    let mode = app.effective_trust_mode();
    let fallback_model = app.effective_model();
    let project_root = app.project_root.clone();
    if host_cli {
        app.host_chat_session_active = true;
    }
    // Host CLI: drain a parked turn over the SAME resident chat session (the latency
    // fix) — `send_turn` into the already-loaded process, no cold start. The session
    // is already primed (a queued turn always follows at least one prior turn), so
    // the transcript is belt-and-suspenders only. Offline: the legacy light path.
    if host_cli {
        let autonomous = mode.gates_auto_approve();
        tokio::spawn(drive_chat_session_turn(ChatSessionTurn {
            text: task,
            backend: spec.label(),
            model: fallback_model,
            project_root,
            conversation,
            mode,
            autonomous,
            chat_session: chat_session.clone(),
            sink: sink.clone(),
            route_tx: route_tx.clone(),
        }))
    } else {
        spawn_agentic(
            AgenticTurn {
                task,
                spec,
                continue_session,
                session_id,
                fallback_model,
                project_root,
                // The queued-drain turn is always light — a fresh message classifies
                // via `run_routed_turn`; a parked one does not re-consult the brain.
                director_build: false,
                host_cli,
                // No brain consult on the drain → `route: None`. `run_agentic`
                // resolves it to a deterministic Tier-0 floor route so the firmware is
                // still sized proportionally without a second base call.
                route: None,
                conversation,
            },
            sink.clone(),
            route_tx.clone(),
        )
    }
}

/// Everything the chat dispatcher ([`run_routed_turn`]) needs, all snapshotted from
/// `&mut App` on the UI thread BEFORE the task spawns — so the task never touches
/// app state (it runs concurrently with the event loop).
///
/// The chat turn drives the LIGHT path only (the reactive detector promotes it to a
/// build if the base writes a file), so this carries no `RunOptions` / autonomy
/// flag — those belong to the EXPLICIT `/run` director loop, a separate path.
struct RoutedTurnInputs {
    /// The user's free-text turn (already recorded into conversation memory).
    text: String,
    /// Which base drives the turn (always a `HostCli` when `host_cli` is true).
    spec: BrainSpec,
    /// `true` when a real base CLI is configured. Gates the reactive build
    /// (run-lock + isolation only mean something for a real host that mutates the
    /// workspace); a non-host brain streams the light path and never isolates.
    host_cli: bool,
    /// UmaDev's OWN bounded conversation transcript (Wave 5 / G11) — threaded into
    /// the light request so memory is not cold.
    conversation: Vec<Message>,
    /// Resume the same chat session (light path) — `host_chat_session_active`
    /// OR a just-handed-back director session.
    continue_session: bool,
    /// Pinned chat session id (light path; `None` when handing a `/run` back).
    session_id: Option<String>,
    /// Fallback model id for the light path when the spec carries none.
    fallback_model: String,
    /// Project root the base subprocess runs in.
    project_root: PathBuf,
    /// Trust tier for this turn — drives the persistent-session approval floor
    /// (the `NeedApproval` gate) and the autonomy flag the session opens with.
    /// An irreversible action is always confirmed regardless of tier (the
    /// always-on floor); the tier only governs the *reversible* gate posture.
    mode: umadev_agent::TrustMode,
}

/// Dispatch ONE free-text chat turn by driving the persistent session **once** on
/// the light streaming path — NO up-front classification, NO separate triage
/// subprocess. This is the fix for the ~30s first-reply latency: the old path ran
/// a one-shot `route_via_brain` consult (a COLD `claude --print` ≈ several seconds)
/// to classify the turn, THEN cold-started a SECOND base process to actually answer
/// — two cold starts per message. The borrowed brain already HAS tools and IS the
/// judge of chat-vs-build; making it classify in a separate stateless call before
/// it is even allowed to answer was redundant latency that broke the "one
/// continuous session" contract (a chat turn should `--continue` the same dialogue,
/// not spin up a throwaway).
///
/// **The base decides chat-vs-build by ACTING, and UmaDev reacts:** the turn opens
/// the chat session once (fast, `--continue`), streams the base's own agentic loop,
/// and watches the tool calls. A pure-reply turn stays a fast, light chat; the
/// FIRST `Write`/`Edit`-family tool call flips the turn into a build via the
/// reactive detector in [`drive_agentic_stream`] (run-lock + branch isolation + a
/// `Build` intent card), with NO pre-commitment. So:
/// - the intent card shown at dispatch is the behaviour-derived "对话" (Chat) card —
///   honest at t=0, when the only signal is "the user typed a message";
/// - the firmware is sized by a fixed proportional default ([`light_default_route`]
///   — identity + craft + repo-map, but not the heavy full-build layers), since the
///   turn is no longer pre-classified;
/// - a turn that turns out to build re-surfaces a "构建中" card the moment the first
///   file is written (see [`react_to_first_write`]).
///
/// The full plan / team-schedule / finalize delivery flow is the EXPLICIT `/run`
/// director loop (unchanged) — a chat-build runs the base's OWN internal
/// PM→design→code→QA, and the user opts into the heavy flow with `/run`.
///
/// **Why a spawned task:** the event loop's `Action::Route` arm runs inline on the
/// UI thread, so any `.await` there would freeze the terminal. The arm sets the
/// immediate UI state + snapshots app inputs, then spawns this; dispatch returns
/// instantly and the UI keeps redrawing the "thinking…" state from `engine_rx`.
///
/// **Fail-open throughout:** the session failing to open / a streaming error is a
/// terminal `Failed` (the held session is dropped so the next turn re-opens a fresh
/// one); a reactive isolation that can't run leaves the turn in place. The shell
/// never wedges.
async fn run_routed_turn(
    inputs: RoutedTurnInputs,
    chat_session: ChatSessionHolder,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) {
    let RoutedTurnInputs {
        text,
        spec,
        host_cli,
        conversation,
        continue_session,
        session_id,
        fallback_model,
        project_root,
        mode,
    } = inputs;

    // ── Host CLI: the PERSISTENT chat-session path (the latency fix). ──────────
    // A real host CLI keeps ONE base session resident across the whole
    // conversation: the base is spawned once, its 7 MCP servers load once, and
    // UmaDev's firmware is injected once (`--append-system-prompt`). Every later
    // chat message only feeds `send_turn` + drains the stream — no per-message
    // `claude --print` cold start (which re-loaded all MCP servers each time and
    // was the ~30-60s first-reply latency). See [`drive_chat_session_turn`].
    if host_cli {
        let autonomous = mode.gates_auto_approve();
        drive_chat_session_turn(ChatSessionTurn {
            text,
            backend: spec.label(),
            model: fallback_model,
            project_root,
            conversation,
            mode,
            autonomous,
            chat_session,
            sink,
            route_tx,
        })
        .await;
        return;
    }

    // ── Offline / non-host brain: the legacy LIGHT path (unchanged). ───────────
    // An offline runtime owns no `BaseSession` (no resident process to keep), so
    // it stays on the single-shot streaming path. The behaviour-derived intent
    // card is dropped here too — the user asked to remove the chat intent card,
    // and the offline path never reactively builds (it writes nothing real).
    run_agentic(
        AgenticTurn {
            task: text,
            spec,
            continue_session,
            session_id,
            fallback_model,
            project_root,
            director_build: false,
            host_cli,
            route: Some(light_default_route()),
            conversation,
        },
        sink,
        route_tx,
    )
    .await;
}

/// Everything one persistent-session chat turn needs, snapshotted so the spawned
/// task never touches `&mut App`. Bundled to keep the driver's signature sane.
struct ChatSessionTurn {
    /// The user's free-text turn (already recorded into conversation memory).
    text: String,
    /// Backend id of the host CLI driving the resident session.
    backend: String,
    /// Fallback model id (the session uses the base's own configured model).
    model: String,
    /// Project root the resident base subprocess runs in.
    project_root: PathBuf,
    /// UmaDev's OWN bounded conversation transcript (Wave 5 / G11) — front-loaded
    /// onto the FIRST directive of a freshly-opened session so the resident base
    /// inherits the prior dialogue even across a restart / switched base; the
    /// session's own native memory carries later turns.
    conversation: Vec<Message>,
    /// Trust tier — the persistent-session approval floor (the `NeedApproval` gate).
    mode: umadev_agent::TrustMode,
    /// Whether the session opens autonomous (`auto` tier writes unattended) — the
    /// base still raises a `NeedApproval` for an irreversible action regardless.
    autonomous: bool,
    /// The resident chat session, held across the whole conversation. `None` until
    /// the pre-load (or the first turn's lazy-open) lands it; parked back after each
    /// `TurnDone`.
    chat_session: ChatSessionHolder,
    /// Live event sink (the same `WorkerStream` render path the director uses).
    sink: Arc<ChannelSink>,
    /// Terminal-decision channel back to the event loop.
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
}

/// The path token of a tool call's raw input — the human-readable target shown in
/// the tool row (file path / command / url / pattern). A self-contained mirror of
/// the agent crate's internal `tool_call_target` (kept local so this TUI boundary
/// does not reach into `umadev-agent` internals). Pure + fail-open: an input with
/// none of the known keys renders an empty target.
fn session_tool_target(input: &serde_json::Value) -> String {
    for key in ["file_path", "path", "command", "url", "pattern"] {
        if let Some(s) = input.get(key).and_then(serde_json::Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

/// Pull the next [`SessionEvent`], bounding the wait by `idle` so a base that HANGS
/// (stops emitting but never exits) can't block the drain forever — pure silence
/// past the window settles the turn instead of wedging `thinking`. ANY event resets
/// the clock (a long compile/test turn survives as long as it emits SOMETHING). The
/// local analogue of the agent crate's idle watchdog. `Ok(None)` = session ended,
/// `Err(())` = idle-timed-out (caller settles), `Ok(Some(ev))` = a real event.
#[allow(clippy::result_unit_err)]
async fn next_chat_event_idle(
    session: &mut dyn umadev_runtime::BaseSession,
    idle: Duration,
) -> Result<Option<umadev_runtime::SessionEvent>, ()> {
    match tokio::time::timeout(idle, session.next_event()).await {
        Ok(ev) => Ok(ev),
        Err(_) => Err(()),
    }
}

/// Idle ceiling for one persistent-session chat turn — the same generous window the
/// director loop uses, so a long agentic turn (deep web research, a big build) is
/// never killed while it is still streaming, but a true silent hang settles.
const CHAT_SESSION_IDLE: Duration = Duration::from_secs(300);

/// Enrich a base-failure reason with the base's OWN stderr tail + exit status, so
/// "base session idle" / "ended mid-turn" tells the user WHY. A broken base
/// model/login config writes its error to stderr (previously discarded) and never
/// to stdout, so the bare reason gave no diagnosis. Fail-open: a missing tail / exit
/// → the bare reason. The tail is bounded (last 3 non-empty lines, ≤280 chars).
fn enrich_base_failure(
    base_msg: &str,
    exit: Option<std::process::ExitStatus>,
    stderr_tail: Option<String>,
) -> String {
    let mut msg = match exit {
        Some(s) if !s.success() => format!("{base_msg}(base 进程已退出: {s})"),
        _ => base_msg.to_string(),
    };
    if let Some(tail) = stderr_tail {
        let lines: Vec<&str> = tail
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let start = lines.len().saturating_sub(3);
        let snippet: String = lines[start..].join(" | ").chars().take(280).collect();
        if !snippet.is_empty() {
            msg = format!("{msg} — base stderr: {snippet}");
        }
    }
    msg
}

/// A WARM resident chat session: the live base process paired with the firmware it
/// was opened with (kept so a non-claude base — which has no native system slot —
/// can re-prefix that firmware onto its FIRST directive). The session is spawned,
/// its MCP servers loaded, and (for claude) the firmware injected natively via
/// `--append-system-prompt`, but it has NOT yet seen any user turn — it is the
/// pre-loaded brain a chat message just `send_turn`s into.
struct WarmChatSession {
    /// The live base session, parked into the holder until the first turn.
    session: Box<dyn umadev_runtime::BaseSession>,
    /// The firmware the session was opened with (`None` when empty / composed to
    /// nothing). Re-prefixed onto the first directive ONLY for a non-claude base —
    /// claude already got it natively, so it is never restated there.
    firmware: Option<String>,
}

/// Open a WARM resident chat session — spawn the base, load its MCP servers, and
/// inject UmaDev's firmware ONCE — WITHOUT sending any turn. This is the work the
/// background pre-load does at launch (so the cold start is paid while the user
/// reads the welcome screen / types) and also the lazy-open the first chat turn
/// falls back to if the pre-load hasn't landed yet.
///
/// Composes the firmware ONCE (identity + craft + a one-time repo-map slice via the
/// light route — NOT re-retrieved per turn) and injects it natively via
/// `session_for`'s `--append-system-prompt`. The conversation transcript is NOT
/// folded in here — a warm session carries no turn yet; the first real turn
/// front-loads history onto its own `send_turn` (see [`first_chat_directive`]).
///
/// Returns the warm session (process + firmware), or the open error (the caller
/// maps it to an honest terminal `Failed`, or — on the pre-load path — simply drops
/// it so the first turn lazily re-opens). Fail-open by contract.
async fn open_warm_chat_session(
    backend: &str,
    model: &str,
    project_root: &std::path::Path,
    autonomous: bool,
) -> Result<WarmChatSession, umadev_runtime::SessionError> {
    // The firmware is keyed off the project + the light route only — NOT the user's
    // message — so it is identical whether composed at pre-load (no message yet) or
    // at lazy-open. Empty query is fine: the light route pulls identity + craft +
    // repo-map, none of which depend on the requirement text.
    let route = light_default_route();
    let firmware = umadev_agent::compose_firmware(project_root, &route, "").await;
    let firmware = (!firmware.trim().is_empty()).then_some(firmware);
    let session = umadev_host::session_for(
        backend,
        project_root,
        model,
        autonomous,
        firmware.as_deref(),
    )
    .await?;
    Ok(WarmChatSession { session, firmware })
}

/// Build the FIRST directive sent into a freshly-opened warm session: front-load
/// UmaDev's bounded conversation transcript so the new session inherits the prior
/// dialogue (across a restart / switched base), and — for codex / opencode, which
/// have no native system slot — prefix the firmware onto this first directive too
/// (the universal fail-open path). For claude the firmware is already native, so
/// the directive carries only the history, never restating it.
///
/// `firmware` is the warm session's firmware (the same value `open_warm_chat_session`
/// returned); `None` / claude → history only.
fn first_chat_directive(
    firmware: Option<&str>,
    backend: &str,
    conversation: &[Message],
    text: &str,
) -> String {
    let with_history = director_directive_with_history(conversation, text, text.to_string());
    match firmware {
        Some(fw) if backend != "claude-code" => format!("{fw}\n\n---\n\n{with_history}"),
        _ => with_history,
    }
}

/// Spawn a BACKGROUND pre-load of the resident chat session — the core of the
/// latency fix. Called the instant UmaDev lands on the chat surface with a host CLI
/// configured (at launch, and after the picker / a `/backend` switch resolves a
/// base), so the base is spawned, its MCP servers loaded, and the firmware injected
/// WHILE the user reads the welcome screen and types — not on the critical path of
/// the first message. By the time the user sends, the holder already has a `Warm`
/// session and the first reply is just `send_turn` + drain (≈ as fast as later
/// turns), instead of paying a full `claude --print` cold start (the ~30-60s reload
/// of all MCP servers + firmware that was the first-reply latency).
///
/// Snapshots the backend / model / root / autonomy on the UI thread (the caller),
/// then opens the warm session off-thread and parks it into the holder. **Fully
/// fail-open + idempotent:**
/// - a non-host (offline) brain is a no-op (nothing resident to keep);
/// - if the holder is ALREADY populated (a prior pre-load landed, or a live turn is
///   mid-flight having taken it), the freshly-opened warm session is dropped — never
///   replacing a live/primed session or racing two opens into one slot;
/// - an open error simply leaves the holder empty, so the first chat turn lazily
///   re-opens exactly as before. The pre-load can NEVER wedge the shell or surface
///   an error — it only ever makes the first turn faster.
fn spawn_chat_session_preload(
    backend: Option<&str>,
    model: String,
    project_root: PathBuf,
    autonomous: bool,
    holder: ChatSessionHolder,
) {
    // Only a real host CLI keeps a resident session (offline owns no process). The
    // authoritative id list is `umadev_host::BACKEND_IDS` (the three first-class
    // bases); anything else (offline / unknown) is a no-op.
    let Some(backend) = backend.filter(|b| umadev_host::BACKEND_IDS.contains(b)) else {
        return;
    };
    let backend = backend.to_string();
    tokio::spawn(async move {
        // Open OUTSIDE the lock so the (slow) MCP/firmware load never holds the
        // mutex a live turn might need — then take the lock only to park it.
        // Fail-open: a failed open is dropped here (the `if let` skips it), leaving
        // the holder empty so the first turn lazily re-opens. No error surfaced.
        if let Ok(mut warm) =
            open_warm_chat_session(&backend, &model, &project_root, autonomous).await
        {
            let mut guard = holder.lock().await;
            // Don't clobber a session that arrived first (another pre-load) or a live
            // turn that already took the slot — close the extra one instead.
            if guard.is_some() {
                drop(guard);
                let _ = warm.session.end().await;
            } else {
                *guard = Some(ResidentChat::Warm(warm));
            }
        }
    });
}

/// Drive ONE chat turn over the **resident** base session — the latency fix.
///
/// Opens the session lazily on the FIRST turn (firmware composed ONCE and injected
/// natively via `session_for`'s `--append-system-prompt`; the conversation
/// transcript front-loaded onto this first directive), then REUSES it on every
/// later turn: each turn is just `send_turn` + a drain of [`SessionEvent`]s. The
/// base is spawned once, its MCP servers load once, the firmware is injected once —
/// removing the per-message `claude --print` cold start.
///
/// The drain mirrors the director loop's [`SessionEvent`] → [`EngineEvent`] mapping
/// (the SAME `WorkerStream` render path), so tool calls + text stream live exactly
/// as before. Three behaviours ride the drain, all fail-open:
/// - **Reactive build** — the first `Write`/`Edit`-family `ToolCall` flips the turn
///   into a build (run-lock + branch isolation + a `Build` intent card), with NO
///   up-front classification (the base decides by ACTING);
/// - **Trust gate** — a `NeedApproval` is answered by the always-on irreversible
///   floor (`requires_confirmation`): an irreversible action is denied with a note,
///   everything else allowed (so a guarded turn isn't wedged headless);
/// - **Settle** — `TurnDone` ends the drain, parks the LIVE session back into the
///   holder for the next turn, runs the post-turn fact line + the source hard-gate
///   (for a reactively-promoted build), and sends the terminal `AgenticDone`.
///
/// **Fail-open by contract:** a session that can't open, a `send_turn` that fails,
/// or a session that dies mid-drain is an honest terminal `Failed` — the holder is
/// cleared so the NEXT turn re-opens a fresh session. It never panics, and the
/// conversation transcript UmaDev holds re-primes a fresh session after a loss.
async fn drive_chat_session_turn(turn: ChatSessionTurn) {
    let ChatSessionTurn {
        text,
        backend,
        model,
        project_root,
        conversation,
        mode,
        autonomous,
        chat_session,
        sink,
        route_tx,
    } = turn;

    // Pre-turn git snapshot (fail-open: git missing → None → the fact line is
    // skipped). Used after the turn to report the real changed-file set.
    let before = git_status_porcelain(&project_root);

    // Take the resident session, or lazily open a fresh one. Three cases:
    //   - `Primed`: a session that already drove a turn — reuse it BARE (its own
    //     native memory carries the dialogue; firmware + MCP loaded long ago);
    //   - `Warm`: a session the background pre-load (or an earlier lazy-open)
    //     spawned but never turned — send its FIRST directive (front-load the
    //     transcript + re-prefix firmware for a non-claude base);
    //   - empty holder: lazily open a warm session NOW (the pre-load missed / a
    //     prior session was closed), then send its first directive.
    // The pre-load is what removes the first-reply latency: by the time the user
    // sends, the holder usually already has a `Warm` session (MCP + firmware loaded
    // off the hot path), so this turn is just `send_turn` + drain.
    let mut guard = chat_session.lock().await;
    let (mut session, first_directive) = match guard.take() {
        Some(ResidentChat::Primed(s)) => (s, text.clone()),
        Some(ResidentChat::Warm(w)) => {
            let directive =
                first_chat_directive(w.firmware.as_deref(), &backend, &conversation, &text);
            (w.session, directive)
        }
        None => match open_warm_chat_session(&backend, &model, &project_root, autonomous).await {
            Ok(w) => {
                let directive =
                    first_chat_directive(w.firmware.as_deref(), &backend, &conversation, &text);
                (w.session, directive)
            }
            Err(e) => {
                drop(guard);
                let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                    "continuous.tui_session_unavailable",
                    &[&e.to_string()],
                )));
                return;
            }
        },
    };
    drop(guard);

    // Send the directive into the resident session. A send error means the session
    // is dead — drop it (so the next turn re-opens) and report an honest failure.
    if let Err(e) = session.send_turn(first_directive).await {
        let _ = session.end().await;
        let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
            "route.failed",
            &[&backend, &e.to_string()],
        )));
        return;
    }

    // Reactive build: the FIRST workspace write flips this turn into a build. Built
    // host-on (a host session writes real files); fires its side-effects once.
    let reactive = Arc::new(ReactiveBuild::new(true));
    let mut text_acc = String::new();

    // Drain the turn. ANY event resets the idle clock; pure silence past the window
    // settles (the base hung). A `None` / a `Failed` status is an honest terminal.
    // The loop breaks with whether the finish was truncated (mid-stream cut-off).
    let truncated = loop {
        let ev = match next_chat_event_idle(session.as_mut(), CHAT_SESSION_IDLE).await {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                // Session ended mid-turn (process dead / EOF) — capture stderr + exit
                // status before dropping so the user sees WHY (e.g. a base that
                // crashed on a bad config), then report.
                let tail = session.stderr_tail();
                let exit = session.try_exit_status();
                let _ = session.end().await;
                let reason = enrich_base_failure("base session ended mid-turn", exit, tail);
                let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                    "route.failed",
                    &[&backend, &reason],
                )));
                return;
            }
            Err(()) => {
                // Idle hang — capture the base's OWN stderr + exit status FIRST (a
                // broken model/login config errors to stderr, never to stdout, so the
                // bare "base session idle" hid the cause), then interrupt + drop the
                // session and settle honestly with the diagnosis.
                let tail = session.stderr_tail();
                let exit = session.try_exit_status();
                let _ = session.interrupt().await;
                let _ = session.end().await;
                let reason = enrich_base_failure(
                    "base session idle(底座 300s 无输出 — 检查它的登录/模型配置)",
                    exit,
                    tail,
                );
                let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                    "route.failed",
                    &[&backend, &reason],
                )));
                return;
            }
        };
        match ev {
            umadev_runtime::SessionEvent::TextDelta(delta) => {
                text_acc.push_str(&delta);
                sink.emit(EngineEvent::WorkerStream {
                    event: umadev_runtime::StreamEvent::Text { delta },
                });
            }
            umadev_runtime::SessionEvent::ToolCall { name, input } => {
                // The FIRST workspace write flips the turn into a build (one-shot,
                // fail-open). The base decides chat-vs-build by ACTING.
                if is_workspace_write_tool(&name) {
                    react_to_first_write(Some(&reactive), &project_root, &sink);
                }
                let detail = session_tool_target(&input);
                // P1: forward the structured before/after for a Write/Edit so the
                // TUI draws a live diff card on the reactive session path too.
                // Fail-open: non-edit / unreadable input → None → plain row.
                let edit = umadev_runtime::ToolEdit::from_claude_tool_input(&name, &input);
                sink.emit(EngineEvent::WorkerStream {
                    event: umadev_runtime::StreamEvent::ToolUse { name, detail, edit },
                });
            }
            umadev_runtime::SessionEvent::ToolResult { ok, summary } => {
                sink.emit(EngineEvent::WorkerStream {
                    event: umadev_runtime::StreamEvent::ToolResult { ok, summary },
                });
            }
            umadev_runtime::SessionEvent::NeedApproval {
                req_id,
                action,
                target,
            } => {
                // Always-on irreversible floor — the SAME gate the director loop
                // applies: deny an irreversible action (with a note), allow the rest
                // so a guarded chat turn isn't wedged waiting on a human headlessly.
                let decision = if umadev_agent::requires_confirmation(mode, &action, &target) {
                    sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                        "continuous.dangerous_action_denied",
                        &[&action, &target],
                    )));
                    umadev_runtime::ApprovalDecision::Deny
                } else {
                    umadev_runtime::ApprovalDecision::Allow
                };
                if let Err(e) = session.respond(&req_id, decision).await {
                    let _ = session.end().await;
                    let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                        "route.failed",
                        &[&backend, &e.to_string()],
                    )));
                    return;
                }
            }
            umadev_runtime::SessionEvent::TurnDone { status, .. } => match status {
                umadev_runtime::TurnStatus::Completed => break false,
                // Truncated → the turn ended early (rate limit / retry / cut-off);
                // accept what landed but flag the "may be incomplete" caveat below.
                umadev_runtime::TurnStatus::Truncated => break true,
                umadev_runtime::TurnStatus::Interrupted => {
                    // ESC / abort. The session is still alive and primed — park it
                    // back so the next turn reuses it, and settle this turn as a
                    // (non-build) chat so `thinking` clears.
                    *chat_session.lock().await = Some(ResidentChat::Primed(session));
                    let _ = route_tx.send(RouteDecision::AgenticDone {
                        reply: String::new(),
                        director_build: false,
                    });
                    return;
                }
                umadev_runtime::TurnStatus::Failed(reason) => {
                    let _ = session.end().await;
                    let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                        "route.failed",
                        &[&backend, &reason],
                    )));
                    return;
                }
            },
        }
    };

    // Post-turn reality fact line — the real changed-file set, plus a `[warn]` when
    // the base CLAIMED changes the working tree does not show (fail-open: skipped if
    // git was unavailable for either snapshot). The SAME guard the light path runs.
    let changed = match (before.as_deref(), git_status_porcelain(&project_root)) {
        (Some(b), Some(a)) => Some(changed_files_between(b, &a)),
        _ => None,
    };
    if let Some(line) = agentic_fact_line(changed.as_deref(), claims_code_changes(&text_acc)) {
        sink.emit(EngineEvent::Note(line));
    }

    // Truncation honesty — a truncated finish gets the "may be incomplete" caveat so
    // it does not read as a clean, fully-flushed success.
    let mut reply = text_acc;
    if truncated {
        if !reply.is_empty() {
            reply.push('\n');
        }
        reply.push_str(
            "[warn] 本轮可能未完成或未全部落盘(底座中途告警/截断),请核对实际文件状态 \
             / turn may be incomplete or not fully written — verify the working tree",
        );
    }

    // The turn's EFFECTIVE build-ness: a pure-reply chat is false; a turn the
    // reactive detector promoted (the base wrote a file) is true — driving the
    // source hard-gate + the Wave-5 session hand-back.
    let became_build = reactive
        .became_build
        .load(std::sync::atomic::Ordering::SeqCst);

    // ARCHITECTURE UNIFICATION: a chat-build (`became_build`) earns the SAME flagship
    // post-build QC the explicit `/run` path runs — governance/design-slop scan +
    // critic-team review + bounded evidence-bearing rework (with the recalled
    // knowledge digest + prior pitfalls front-loaded) + usage/lessons capture. It runs
    // on the LIVE continuous session (BEFORE it is parked) so the fix turns drive the
    // SAME base that built, keeping its accumulated context. A pure chat reply (no
    // `became_build`) NEVER reaches here — it parks immediately below, staying light +
    // fast (no QC latency on conversation). The whole pass is fail-open inside
    // `run_post_build_qc` (a scan/fork/rework that can't run settles), and bounded by
    // the wall-clock run budget, so a chat turn is never wedged or slowed by QC.
    if became_build {
        // The deterministic source-present hard floor first (the objective "did
        // anything actually land" check), exactly as the light path runs it.
        if let Some(note) = director_source_hardgate(&project_root, &reply) {
            sink.emit(EngineEvent::Note(note));
        }
        // Build the run options for the QC pass from this chat turn's context (the
        // `requirement` is the user's free-text ask; the slug defaults to the dir).
        let qc_opts = RunOptions {
            project_root: project_root.clone(),
            requirement: text.clone(),
            slug: String::new(),
            model: model.clone(),
            backend: backend.clone(),
            design_system: String::new(),
            seed_template: String::new(),
            mode,
            strict_coverage: umadev_agent::strict_coverage_from_env(),
        };
        // Size the QC team from the reactive build route (the same behaviour-derived
        // `Build`/`Fast` route the intent card showed when the write was detected).
        let qc_route = reactive_build_route();
        let sink_dyn: Arc<dyn EventSink> = sink.clone();
        let qc_reply = umadev_agent::run_post_build_qc(
            session.as_mut(),
            &qc_opts,
            &sink_dyn,
            &qc_route,
            &reply,
        )
        .await;
        // A non-empty fix reply means rework actually ran a fix turn — surface its
        // final word as the turn's reply (the build + its corrections), like `/run`.
        if !qc_reply.trim().is_empty() {
            reply = qc_reply;
        }
    }

    // The turn finished cleanly (and any post-build QC ran on the live session). Park
    // the LIVE session back into the holder as `Primed` so the next chat message
    // reuses it BARE (resident base, MCP + firmware already loaded, native memory
    // carries the dialogue + the just-completed build + its QC fixes).
    *chat_session.lock().await = Some(ResidentChat::Primed(session));

    let _ = route_tx.send(RouteDecision::AgenticDone {
        reply,
        director_build: became_build,
    });
}

/// After a TERMINAL chat route outcome (`Chat` / `Failed`), fire the next turn
/// the user parked while the route was in flight, keeping same-session routing
/// serial. Returns `true` if a parked turn was dispatched.
/// After a terminal turn outcome, fire the next message the user parked while the
/// turn was in flight, keeping the single base session serial. Brain-driven: the
/// drained message goes straight to the tools-enabled agentic turn (the same path
/// as a fresh message), so a parked turn is handled identically. Returns the
/// in-flight handle so the caller can park it in `run_task` for Ctrl-C.
fn drain_next_queued_chat(
    app: &mut App,
    chat_session: &ChatSessionHolder,
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) -> Option<tokio::task::JoinHandle<()>> {
    let text = app.take_next_queued_chat()?;
    Some(fire_agentic(app, chat_session, sink, route_tx, text))
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

fn spawn_probe(sink: Arc<ChannelSink>) {
    tokio::spawn(async move {
        for status in umadev_host::probe_all().await {
            // The honest auth state (gap G10): a base can be installed yet not
            // logged in, in which case the picker must NOT show a green "ready".
            // `ready` now means installed AND confidently authenticated, so a
            // not-logged-in base is correctly blocked at commit.
            let auth_state = status.probe.auth_state();
            let ready = auth_state.is_logged_in();
            let (auth_tag, human) = match &status.probe {
                umadev_host::ProbeResult::Ready { version, .. } => {
                    let tag = match auth_state {
                        umadev_host::AuthState::LoggedIn => "logged_in",
                        umadev_host::AuthState::NotLoggedIn => "not_logged_in",
                        umadev_host::AuthState::NotInstalled => "not_installed",
                        umadev_host::AuthState::Unknown => "unknown",
                    };
                    (tag, version.clone())
                }
                umadev_host::ProbeResult::NotInstalled { program } => {
                    ("not_installed", format!("`{program}` not on PATH"))
                }
                umadev_host::ProbeResult::Unhealthy { detail } => ("unknown", detail.clone()),
            };
            // The base's own login / install commands so the picker can tell the
            // user EXACTLY what to run — read off the driver (fail-open to empty).
            let (login_cmd, install_cmd) = umadev_host::driver_for(status.id)
                .map(|d| {
                    (
                        d.login_hint().unwrap_or("").to_string(),
                        d.install_hint().unwrap_or("").to_string(),
                    )
                })
                .unwrap_or_default();
            // Pack the structured auth metadata onto `detail` (the BackendProbed
            // event can't grow fields — it lives in umadev-agent). The TUI
            // unpacks via `parse_probe_detail`; `|` and the \u{1} sentinels never
            // occur in a real version string / login command.
            let detail = format!(
                "{s}auth={auth_tag}|login={login_cmd}|install={install_cmd}{s}{human}",
                s = '\u{1}',
            );
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
    // Bound the blocking OSC 11 probe with a timeout via a worker thread. The probe
    // ends at the FIRST `stdin.read()`, whose deadline is only checked BETWEEN
    // reads — so a terminal that enables raw mode but never answers OSC 11 (conhost,
    // dumb terms, some SSH / screen / tmux) blocks that first read FOREVER, hanging
    // the launch before the first draw (P0). With the timeout the launch proceeds
    // with the default theme; the worker exits as soon as ANY byte arrives (on such
    // a terminal it may swallow the user's first keystroke after launch — a far
    // smaller cost than an indefinite hang, and only when OSC 11 went unanswered).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(theme_from_osc11_blocking());
    });
    rx.recv_timeout(Duration::from_millis(250)).ok().flatten()
}

/// The blocking OSC 11 background-color probe, run under a timeout by
/// [`theme_from_osc11`]. Writes `OSC 11 ?` and reads the `rgb:…` reply.
fn theme_from_osc11_blocking() -> Option<bool> {
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

/// Copy `text` to the system clipboard from inside the alternate screen, the way
/// Claude Code does: three paths, most-compatible first.
///
/// 1. **native** (preferred on a LOCAL session) — shell out to the OS clipboard
///    command, piping `text` to its stdin: `pbcopy` on macOS; on Linux/BSD try
///    `wl-copy`, then `xclip -selection clipboard`, then `xsel --clipboard
///    --input`. The first that spawns + exits cleanly wins. This is the path
///    that works in **macOS Terminal.app**, which has no OSC 52 support.
/// 2. **osc52** (fallback) — write the OSC 52 escape (`selection::osc52_sequence`)
///    to stdout. Used when the session is **remote** (`SSH_CONNECTION` /
///    `SSH_TTY` set — where a native command would target the far host, not the
///    user's terminal) OR when no native command was found / succeeded.
///
/// Every step is best-effort / fail-open: a missing binary, a spawn error, or a
/// non-zero exit silently falls through to OSC 52; nothing here panics or
/// blocks the UI loop.
fn copy_to_clipboard(text: &str) {
    use std::io::Write as _;

    // Pipe `text` to one native clipboard command's stdin; `true` only when it
    // spawned AND exited successfully. stdout/stderr are discarded.
    fn try_native(cmd: &str, args: &[&str], text: &str) -> bool {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        // (the outer `use` is for the OSC 52 fallback; this nested fn needs its own)
        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            // Ignore a broken pipe — we still wait + check the exit status below.
            let _ = stdin.write_all(text.as_bytes());
            // Drop stdin so the child sees EOF and can finish.
        }
        child.wait().is_ok_and(|s| s.success())
    }

    let is_remote =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();

    if !is_remote {
        // LOCAL → prefer the native OS command (the path that works everywhere,
        // including macOS Terminal.app). First success wins; return on it.
        let native_ok = if cfg!(target_os = "macos") {
            try_native("pbcopy", &[], text)
        } else {
            try_native("wl-copy", &[], text)
                || try_native("xclip", &["-selection", "clipboard"], text)
                || try_native("xsel", &["--clipboard", "--input"], text)
        };
        if native_ok {
            return;
        }
    }

    // REMOTE, or no native command available/succeeded → OSC 52 (the SSH-safe
    // fallback, and the only path the terminal itself can honor here).
    let seq = crate::selection::osc52_sequence(text);
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
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
    // Mouse capture is ON by default. We're on the alternate screen (no native
    // scrollback), where the terminal can't give us BOTH wheel-scroll AND native
    // click-drag copy — so UmaDev runs its OWN selection layer (the Claude Code
    // approach): capture the mouse, page the transcript on the wheel, render the
    // drag-selection highlight ourselves, and copy via OSC 52. `/mouse` toggles
    // capture OFF (DisableMouseCapture) for users who prefer the terminal's native
    // click-drag selection. The transcript also scrolls via the keyboard
    // (PageUp/PageDown, Home/End, Shift+↑/↓, Ctrl+Alt+U/D). Teardown + the panic
    // hook DisableMouseCapture regardless.
    stdout.execute(EnableMouseCapture).map_err(fail)?;
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
    // An aborted task that is winding down after a cancel. `abort()` only
    // SCHEDULES cancellation — the base subprocess keeps generating until the
    // task unwinds and drops its owned session (Child kill_on_drop / releases the
    // session lock). We must wait for that BEFORE the post-cancel cleanup (which
    // `try_lock`s the holders). But that wait must NOT block the render path, or
    // the UI freezes for up to the drain budget. So the handle is parked here and
    // drained in a dedicated `select!` branch while the loop keeps redrawing the
    // live "stopping…" state. `Some` only between the Esc/Ctrl-C keypress and the
    // drain completing.
    let mut cancel_drain: Option<tokio::task::JoinHandle<()>> = None;
    // The director's persistent base session for the continuous run path — ONE
    // brain held across the whole TUI session so context flows across gate
    // blocks (see `spawn_continuous_block`). Always empty unless the continuous
    // path is enabled; a parked session here is what makes a `Continue` block
    // resume the SAME session rather than re-prime a fresh one.
    let session_holder: SessionHolder = Arc::new(tokio::sync::Mutex::new(None));
    // The RESIDENT chat session — ONE base session kept alive across the whole
    // conversation on the host-CLI chat path (the latency fix). A BACKGROUND pre-load
    // (`spawn_chat_session_preload`, fired below at launch + after a backend switch)
    // lands a `Warm` session here while the user reads the welcome screen / types, so
    // the FIRST message is just `send_turn` + drain (no cold start). Parked back as
    // `Primed` after every turn so the next message reuses the SAME process instead of
    // cold-starting `claude --print`. Closed + cleared on cancel / quit / `/clear` /
    // a backend switch (see those arms). Distinct from `session_holder` (the
    // director-run continuous session) — chat and `/run` keep separate brains.
    let chat_session_holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(None));
    // Pre-load the resident chat session NOW if we launched straight into chat with a
    // host CLI already configured (a returning user — first launch lands on the
    // picker, which fires the pre-load on `Action::BackendChanged` once a base is
    // chosen). Fail-open + idempotent: a non-host brain / an open failure is a silent
    // no-op, leaving the first turn to lazily open exactly as before.
    if matches!(app.mode, crate::app::AppMode::Chat) {
        spawn_chat_session_preload(
            app.backend.as_deref(),
            app.effective_model(),
            app.project_root.clone(),
            continuous_autonomous(app.effective_trust_mode()),
            chat_session_holder.clone(),
        );
    }
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
                    // The brain-driven turn finished cleanly: the body already
                    // streamed live, so we only record it as the assistant turn
                    // (chat memory) + clear `thinking`, then fire the next message
                    // the user parked while this turn was in flight (serial — one
                    // base session, never two turns at once). The drained turn's
                    // handle is parked in `run_task` so Ctrl-C can abort it.
                    Some(RouteDecision::AgenticDone { reply, director_build }) => {
                        app.record_agentic_done(reply, director_build);
                        // Build-complete experience: an EFFECTIVE build (a `/run`
                        // build, a chat "build me X", or a chat turn the reactive
                        // detector promoted to a build) gets the "✅ done + what
                        // changed + here's the demo" card, and — for a web project —
                        // an auto-started dev server surfacing a clickable localhost
                        // URL. A plain chat / explain / quick-edit turn carries
                        // `director_build = false` and gets NO card (it just streamed
                        // its answer). Fail-open + non-blocking by contract.
                        if director_build {
                            finalize_build_completion(app, &sink);
                        }
                        run_task = drain_next_queued_chat(app, &chat_session_holder, &sink, &route_tx);
                    }
                    // The turn produced no usable reply (base init / stream error).
                    // `record_route_failed` clears `thinking`; then fire the next
                    // parked message so a failed turn doesn't strand the messages
                    // typed behind it.
                    Some(RouteDecision::Failed(note)) => {
                        app.record_route_failed(note);
                        run_task = drain_next_queued_chat(app, &chat_session_holder, &sink, &route_tx);
                    }
                    None => {}
                }
            }
            maybe_event = engine_rx.recv() => {
                if let Some(ev) = maybe_event {
                    let was_finished = app.finished;
                    app.apply_engine(ev);
                    // Delivery build just completed (the banner with its preview URL
                    // line was pushed inside `apply_engine`): auto-start the dev
                    // server too so the user gets a live, clickable demo — not just a
                    // printed address. Mirrors the chat/Fast build's auto-preview,
                    // but does NOT re-push a card (the Delivery banner already
                    // covers the "✅ done + what changed" summary). Fail-open: a
                    // non-web project detects no dev server and starts nothing.
                    if !was_finished && app.finished {
                        if let Some((url, command)) = app.auto_preview_target() {
                            start_preview_server(
                                &app.preview_server,
                                &sink,
                                &url,
                                &command,
                                &app.project_root,
                                false,
                            );
                        }
                    }
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
                    // Mouse → wheel scrollback + the in-app drag-to-select/copy layer
                    // (the Claude Code approach: WE render the selection highlight and
                    // copy via OSC 52, so both work on the alternate screen). Gated by
                    // `/mouse`; when capture is off these never arrive and the
                    // terminal's native selection/copy takes over. Only meaningful on
                    // the chat screen.
                    if app.mouse_scroll && matches!(app.mode, crate::app::AppMode::Chat) {
                        let (col, row) = (me.column, me.row);
                        match me.kind {
                            // Wheel → scroll ~3 rows per notch (the usual step).
                            MouseEventKind::ScrollUp => app.transcript_scroll_up(3),
                            MouseEventKind::ScrollDown => app.transcript_scroll_down(3),
                            // Left-down: begin a selection at this point (or clear it
                            // if the click is outside the transcript area).
                            MouseEventKind::Down(MouseButton::Left) => {
                                app.selection_begin(col, row);
                            }
                            // Left-drag: extend the live selection's cursor.
                            MouseEventKind::Drag(MouseButton::Left) => {
                                app.selection_extend(col, row);
                            }
                            // Left-up: if a non-empty selection was made, copy its
                            // text to the system clipboard via OSC 52 and toast. The
                            // highlight is KEPT so the user sees what was copied; a
                            // later Down elsewhere clears it. Fail-open: a write error
                            // is ignored, never blocking the loop.
                            MouseEventKind::Up(MouseButton::Left) => {
                                if let Some(text) = app.selection_finish_copy() {
                                    // The native clipboard helper spawns a child and
                                    // blocks on its stdin write + `wait()`; a wedged
                                    // pbcopy/xclip would otherwise stall this tokio
                                    // worker mid-render. Push it to the blocking pool
                                    // fire-and-forget (the OSC 52 fallback lives inside
                                    // it). Fail-open: errors are ignored.
                                    tokio::task::spawn_blocking(move || copy_to_clipboard(&text));
                                }
                            }
                            _ => {}
                        }
                    }
                } else if let Some(Ok(Event::Paste(pasted))) = &maybe_key {
                    // Bracketed paste (and CJK IME commits, which most terminals
                    // deliver as a paste burst): insert the text atomically at the
                    // cursor instead of letting it arrive as a scrambled stream of
                    // raw `Char` events. Without this the buffer and the rendered
                    // cursor desync — the reported "打字乱串 / 输入框乱跳".
                    // `handle_paste` also detects a dragged-in image PATH and turns
                    // it into an `[图片 N]` attachment chip (forwarded to the base as
                    // an `@<path>` mention on submit); plain text is inserted as-is.
                    app.handle_paste(pasted);
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
                            Action::None => {}
                            Action::BackendChanged => {
                                // A base was just chosen — either first-launch picker
                                // completion (the `None`→host case) or a `/backend`
                                // switch (both set `chat_session_dirty`). Close any
                                // stale resident session pinned to the OLD base, clear
                                // the dirty flag (so the bottom-of-loop close doesn't
                                // also fire), and PRE-LOAD a fresh warm session against
                                // the NEW base so the next chat message is hot. All
                                // best-effort / fail-open: a switch mid-turn can't reach
                                // here (rejected upstream), so this only closes a
                                // parked/idle session.
                                app.chat_session_dirty = false;
                                if let Some(stale) =
                                    chat_session_holder.lock().await.take()
                                {
                                    stale.end().await;
                                }
                                spawn_chat_session_preload(
                                    app.backend.as_deref(),
                                    app.effective_model(),
                                    app.project_root.clone(),
                                    continuous_autonomous(app.effective_trust_mode()),
                                    chat_session_holder.clone(),
                                );
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
                                    // Schedule cancellation, then get the WAIT off the
                                    // render path: park the aborting handle and let the
                                    // dedicated `cancel_drain` branch await it (bounded)
                                    // while the loop keeps drawing the "stopping…" state.
                                    // The post-cancel cleanup runs there, AFTER the task
                                    // has actually released its session lock — so the
                                    // try_lock cleanup never races a still-held lock.
                                    h.abort();
                                    cancel_drain = Some(h);
                                    // Keep the spinner alive + show an explicit
                                    // "stopping…" line so the cancel reads as in-progress
                                    // (not frozen) until the drain settles.
                                    app.begin_cancelling();
                                } else {
                                    // Nothing in flight — cancel is an immediate reset
                                    // (still drop any parked session + drain stale
                                    // events so a buffered reply can't resurrect state).
                                    if continuous_run_active {
                                        if let Ok(mut g) = session_holder.try_lock() {
                                            if let Some(mut s) = g.take() {
                                                let _ = s.end().await;
                                            }
                                        }
                                        continuous_run_active = false;
                                    }
                                    let parked = chat_session_holder
                                        .try_lock()
                                        .ok()
                                        .and_then(|mut g| g.take());
                                    if let Some(s) = parked {
                                        s.end().await;
                                    }
                                    while engine_rx.try_recv().is_ok() {}
                                    while route_rx.try_recv().is_ok() {}
                                    app.cancel_run();
                                }
                            }
                            Action::StartRun(req) | Action::StartGoal(req) => {
                                // `/goal <objective>` and `/run` BOTH ride this one
                                // director-build path (the orchestration that owns the
                                // plan / team / firmware / finalize). Both opt into goal
                                // mode (the universal enhancement — Claude Code's native
                                // persistent `/goal` is strictly stronger than a plain
                                // prompt loop), so the base gets a persistent-`/goal`
                                // framing — "keep working until the objective is met."
                                // `StartGoal` is a distinct Action only so the `/goal`
                                // command can carry its own usage / preflight in
                                // `slash_goal`; from here the build branch is shared
                                // byte-for-byte. The framing itself is applied inside
                                // `run_director_loop`, gated by the brain's capability +
                                // `UMADEV_NO_GOAL_MODE` (so it fully reverts).
                                let goal_mode = true;
                                // Wave 1 (docs/AGENT_WIELDS_BASE_ARCHITECTURE.md §5):
                                // an explicit `/run` is now the DIRECTOR-driven agentic
                                // path by default — the SAME engine a free-text message
                                // reaches — with the goal framed as a full commercial
                                // build the director orchestrates with its team however
                                // it judges fit, NOT the fixed 9-phase pipeline. The
                                // legacy pipeline is retained UNTOUCHED behind an
                                // explicit opt-in (`UMADEV_LEGACY_PIPELINE=1`) so the
                                // field reverts with no code change. A host CLI is
                                // required for the director path (it drives a real base
                                // session); offline / non-host brains and the legacy
                                // flag both fall through to the pipeline below.
                                let host_cli = matches!(app.brain_spec(), BrainSpec::HostCli(_));
                                let legacy = umadev_agent::legacy_pipeline_from_env();
                                if host_cli && !legacy {
                                    // DEFAULT (USB model): the director build loop.
                                    // `/run` opens a live base session and drives
                                    // `drive_director_loop`. The firmware (team
                                    // identity + craft) is injected; the base's body
                                    // builds the goal end to end with its OWN tools,
                                    // then UmaDev runs a read-only honesty/QC pass and
                                    // feeds any blocking findings back as a fix
                                    // directive (bounded) — no marker protocol, no
                                    // outside "summon". No fixed-phase continuous run
                                    // is in flight, so the gate-resume machinery stays
                                    // dormant. The run-lock + governance + source-
                                    // present hard-gate are held inside the loop's task.
                                    continuous_run_active = false;
                                    app.thinking = true;
                                    app.thinking_started = Some(std::time::Instant::now());
                                    app.last_output_at = None;
                                    app.tool_in_progress = false;
                                    app.agentic_in_flight = true;
                                    // Wave 5 deliverable 2: an explicit `/run` is a
                                    // director build — its session is handed back to
                                    // chat when it settles (see `record_agentic_done`).
                                    app.director_run_in_flight = true;
                                    // Remember the goal so the status bar + a later
                                    // revise see it, then build the run options for
                                    // this director build with the requirement set.
                                    app.requirement.clone_from(&req);
                                    let mut run_opts = current_run_options(app, &opts);
                                    run_opts.requirement = req;
                                    let autonomous = continuous_autonomous(run_opts.mode);
                                    run_task = Some(spawn_director_loop(
                                        run_opts,
                                        sink.clone(),
                                        route_tx.clone(),
                                        autonomous,
                                        // Explicit `/run` carries no prior chat to
                                        // inherit — the director build starts from the
                                        // goal alone (unchanged behaviour).
                                        Vec::new(),
                                        // `None` → `for_run` FORCES a Build (the
                                        // explicit-run contract), unchanged.
                                        None,
                                        // `/goal` (and `/run`) → persistent-goal framing,
                                        // gated by capability + opt-out inside the loop.
                                        goal_mode,
                                    ));
                                } else {
                                    // LEGACY (opt-in) or offline / non-host: drive the
                                    // fixed pipeline exactly as before. Continuous is
                                    // the default within the legacy pipeline itself; a
                                    // non-host brain stays single-shot `Block::Clarify`.
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
                                // Chat dispatch: drive the persistent session ONCE on the
                                // light path — NO up-front classification subprocess. The
                                // base HAS tools and IS the brain; it decides chat-vs-build
                                // by ACTING (a reply is chat; a file write is a build), and
                                // UmaDev reacts (`react_to_first_write`). This is the fix
                                // for the ~30s first reply: the old path cold-started a
                                // throwaway `claude --print` to classify, THEN cold-started
                                // a SECOND base to answer — two cold starts per message.
                                //
                                // This arm runs INLINE on the UI thread (the `keys.next()`
                                // branch of the `tokio::select!`), so any `.await` HERE
                                // would freeze the terminal — no redraw, no input. Instead
                                // we (a) set the immediate UI state here, (b) snapshot every
                                // `&mut App` input the turn needs (the spawned task can't
                                // touch app state — it runs concurrently with this loop),
                                // and (c) spawn ONE task (`run_routed_turn`) that emits the
                                // chat intent card + streams the turn off the render path.
                                // Dispatch returns instantly; the UI keeps redrawing the
                                // "thinking…" state from `engine_rx` events.
                                let host_cli =
                                    matches!(app.brain_spec(), BrainSpec::HostCli(_));
                                // Immediate UI state (same bookkeeping the `/run` arm sets):
                                // thinking + aliveness clock + agentic-in-flight, and a
                                // chat turn is never the continuous fixed-phase run.
                                continuous_run_active = false;
                                app.thinking = true;
                                app.thinking_started = Some(std::time::Instant::now());
                                app.last_output_at = None;
                                app.tool_in_progress = false;
                                app.agentic_in_flight = true;
                                // The turn is NOT pre-classified anymore (the base
                                // decides chat-vs-build by acting — see
                                // `run_routed_turn`), so `director_run_in_flight` stays
                                // false; the hand-back rides the terminal `AgenticDone`'s
                                // effective build-ness. Record the goal for the status
                                // bar / a revise.
                                app.director_run_in_flight = false;
                                app.requirement.clone_from(&text);
                                // ── Snapshot the session-continuity inputs on the UI
                                // thread (formerly computed inside `fire_agentic_routed`).
                                // Wave 5: a just-handed-back `/run` session continues via
                                // `--continue` (no fresh id); otherwise pin the stable chat
                                // id. Consume `run_session_handed_to_chat` here (one-shot).
                                let handing_back =
                                    host_cli && app.run_session_handed_to_chat;
                                let continue_session =
                                    app.host_chat_session_active || handing_back;
                                let session_id = if host_cli && !handing_back {
                                    Some(app.ensure_chat_session_id())
                                } else {
                                    None
                                };
                                app.run_session_handed_to_chat = false;
                                // Conversation snapshot stays taken on the UI thread so
                                // memory is never cold (Wave 5 / G11), passed into the task.
                                let conversation = app.conversation_snapshot();
                                let inputs = RoutedTurnInputs {
                                    text,
                                    spec: app.brain_spec(),
                                    host_cli,
                                    conversation,
                                    continue_session,
                                    session_id,
                                    fallback_model: app.effective_model(),
                                    project_root: app.project_root.clone(),
                                    mode: app.effective_trust_mode(),
                                };
                                // Resuming the chat session means the NEXT turn must also
                                // `--continue` it — set this now (the light path used to set
                                // it after spawning); a director build hands its session back
                                // via the terminal decision instead. (On the host-CLI path the
                                // RESIDENT `chat_session_holder` IS the live memory; this flag
                                // still gates the offline / `--continue` fallback.)
                                if host_cli {
                                    app.host_chat_session_active = true;
                                }
                                run_task = Some(tokio::spawn(run_routed_turn(
                                    inputs,
                                    chat_session_holder.clone(),
                                    sink.clone(),
                                    route_tx.clone(),
                                )));
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
                                // Manual `/preview`: start the server AND open the
                                // browser (the user explicitly asked to preview).
                                start_preview_server(
                                    &app.preview_server,
                                    &sink,
                                    &url,
                                    &command,
                                    &opts.project_root,
                                    true,
                                );
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
                        // The conversation context just changed (`/clear` set
                        // `chat_session_dirty`; a backend switch is handled inline in the
                        // `BackendChanged` arm, which clears the flag): close the RESIDENT
                        // chat session so the next chat turn opens a fresh one against the
                        // new context instead of carrying a stale live process. Best-effort
                        // `try_lock` so this never blocks the UI; fail-open. A chat turn
                        // that is mid-flight OWNS the session (holder is `None`), so this
                        // only closes a parked/idle one. Same base after `/clear`, so we
                        // PRE-LOAD a fresh warm session so the next message stays hot.
                        if app.chat_session_dirty {
                            app.chat_session_dirty = false;
                            let parked = chat_session_holder
                                .try_lock()
                                .ok()
                                .and_then(|mut g| g.take());
                            if let Some(s) = parked {
                                s.end().await;
                            }
                            spawn_chat_session_preload(
                                app.backend.as_deref(),
                                app.effective_model(),
                                app.project_root.clone(),
                                continuous_autonomous(app.effective_trust_mode()),
                                chat_session_holder.clone(),
                            );
                        }
                    }
                }
            }
            // Drain a cancelled task OFF the render path. The branch is only armed
            // while `cancel_drain` holds an aborting handle; it awaits the handle
            // (bounded so a wedged base can't hang the drain forever), then runs the
            // post-cancel cleanup that the `Action::Cancel` arm deferred. Until this
            // fires the loop keeps drawing the live "stopping…" state every tick.
            () = async {
                // SAFETY: the `if` guard guarantees `Some`.
                let h = cancel_drain.as_mut().expect("guarded by cancel_drain.is_some()");
                let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
            }, if cancel_drain.is_some() => {
                cancel_drain = None;
                // The aborted task has wound down (or the budget elapsed) — its
                // session lock is released, so the cleanup `try_lock`s succeed.
                // A continuous run was cancelled: close + drop the parked director
                // session so the NEXT run opens a fresh brain.
                if continuous_run_active {
                    if let Ok(mut g) = session_holder.try_lock() {
                        if let Some(mut s) = g.take() {
                            let _ = s.end().await;
                        }
                    }
                    continuous_run_active = false;
                }
                // ESC / Ctrl-C on a chat turn: the aborted task OWNED the resident
                // chat session, so the abort already dropped it. Best-effort close +
                // clear ANY session still parked (idle case, or a turn that hadn't
                // taken it yet) so a wedged session never lingers.
                let parked = chat_session_holder
                    .try_lock()
                    .ok()
                    .and_then(|mut g| g.take());
                if let Some(s) = parked {
                    s.end().await;
                }
                // Drain any events the aborted task already queued (a buffered
                // PipelineStarted / GateOpened) so they can't resurrect run state.
                while engine_rx.try_recv().is_ok() {}
                // Same for a route decision the aborted agentic turn already emitted:
                // a late `AgenticDone` / `Failed` would otherwise append a stale reply
                // AFTER the cancel reset.
                while route_rx.try_recv().is_ok() {}
                app.cancelling = false;
                app.cancel_run();
            }
            _ = tick.tick() => app.tick(),
        }

        if app.should_quit {
            break;
        }
    }
    // Quit / app teardown: close the resident chat session so its base subprocess
    // doesn't outlive the TUI. Best-effort; fail-open — never block the exit.
    let parked = chat_session_holder
        .try_lock()
        .ok()
        .and_then(|mut g| g.take());
    if let Some(s) = parked {
        s.end().await;
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

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
        }
    }

    /// A minimal [`RoutePlan`] of a given class for driving [`drive_agentic_stream`]
    /// / [`AgenticTurn`] in tests — the firmware tier is what these tests exercise on
    /// the light path (chat = identity only; a work class = + craft). Mirrors the
    /// agent crate's own `compose_firmware` test route builder.
    fn test_route(class: umadev_agent::RouteClass) -> RoutePlan {
        use umadev_agent::{Budget, Depth, RouteClass, Seat, TaskKind};
        let team = if matches!(class, RouteClass::Build) {
            vec![Seat::FrontendEngineer, Seat::QaEngineer]
        } else {
            Vec::new()
        };
        RoutePlan {
            class,
            kind: TaskKind::Greenfield,
            depth: Depth::Fast,
            team,
            scope: Vec::new(),
            needs_clarify: None,
            est_budget: Budget::for_route(class, Depth::Fast),
            confidence: 0.6,
        }
    }

    /// The light-path chat route (identity-only firmware tier).
    fn chat_route() -> RoutePlan {
        test_route(umadev_agent::RouteClass::Chat)
    }

    #[test]
    fn bounded_transcript_drops_the_duplicate_current_turn_and_keeps_order() {
        // The caller records the current user turn into `conversation` BEFORE the
        // turn fires, so the last entry equals `task` — it must NOT be sent twice.
        let conv = vec![
            msg("user", "hi"),
            msg("assistant", "hello"),
            msg("user", "build a todo app"),
        ];
        let prior = bounded_transcript(&conv, "build a todo app", TRANSCRIPT_TOKEN_BUDGET);
        // The trailing duplicate of the current task is dropped; the rest is in order.
        assert_eq!(prior.len(), 2);
        assert_eq!(prior[0].content, "hi");
        assert_eq!(prior[1].content, "hello");
    }

    #[test]
    fn bounded_transcript_is_empty_when_only_the_current_turn() {
        let conv = vec![msg("user", "just this")];
        assert!(bounded_transcript(&conv, "just this", TRANSCRIPT_TOKEN_BUDGET).is_empty());
        assert!(bounded_transcript(&[], "x", TRANSCRIPT_TOKEN_BUDGET).is_empty());
    }

    #[test]
    fn bounded_transcript_keeps_the_recent_suffix_within_budget() {
        // A tiny budget keeps only the most-recent message(s), oldest drop off,
        // and the result never sends the current `task` twice.
        let mut conv = Vec::new();
        for i in 0..50 {
            conv.push(msg("user", &format!("question number {i}")));
            conv.push(msg("assistant", &format!("answer number {i}")));
        }
        conv.push(msg("user", "current ask"));
        let prior = bounded_transcript(&conv, "current ask", 20);
        // Budget-bounded: a small suffix, not the whole 100-message history.
        assert!(!prior.is_empty());
        assert!(prior.len() < 100);
        // The kept window is the most-recent suffix (ends near the latest answer).
        assert!(prior.last().unwrap().content.contains("answer number 49"));
    }

    #[test]
    fn director_directive_is_unchanged_for_an_explicit_run() {
        // Blocker #2 fail-open invariant: an explicit `/run` passes an EMPTY
        // conversation → the directive is the goal byte-for-byte (no history block),
        // so the explicit-run path is exactly as before this change.
        let goal = "## Goal\nbuild a forum".to_string();
        let out = director_directive_with_history(&[], "build a forum", goal.clone());
        assert_eq!(out, goal, "no prior chat → directive unchanged");
        // A conversation that is ONLY the current task also yields the bare goal.
        let only_current = vec![msg("user", "build a forum")];
        let out2 = director_directive_with_history(&only_current, "build a forum", goal.clone());
        assert_eq!(out2, goal);
    }

    #[test]
    fn resolve_goal_mode_reads_the_brain_capability_per_backend() {
        // GOAL MODE wiring: a director build with `goal_mode` on resolves the
        // borrowed brain's `persistent_goal` capability from the backend id. ALL
        // THREE first-class bases (claude-code / codex / opencode) support a native
        // persistent `/goal` mode, so each resolves to Some(true).
        assert_eq!(resolve_goal_mode("claude-code", true), Some(true));
        assert_eq!(resolve_goal_mode("codex", true), Some(true));
        assert_eq!(resolve_goal_mode("opencode", true), Some(true));
    }

    #[test]
    fn resolve_goal_mode_is_fail_open_off() {
        // `goal_mode == false` (a build that did not opt in) → no framing.
        assert_eq!(resolve_goal_mode("claude-code", false), None);
        // An unknown / offline backend has no driver → no capability, no framing
        // (fail-open: the directive degrades to exactly today's behaviour).
        assert_eq!(resolve_goal_mode("nonexistent-backend", true), None);
        assert_eq!(resolve_goal_mode("offline", true), None);
    }

    #[test]
    fn resolve_goal_mode_honors_the_no_goal_opt_out() {
        // `UMADEV_NO_GOAL_MODE=1` suppresses goal framing on EVERY path (shared
        // verbatim with the legacy pipeline's `with_goal_mode`). The env guard is
        // global, so scope the mutation tightly and restore it.
        let prev = std::env::var("UMADEV_NO_GOAL_MODE").ok();
        std::env::set_var("UMADEV_NO_GOAL_MODE", "1");
        assert_eq!(resolve_goal_mode("claude-code", true), None);
        match prev {
            Some(v) => std::env::set_var("UMADEV_NO_GOAL_MODE", v),
            None => std::env::remove_var("UMADEV_NO_GOAL_MODE"),
        }
    }

    #[test]
    fn chat_director_build_inherits_the_conversation() {
        // Blocker #2 memory invariant: a build PROMOTED from chat front-loads the
        // prior dialogue (Wave 5 / G11) so the director's brain has the context the
        // user already gave — NOT a cold start. The current task is not duplicated.
        let conv = vec![
            msg("user", "I'm building a kanban board"),
            msg("assistant", "Nice — columns + drag-drop?"),
            msg("user", "yes, now build it"),
        ];
        let goal = "## Goal\nbuild it".to_string();
        let out = director_directive_with_history(&conv, "yes, now build it", goal);
        // The prior turns are present (memory bridged into the directive)...
        assert!(out.contains("I'm building a kanban board"));
        assert!(out.contains("columns + drag-drop"));
        // ...the goal still ends the directive...
        assert!(out.trim_end().ends_with("build it"));
        // ...and the trailing current task is NOT echoed a second time in history
        // (it appears once, as the goal — `bounded_transcript` drops the duplicate).
        assert_eq!(out.matches("yes, now build it").count(), 0);
    }

    /// A runtime spy that CAPTURES the request it was driven with, so a test can
    /// assert the conversation transcript was threaded into the messages.
    struct CapturingSpy {
        seen: Arc<std::sync::Mutex<Option<CompletionRequest>>>,
    }
    #[async_trait::async_trait]
    impl Runtime for CapturingSpy {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            unreachable!("agentic path uses streaming")
        }
        async fn complete_streaming(
            &self,
            req: CompletionRequest,
            on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            *self.seen.lock().unwrap() = Some(req);
            on_event(umadev_runtime::StreamEvent::Text { delta: "ok".into() });
            Ok(umadev_runtime::CompletionResponse {
                text: "ok".into(),
                id: "spy".into(),
                model: "spy".into(),
                usage: umadev_runtime::Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn agentic_turn_threads_the_conversation_transcript_into_the_request() {
        // Wave 5 / G11: UmaDev's OWN bounded transcript is sent every turn (not just
        // the single task), so memory no longer relies solely on the base's --resume.
        let seen = Arc::new(std::sync::Mutex::new(None));
        let spy = CapturingSpy {
            seen: Arc::clone(&seen),
        };
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let tmp = tempfile::TempDir::new().unwrap();
        let conversation = vec![
            msg("user", "我在做看板"),
            msg("assistant", "好的"),
            msg("user", "继续"),
        ];
        drive_agentic_stream(
            &spy,
            "继续",
            "m",
            "claude-code",
            tmp.path(),
            false,
            &chat_route(),
            &conversation,
            &sink,
            &route_tx,
            None,
        )
        .await;
        let req = seen.lock().unwrap().take().expect("request captured");
        // The request carries the prior dialogue + the current task (last), in order,
        // and does NOT duplicate the current "继续" turn.
        assert!(
            req.messages.len() >= 3,
            "transcript threaded: {:?}",
            req.messages
        );
        assert_eq!(req.messages[0].content, "我在做看板");
        assert_eq!(req.messages.last().unwrap().content, "继续");
        let continues = req.messages.iter().filter(|m| m.content == "继续").count();
        assert_eq!(continues, 1, "current turn must not be sent twice");
    }

    #[tokio::test]
    async fn offline_chat_never_returns_silence() {
        // Wave 5 / G11: an offline chat turn with an empty body gets a context-aware
        // fallback reply (echoing the ask), never the bare "[agentic] done." silence.
        let brain = OfflineRuntime::new(RuntimeKind::Anthropic);
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let tmp = tempfile::TempDir::new().unwrap();
        drive_agentic_stream(
            &brain,
            "帮我做个登录页",
            "m",
            "offline",
            tmp.path(),
            false,
            &chat_route(),
            &[],
            &sink,
            &route_tx,
            None,
        )
        .await;
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone { reply, .. }) => {
                assert!(!reply.trim().is_empty(), "offline reply must not be empty");
                assert!(reply.contains("帮我做个登录页"), "echoes the ask: {reply}");
            }
            other => panic!("expected a non-empty AgenticDone, got {other:?}"),
        }
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
    fn route_model_uses_launch_model_for_host_cli() {
        let spec = BrainSpec::HostCli("codex".to_string());

        assert_eq!(
            route_model_for_spec(&spec, "fallback-model".to_string()),
            "fallback-model"
        );
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
                edit: None,
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
            false,
            &chat_route(),
            &[],
            &sink,
            &route_tx,
            None,
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
            Ok(RouteDecision::AgenticDone { reply, .. }) => assert_eq!(reply, "no bug found"),
            other => panic!("expected AgenticDone, got {other:?}"),
        }
    }

    /// A minimal `Runtime` that plays the base's one-shot triage verdict for
    /// [`umadev_agent::router::route_via_brain`] — its `complete()` returns a JSON
    /// `BrainRoute` with the requested `class`, so a test can drive the brain-routed
    /// dispatcher without a live base. Not offline (so the router actually consults
    /// it); `complete_streaming` is unused on this path.
    struct RouteSpy {
        class: &'static str,
    }

    impl RouteSpy {
        fn with_class(class: &'static str) -> Self {
            Self { class }
        }
    }

    #[async_trait::async_trait]
    impl Runtime for RouteSpy {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            // The exact JSON shape the router's `BrainRoute` parses (extra keys are
            // ignored). A `build` class also carries a complexity so the route is a
            // real deliberate build, not a degenerate one.
            let text = format!(
                "{{\"class\":\"{}\",\"kind\":\"greenfield\",\"complexity\":\"complex\",\
                 \"needs\":[],\"scope\":[],\"confidence\":0.9}}",
                self.class
            );
            Ok(umadev_runtime::CompletionResponse {
                text,
                id: "route-spy".to_string(),
                model: "route-spy".to_string(),
                usage: umadev_runtime::Usage::default(),
            })
        }
        async fn complete_streaming(
            &self,
            _req: CompletionRequest,
            _on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            unreachable!("RouteSpy is only used for the one-shot triage `complete`")
        }
    }

    #[tokio::test]
    async fn director_build_is_decided_by_the_brain_class() {
        // `route_via_brain` is RETAINED for the explicit `/run` router consult — it
        // is no longer on the CHAT hot path (the chat surface drives the light path
        // ONCE with no triage subprocess; the base decides chat-vs-build by acting,
        // and `react_to_first_write` promotes a write into a build — see the
        // `reactive_*` tests). This test still locks `route_via_brain`'s
        // brain-authoritative verdict mapping where it IS used: a `build` verdict →
        // a Build route; a `chat` verdict → a non-mutating Chat route. The `RouteSpy`
        // plays the base's one-shot triage verdict.
        let build_spy = RouteSpy::with_class("build");
        let route = umadev_agent::router::route_via_brain(&build_spy, "做一个待办应用").await;
        assert!(
            matches!(route.class, umadev_agent::RouteClass::Build),
            "the brain's `build` verdict is honoured authoritatively"
        );
        assert!(
            matches!(route.class, umadev_agent::RouteClass::Build),
            "director_build = class == Build"
        );

        let chat_spy = RouteSpy::with_class("chat");
        let route = umadev_agent::router::route_via_brain(&chat_spy, "你好，能帮我做什么？").await;
        assert!(
            !matches!(route.class, umadev_agent::RouteClass::Build),
            "a greeting / capability question the brain calls `chat` is NOT a build"
        );
        assert!(
            !route.class.mutates_workspace(),
            "a chat verdict does not take the run-lock / mutate the workspace"
        );
    }

    #[tokio::test]
    async fn route_via_brain_fails_open_to_chat_when_brain_unavailable() {
        // Fail-open by design: there is NO keyword fallback on this path. An
        // unreachable brain (here: the offline runtime, which the router treats as
        // "can't consult") degrades to the lightest path — `Chat`, a pass-through to
        // the base — never a keyword guess that could mis-promote a greeting into a
        // 7-seat build. `director_build` is therefore false.
        let offline = OfflineRuntime::new(RuntimeKind::Anthropic);
        let route =
            umadev_agent::router::route_via_brain(&offline, "build me a full login app").await;
        assert!(
            !matches!(route.class, umadev_agent::RouteClass::Build),
            "an unreachable brain degrades to Chat, never a keyword-guessed Build"
        );
        assert!(
            !route.class.mutates_workspace(),
            "the fail-open Chat route does not mutate the workspace"
        );
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
            false,
            &chat_route(),
            &[],
            &sink,
            &route_tx,
            None,
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
    fn scaffold_injects_git_state_and_unlocks_tools() {
        // The reality SCAFFOLD (the non-firmware half of the light-path system
        // prompt) must keep tools UNLOCKED — never re-add the chat-route tool ban —
        // and embed the live git status plus a no-recitation contract. The firmware
        // (identity / craft / knowledge) is composed SEPARATELY by `compose_firmware`
        // and prepended in `drive_agentic_stream`.
        let status = concat!(" M crates/umadev-tui/src/lib.rs\n", "?? new.rs\n");
        let p = agentic_reality_scaffold(Some(status), Some("1 file changed"));
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
    fn scaffold_lets_the_brain_decide_chat_vs_act() {
        // The unified brain-driven path: instead of UmaDev classifying the message
        // up front, the scaffold hands that judgement to the base — reply to small
        // talk without tools, do the work when it needs tools. This is what makes
        // a greeting not waste tool calls and a real task actually get done.
        let p = agentic_reality_scaffold(None, None);
        let lower = p.to_lowercase();
        assert!(lower.contains("decide for yourself"));
        // It must cover BOTH arms: just reply to conversation, and do the work.
        assert!(lower.contains("just talking") || lower.contains("simply reply"));
        assert!(lower.contains("do not use tools") || lower.contains("small talk"));
        assert!(lower.contains("actually do it") || lower.contains("do the work"));
        // The scaffold itself carries NO firmware identity/craft — that is now the
        // job of `compose_firmware` (route-tiered), prepended separately. The
        // scaffold stays constant across classes (no work-class branch).
        assert!(
            !lower.contains("anti-ai-slop") && !p.contains("Lucide"),
            "the scaffold is the reality contract only — no firmware craft block"
        );
    }

    /// Drive the light path against a [`CapturingSpy`] and return the assembled
    /// `system` prompt the base would have received (firmware + scaffold).
    async fn captured_system_for_route(route: &RoutePlan, task: &str) -> String {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let spy = CapturingSpy {
            seen: Arc::clone(&seen),
        };
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let tmp = tempfile::TempDir::new().unwrap();
        drive_agentic_stream(
            &spy,
            task,
            "m",
            "claude-code",
            tmp.path(),
            matches!(route.class, umadev_agent::RouteClass::Build),
            route,
            &[],
            &sink,
            &route_tx,
            None,
        )
        .await;
        let req = seen.lock().unwrap().take().expect("request captured");
        req.system.unwrap_or_default()
    }

    #[tokio::test]
    async fn light_path_firmware_is_route_tiered_via_compose_firmware() {
        // HIGH #3 / MEDIUM #6: the LIGHT path now injects firmware through
        // `compose_firmware`, sized by the turn's route — NOT a keyword table.
        //
        // (1) A pure CHAT turn carries ONLY the always-on identity: no craft / no
        //     anti-slop / no knowledge — a greeting stays light.
        let chat = captured_system_for_route(&chat_route(), "你好").await;
        let chat_lower = chat.to_lowercase();
        assert!(chat_lower.contains("umadev"), "identity is always-on");
        assert!(
            !chat.contains("emoji") && !chat.contains("Lucide"),
            "a chat turn must NOT carry the engineering craft block (identity only)"
        );
        // The reality scaffold is still appended on every light turn.
        assert!(chat.contains("FULL tool access"));
        assert!(chat.contains("REALITY CONTRACT"));

        // (2) A BUILD-class turn (a non-host would-be build on the light path) gets
        //     the FULL firmware: identity + the team's craft/anti-slop.
        let build =
            captured_system_for_route(&test_route(umadev_agent::RouteClass::Build), "做一个登录页")
                .await;
        let build_lower = build.to_lowercase();
        assert!(build_lower.contains("umadev"));
        assert!(
            build.contains("emoji") && (build.contains("Lucide") || build.contains("icon library")),
            "a build turn carries the team's craft (anti-AI-slop) firmware"
        );
        // No marker/lever syntax is ever taught to the base (USB model).
        assert!(!build.contains("<<<umadev:"));
    }

    #[tokio::test]
    async fn light_path_quick_edit_carries_craft_but_chat_does_not() {
        // A QuickEdit (a small work turn) sits between chat and build: it carries the
        // craft law (so a small edit still respects the visual + engineering moat)
        // but pays for no full build ceremony. Pure chat carries neither.
        let edit =
            captured_system_for_route(&test_route(umadev_agent::RouteClass::QuickEdit), "改个文案")
                .await;
        assert!(
            edit.contains("emoji"),
            "a quick edit carries the compact craft law"
        );
        let chat = captured_system_for_route(&chat_route(), "谢谢").await;
        assert!(
            !chat.contains("emoji"),
            "pure chat must NOT carry the craft law"
        );
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
        drive_agentic_stream(
            &spy,
            "do it",
            "m",
            "claude-code",
            &path,
            false,
            &chat_route(),
            &[],
            &sink,
            &route_tx,
            None,
        )
        .await;

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
            false,
            &chat_route(),
            &[],
            &sink,
            &route_tx,
            None,
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
        drive_agentic_stream(
            &spy,
            "go",
            "m",
            "claude-code",
            &path,
            false,
            &chat_route(),
            &[],
            &sink,
            &route_tx,
            None,
        )
        .await;

        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone { reply, .. }) => {
                let incomplete = reply.contains("未完成") || reply.contains("incomplete");
                assert!(
                    reply.contains("[warn]") && incomplete,
                    "a truncated turn must flag possible incompleteness, got: {reply}"
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
        drive_agentic_stream(
            &spy,
            "go",
            "m",
            "claude-code",
            &path,
            false,
            &chat_route(),
            &[],
            &sink,
            &route_tx,
            None,
        )
        .await;

        // No [warn]/fact Note despite a loud claim — git was unavailable.
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                let leaked = n.contains("[warn]") || n.contains("文件变更");
                assert!(!leaked, "fail-open: no fact/warn line outside a git repo");
            }
        }
        // The turn still finishes cleanly — a non-director turn carries
        // `director_build: false` (no session hand-back).
        assert!(matches!(
            route_rx.try_recv(),
            Ok(RouteDecision::AgenticDone {
                director_build: false,
                ..
            })
        ));
    }

    // ── Wave 1: the director-build (`/run`) source-present hard-gate ───────

    #[test]
    fn director_hardgate_aborts_on_claimed_build_with_zero_source() {
        // The deterministic floor: the director claims a build but the workspace
        // has ZERO real source files -> an honest, loud terminal abort (carrying
        // the ABORT_SENTINEL), never a clean success.
        let tmp = tempfile::TempDir::new().unwrap();
        let note = director_source_hardgate(tmp.path(), "I implemented the whole login page")
            .expect("a claimed build with no source must trip the hard-gate");
        assert!(
            note.starts_with(ABORT_SENTINEL),
            "carries the abort sentinel"
        );
        assert!(note.contains("[warn]"));
        assert!(note.contains("ZERO real source") || note.contains("没有任何真实源码"));
    }

    #[test]
    fn director_hardgate_passes_when_real_source_exists() {
        // A build that produced even one real source file passes — the gate
        // checks RESULT (did code land), not the route the director took.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("app.tsx"),
            "export const App = () => null;\n",
        )
        .unwrap();
        assert!(
            director_source_hardgate(tmp.path(), "Created app.tsx with the login form").is_none(),
            "real source on disk satisfies the hard-gate"
        );
    }

    #[test]
    fn director_hardgate_ignores_a_pure_answer() {
        // A director that just ANSWERED (no change-verb claim) is not failing by
        // producing no new source — the gate only judges a claimed build. The
        // phrase carries no change verb (EN or ZH), so `claims_code_changes` is
        // false and the gate stays silent.
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(
            !claims_code_changes("这段代码看起来没有问题,逻辑正确"),
            "sanity: the answer carries no change verb"
        );
        assert!(
            director_source_hardgate(tmp.path(), "这段代码看起来没有问题,逻辑正确").is_none(),
            "a no-build answer never trips the hard-gate"
        );
    }

    #[tokio::test]
    async fn director_build_stream_fires_hardgate_on_phantom_build() {
        // End to end through the agentic stream in DIRECTOR-BUILD mode: the base
        // claims a build but writes nothing -> the objective source-present
        // hard-gate emits the ABORT_SENTINEL note, on TOP of the git phantom-change
        // warning. (A non-director turn would only get the lighter git fact line.)
        let tmp = init_git_repo();
        let path = tmp.path().to_path_buf();
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let spy = EffectSpy {
            // "implemented" / "created" are recognised change verbs, so this reply
            // CLAIMS a build — which the hard-gate must then check against reality.
            reply: "I implemented the entire dashboard and created the API routes".to_string(),
            warn: false,
            effect: Box::new(|| ()), // writes NOTHING
        };
        drive_agentic_stream(
            &spy,
            "build me a dashboard",
            "m",
            "claude-code",
            &path,
            true, // director_build
            &test_route(umadev_agent::RouteClass::Build),
            &[],
            &sink,
            &route_tx,
            None,
        )
        .await;

        let mut saw_sentinel = false;
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.starts_with(ABORT_SENTINEL) {
                    saw_sentinel = true;
                }
            }
        }
        assert!(
            saw_sentinel,
            "a director-build that claimed code but wrote zero source must abort honestly"
        );
        // The turn still terminates cleanly (the gate is an honest note, not a
        // panic), and carries `director_build: true` back so the event loop drives
        // the Wave-5 session hand-back.
        assert!(matches!(
            route_rx.try_recv(),
            Ok(RouteDecision::AgenticDone {
                director_build: true,
                ..
            })
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

    /// Build a chat-mode App rooted at a fresh temp dir for the build-complete
    /// wiring tests.
    #[cfg(test)]
    fn build_test_app() -> (crate::app::App, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = crate::config::UserConfig {
            backend: Some("claude-code".to_string()),
            lang: Some("zh-CN".to_string()),
            ..Default::default()
        };
        let app = crate::app::App::new(
            "demo",
            cfg,
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        (app, tmp)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn start_preview_server_registers_child_and_take_kills_it() {
        // The dev-server child must be parked in `preview_server` so the run-exit
        // cleanup can kill it — no leaked process. Spawn a real long-lived process
        // on a free ephemeral port, confirm it's registered, then take + kill it
        // (exactly what `run()`'s exit cleanup does).
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let preview: std::sync::Arc<std::sync::Mutex<Option<tokio::process::Child>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        // Retry across ephemeral ports for determinism under parallel tests: a free
        // port (`port_is_free` → we spawn) is found by bind(:0)+drop, but a CONCURRENT
        // test can grab that just-freed port in the window before start_preview_server
        // re-checks it — which would skip the spawn. Losing the race 8× is negligible.
        // `cd / && sleep 30` → parse_run_command resolves `sleep` directly (a real
        // long-lived child) in `/`, so the test never depends on `sh` resolution.
        let mut registered = false;
        for _ in 0..8 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let url = format!("http://127.0.0.1:{port}");
            start_preview_server(
                &preview,
                &sink,
                &url,
                "cd / && sleep 30",
                std::path::Path::new("/"),
                false,
            );
            if preview.lock().unwrap().is_some() {
                registered = true;
                break;
            }
        }
        // A child was registered (the build flow never blocks; this is sync).
        assert!(
            registered,
            "dev-server child must be parked for exit cleanup"
        );
        // Exit cleanup: take + kill — must not leak.
        let killed = preview
            .lock()
            .unwrap()
            .take()
            .is_some_and(|mut c| c.start_kill().is_ok());
        assert!(killed, "the parked child must be killable on exit");
        assert!(
            preview.lock().unwrap().is_none(),
            "the slot is cleared after take()"
        );
    }

    #[test]
    fn phantom_build_with_zero_source_gets_no_completion_card() {
        // Honesty guard: a build that produced NO real source (the director
        // claimed a build the workspace doesn't show) must NOT get a celebratory
        // "✅ done" card — the source hard-gate already flagged it as not done.
        let (mut app, _tmp) = build_test_app();
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let before = app.history.len();
        // Empty workspace → `acceptance::source_files` is empty → guard fires.
        finalize_build_completion(&mut app, &sink);
        assert_eq!(
            app.history.len(),
            before,
            "no completion card for a zero-source phantom build"
        );
        assert!(
            app.preview_server.lock().unwrap().is_none(),
            "no server started"
        );
    }

    #[test]
    fn real_build_with_source_gets_a_completion_card() {
        // The positive case: a build that produced real source DOES get the card.
        let (mut app, _tmp) = build_test_app();
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        std::fs::create_dir_all(app.project_root.join("src")).unwrap();
        std::fs::write(app.project_root.join("src").join("main.rs"), "fn main(){}").unwrap();
        let before = app.history.len();
        finalize_build_completion(&mut app, &sink);
        assert_eq!(app.history.len(), before + 1, "exactly one completion card");
        // Non-web (pure rust) → no dev server started.
        assert!(
            app.preview_server.lock().unwrap().is_none(),
            "no server for a non-web build"
        );
    }

    #[test]
    fn non_web_build_completion_card_pushes_card_without_a_server() {
        // A non-web effective build: the card is pushed (✅ done + what changed)
        // but NO dev server target resolves → no preview line, and the caller
        // starts nothing. Fail-open + non-blocking.
        let (mut app, _tmp) = build_test_app();
        std::fs::create_dir_all(app.project_root.join("src")).unwrap();
        std::fs::write(app.project_root.join("src").join("main.rs"), "fn main(){}").unwrap();
        let before = app.history.len();
        // `post_build_completion_card` is what `finalize_build_completion` drives.
        let target = app.post_build_completion_card();
        assert!(target.is_none(), "non-web build resolves no preview target");
        assert_eq!(app.history.len(), before + 1, "exactly one card is pushed");
        assert!(
            app.preview_server.lock().unwrap().is_none(),
            "no server started"
        );
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

    /// MEDIUM #7: a director build STARTED from the chat TUI must write the same
    /// `WorkflowState` baseline the CLI's `AgentRunner::start` does, so `umadev
    /// status` / `umadev continue` can see + resume a build kicked off in the TUI.
    /// The baseline is written BEFORE the base session opens, so even a turn whose
    /// session can't start (an unknown backend → deterministic `session_for` error,
    /// hermetic on any machine) still leaves the baseline on disk.
    #[tokio::test]
    async fn tui_director_build_writes_workflow_state_baseline() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let options = RunOptions {
            project_root: tmp.path().to_path_buf(),
            requirement: "build a kanban board".into(),
            slug: "kanban".into(),
            model: String::new(),
            backend: "nonexistent-backend".into(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: umadev_agent::TrustMode::Guarded,
            strict_coverage: false,
        };

        // Drive the director loop body directly (no spawn): the session start fails
        // open AFTER the baseline write, so the loop returns cleanly.
        run_director_loop(options, sink, route_tx, false, Vec::new(), None, false).await;

        // The baseline is on disk and carries the run's identity — exactly what the
        // CLI surfaces read.
        let state =
            umadev_agent::read_workflow_state(tmp.path()).expect("TUI build wrote a baseline");
        assert_eq!(state.slug, "kanban");
        assert_eq!(state.requirement, "build a kanban board");
        assert_eq!(state.backend, "nonexistent-backend");
        // It is a fresh run baseline (phase research, no open gate).
        assert_eq!(state.phase, umadev_spec::Phase::Research.id());
        assert!(state.active_gate.is_empty());
    }

    /// Drive a light agentic turn (`run_agentic`) against the OFFLINE brain in `root`
    /// with a `Build`-class verdict, toggling `host_cli`, and report whether a
    /// `trust.branch_isolated` note was emitted — the observable proxy for "did this
    /// turn take the run-lock + isolate the branch".
    async fn build_turn_isolated(root: &std::path::Path, host_cli: bool) -> bool {
        let (sink, mut rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        run_agentic(
            AgenticTurn {
                task: "build me a dashboard".into(),
                spec: BrainSpec::Offline,
                continue_session: false,
                session_id: None,
                fallback_model: "offline".into(),
                project_root: root.to_path_buf(),
                director_build: true,
                host_cli,
                route: Some(test_route(umadev_agent::RouteClass::Build)),
                conversation: Vec::new(),
            },
            sink,
            route_tx,
        )
        .await;
        // The isolation note (any locale) embeds the derived `umadev/<slug>` branch
        // name — a stable, locale-independent observable for "this turn isolated".
        let mut isolated = false;
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains("umadev/") {
                    isolated = true;
                }
            }
        }
        isolated
    }

    /// LOW fix (tui-dispatch): a `Build`-class verdict against a NON-host brain
    /// stays on the light streaming path and must NOT take the run-lock or isolate a
    /// branch — only a real HOST director build (which actually mutates the
    /// workspace under the lock) does. We assert the gate by observing the
    /// `trust.branch_isolated` note: present for a HOST build, absent for a non-host
    /// one, against the SAME committed git repo.
    #[tokio::test]
    async fn non_host_build_does_not_lock_or_isolate_on_the_light_path() {
        // A committed git repo on a normal branch — the only setup that would let
        // `setup_run_isolation` create+switch to an isolation branch.
        let tmp = init_git_repo();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap();
        };
        std::fs::write(tmp.path().join("seed.txt"), "seed").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "seed"]);

        // (1) Non-host build → NO isolation note (no lock, no branch isolation).
        let isolated_non_host = build_turn_isolated(tmp.path(), false).await;
        assert!(
            !isolated_non_host,
            "a non-host would-be build must NOT isolate / lock on the light path"
        );

        // (2) The SAME setup but driven as a HOST build DOES isolate — proving the
        // observable is real and the gate, not the environment, is what differs.
        // (Re-clean the tree: the non-host turn wrote nothing, so the repo is still
        // clean on the default branch.)
        let isolated_host = build_turn_isolated(tmp.path(), true).await;
        assert!(
            isolated_host,
            "a HOST director build isolates onto umadev/<slug> as before"
        );
    }

    // ── Reactive-build (the ~30s-latency fix) ───────────────────────────────
    //
    // The chat surface drives the persistent session ONCE on the light path with
    // NO up-front classification subprocess; the base decides chat-vs-build by
    // ACTING, and `react_to_first_write` promotes the turn the instant the first
    // `Write`/`Edit`-family tool call appears. These tests lock that behaviour.

    /// A streaming spy that emits a caller-chosen tool call (so the reactive write
    /// detector can be exercised) and OPTIONALLY runs a side effect AFTER emitting
    /// it (e.g. writes a file — mirroring how a real base writes a file when its
    /// `Write` tool executes, just AFTER announcing the `tool_use`). Not offline,
    /// so it drives the host-CLI code paths. A fixed reply closes the turn.
    struct WriteSpy {
        tool_name: &'static str,
        reply: &'static str,
        /// Run after the tool event is emitted (the file write the tool performs).
        effect: Box<dyn Fn() + Send + Sync>,
    }

    #[async_trait::async_trait]
    impl Runtime for WriteSpy {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            unreachable!("the chat light path must stream, never one-shot complete")
        }
        async fn complete_streaming(
            &self,
            _req: CompletionRequest,
            on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
        ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
            // Announce the tool call FIRST (clean tree → `setup_run_isolation` can
            // switch onto a fresh branch), THEN perform the write so the change is
            // carried onto the isolation branch — the real `switch -c` semantics.
            on_event(umadev_runtime::StreamEvent::ToolUse {
                name: self.tool_name.to_string(),
                detail: "src/App.tsx".to_string(),
                edit: None,
            });
            (self.effect)();
            on_event(umadev_runtime::StreamEvent::Text {
                delta: self.reply.to_string(),
            });
            Ok(umadev_runtime::CompletionResponse {
                text: self.reply.to_string(),
                id: "spy".to_string(),
                model: "spy".to_string(),
                usage: umadev_runtime::Usage::default(),
            })
        }
    }

    /// Read the current git branch of `root` (empty on failure).
    fn git_branch(root: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Reactive build, the load-bearing case: a HOST chat turn whose base writes its
    /// first real file is promoted to a build — it isolates onto `umadev/<slug>`
    /// (carrying the just-written file) and the user's branch is left untouched,
    /// AND the terminal decision carries `director_build: true` (so the Wave-5
    /// hand-back + source hard-gate fire). The intent card + the build note surface.
    #[tokio::test]
    async fn reactive_first_write_isolates_and_keeps_branch_clean() {
        // A committed repo on its default branch — the only state in which
        // `setup_run_isolation` will create + switch to an isolation branch.
        let tmp = init_git_repo();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap();
        };
        std::fs::write(tmp.path().join("seed.txt"), "seed").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "seed"]);
        let start_branch = git_branch(tmp.path());

        let (sink, mut rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let target = tmp.path().join("src");
        let reactive = Arc::new(ReactiveBuild::new(true));
        let spy = WriteSpy {
            tool_name: "Write",
            reply: "Created src/App.tsx",
            effect: Box::new(move || {
                std::fs::create_dir_all(&target).unwrap();
                std::fs::write(target.join("App.tsx"), "export const A = 1;").unwrap();
            }),
        };
        drive_agentic_stream(
            &spy,
            "做一个登录页",
            "m",
            "claude-code",
            tmp.path(),
            false, // dispatched as CHAT (not a pre-classified build)
            &light_default_route(),
            &[],
            &sink,
            &route_tx,
            Some(&reactive),
        )
        .await;

        // The turn became a build: the terminal decision carries it (→ hand-back).
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone { director_build, .. }) => assert!(
                director_build,
                "a chat turn that wrote a file is reactively a build"
            ),
            other => panic!("expected AgenticDone, got {other:?}"),
        }
        // It isolated onto a fresh `umadev/<slug>` branch (carrying the write) and
        // surfaced both the build note and the trust/isolation note.
        let now_branch = git_branch(tmp.path());
        assert_ne!(
            now_branch, start_branch,
            "the turn switched off the user branch"
        );
        assert!(
            now_branch.starts_with("umadev/"),
            "isolated onto umadev/<slug>, got `{now_branch}`"
        );
        // The user's original branch has NO new commit — UmaDev never auto-commits
        // / merges; the work sits uncommitted on the isolation branch.
        let mut saw_isolated = false;
        let mut saw_build_note = false;
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains("umadev/") {
                    saw_isolated = true;
                }
                if n.contains("[build]") {
                    saw_build_note = true;
                }
            }
        }
        assert!(saw_isolated, "the trust/isolation note was surfaced");
        assert!(saw_build_note, "the reactive build note was surfaced");
    }

    /// A pure chat reply (the base only emits text, never a write) stays a fast,
    /// light chat: NO run-lock, NO branch isolation, and the terminal decision
    /// carries `director_build: false` (no Wave-5 hand-back, no source hard-gate).
    #[tokio::test]
    async fn pure_chat_reply_does_not_isolate_or_lock() {
        let tmp = init_git_repo();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap();
        };
        std::fs::write(tmp.path().join("seed.txt"), "seed").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "seed"]);
        let start_branch = git_branch(tmp.path());

        let (sink, mut rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        let reactive = Arc::new(ReactiveBuild::new(true));
        // A spy that only READS + replies (no write tool, no effect).
        let spy = WriteSpy {
            tool_name: "Read",
            reply: "Here's how that works…",
            effect: Box::new(|| ()),
        };
        drive_agentic_stream(
            &spy,
            "解释一下这段代码",
            "m",
            "claude-code",
            tmp.path(),
            false,
            &light_default_route(),
            &[],
            &sink,
            &route_tx,
            Some(&reactive),
        )
        .await;

        // Still a chat: the terminal decision did NOT promote it to a build.
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone { director_build, .. }) => {
                assert!(!director_build, "a pure-reply turn stays a chat");
            }
            other => panic!("expected AgenticDone, got {other:?}"),
        }
        // No isolation: still on the user's branch, no run-lock left on disk, and
        // no `umadev/` isolation note emitted.
        assert_eq!(
            git_branch(tmp.path()),
            start_branch,
            "stayed on the user branch"
        );
        assert!(
            !tmp.path().join(".umadev/run.lock").exists(),
            "a pure chat turn takes no run-lock"
        );
        let mut saw_isolated = false;
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains("umadev/") {
                    saw_isolated = true;
                }
            }
        }
        assert!(!saw_isolated, "a pure chat turn never isolates");
    }

    /// The hot path is a SINGLE base call: the chat dispatcher no longer runs a
    /// separate `route_via_brain` triage `complete()` before answering (the two
    /// cold starts that caused the ~30s first reply). Driving the chat light path
    /// must hit `complete_streaming` exactly once and `complete` (the one-shot
    /// triage surface) ZERO times.
    #[tokio::test]
    async fn chat_first_reply_is_one_streaming_call_no_triage() {
        let (sink, _rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let complete_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let streaming_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spy = StreamSpy {
            complete_calls: Arc::clone(&complete_calls),
            streaming_calls: Arc::clone(&streaming_calls),
            fail: false,
        };
        let reactive = Arc::new(ReactiveBuild::new(true));
        let tmp = tempfile::TempDir::new().unwrap();
        drive_agentic_stream(
            &spy,
            "你好，能帮我做什么？",
            "m",
            "claude-code",
            tmp.path(),
            false,
            &light_default_route(),
            &[],
            &sink,
            &route_tx,
            Some(&reactive),
        )
        .await;
        assert_eq!(
            complete_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "NO separate triage `complete()` on the chat hot path"
        );
        assert_eq!(
            streaming_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly ONE base call drives the first reply"
        );
    }

    /// `is_workspace_write_tool` recognises the write family across the three bases'
    /// normalised tool names, and treats read/inspect/run tools as non-writes (so a
    /// pure read/answer turn never trips the reactive build).
    #[test]
    fn write_tool_detection_covers_the_write_family_only() {
        for w in [
            "Write",
            "Edit",
            "MultiEdit",
            "write",
            "edit",
            "apply_patch",
            "create",
        ] {
            assert!(is_workspace_write_tool(w), "`{w}` is a workspace write");
        }
        for r in ["Read", "Grep", "Glob", "Bash", "WebFetch", "Task", ""] {
            assert!(
                !is_workspace_write_tool(r),
                "`{r}` is NOT a workspace write"
            );
        }
    }

    /// Reactive build is OPT-IN per turn: with `reactive: None` (the explicit `/run`
    /// path + the queued-drain + the test default), a write tool does NOT isolate —
    /// the behaviour is byte-for-byte the pre-change light path.
    #[tokio::test]
    async fn reactive_disabled_never_isolates_on_write() {
        let tmp = init_git_repo();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap();
        };
        std::fs::write(tmp.path().join("seed.txt"), "seed").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "seed"]);
        let start_branch = git_branch(tmp.path());

        let (sink, mut rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let target = tmp.path().join("written.txt");
        let spy = WriteSpy {
            tool_name: "Write",
            reply: "done",
            effect: Box::new(move || std::fs::write(&target, "x").unwrap()),
        };
        drive_agentic_stream(
            &spy,
            "x",
            "m",
            "claude-code",
            tmp.path(),
            false,
            &light_default_route(),
            &[],
            &sink,
            &route_tx,
            None, // reactive build disabled
        )
        .await;
        assert_eq!(
            git_branch(tmp.path()),
            start_branch,
            "with reactive disabled a write never isolates"
        );
        let mut saw_isolated = false;
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains("umadev/") {
                    saw_isolated = true;
                }
            }
        }
        assert!(
            !saw_isolated,
            "no isolation note when reactive build is off"
        );
    }

    // ── Persistent chat-session path (the latency fix) ────────────────────────

    /// A scripted fake [`umadev_runtime::BaseSession`] for the resident chat path.
    /// Pre-loaded into the holder so [`drive_chat_session_turn`] REUSES it (never
    /// calls `session_for`), and records every directive + how often it was opened
    /// so a test can assert "one base, reused" + "firmware/transcript once".
    struct FakeChatSession {
        /// One event-batch per upcoming turn, consumed front-to-back.
        turns: std::collections::VecDeque<Vec<umadev_runtime::SessionEvent>>,
        /// The currently-draining batch.
        current: std::collections::VecDeque<umadev_runtime::SessionEvent>,
        /// Every directive this session received, in order (asserted by tests).
        sent: Arc<std::sync::Mutex<Vec<String>>>,
        /// Bumped on `interrupt()` / `end()` so a test can assert lifecycle.
        ended: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeChatSession {
        fn new(
            turns: Vec<Vec<umadev_runtime::SessionEvent>>,
        ) -> (
            Self,
            Arc<std::sync::Mutex<Vec<String>>>,
            Arc<std::sync::atomic::AtomicBool>,
        ) {
            let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
            let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                Self {
                    turns: turns.into_iter().collect(),
                    current: std::collections::VecDeque::new(),
                    sent: Arc::clone(&sent),
                    ended: Arc::clone(&ended),
                },
                sent,
                ended,
            )
        }
    }

    #[async_trait::async_trait]
    impl umadev_runtime::BaseSession for FakeChatSession {
        async fn send_turn(
            &mut self,
            directive: String,
        ) -> Result<(), umadev_runtime::SessionError> {
            self.sent.lock().unwrap().push(directive);
            self.current = self
                .turns
                .pop_front()
                .unwrap_or_default()
                .into_iter()
                .collect();
            Ok(())
        }
        async fn next_event(&mut self) -> Option<umadev_runtime::SessionEvent> {
            self.current.pop_front()
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: umadev_runtime::ApprovalDecision,
        ) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), umadev_runtime::SessionError> {
            self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn end(&mut self) -> Result<(), umadev_runtime::SessionError> {
            self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    fn chat_turn(
        text: &str,
        chat_session: ChatSessionHolder,
        sink: Arc<ChannelSink>,
        route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
        project_root: std::path::PathBuf,
    ) -> ChatSessionTurn {
        ChatSessionTurn {
            text: text.to_string(),
            backend: "claude-code".to_string(),
            model: "m".to_string(),
            project_root,
            conversation: Vec::new(),
            mode: umadev_agent::TrustMode::Guarded,
            autonomous: false,
            chat_session,
            sink,
            route_tx,
        }
    }

    /// The core latency-fix invariant: two chat turns REUSE the one held session
    /// (never re-open / cold-start), and the session is PARKED back after each turn
    /// for the next message. A reused session gets the BARE user directive (no
    /// per-turn firmware/transcript re-injection — that is a one-time open cost).
    #[tokio::test]
    async fn chat_reuses_one_resident_session_across_turns() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

        // Two scripted turns: each a plain text reply then a clean TurnDone.
        let (fake, sent, ended) = FakeChatSession::new(vec![
            vec![
                umadev_runtime::SessionEvent::TextDelta("hi there".into()),
                umadev_runtime::SessionEvent::TurnDone {
                    status: umadev_runtime::TurnStatus::Completed,
                    usage: None,
                },
            ],
            vec![
                umadev_runtime::SessionEvent::TextDelta("still here".into()),
                umadev_runtime::SessionEvent::TurnDone {
                    status: umadev_runtime::TurnStatus::Completed,
                    usage: None,
                },
            ],
        ]);
        // Pre-load the holder with a PRIMED session → `drive_chat_session_turn` takes
        // it on the bare-reuse path, so `session_for` is NEVER called (no cold start)
        // and the directive is the bare user turn (no firmware/transcript prefix).
        let holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(Some(
            ResidentChat::Primed(Box::new(fake)),
        )));

        // Turn 1.
        drive_chat_session_turn(chat_turn(
            "你好",
            holder.clone(),
            sink.clone(),
            route_tx.clone(),
            tmp.path().to_path_buf(),
        ))
        .await;
        // The live session was parked back for reuse.
        assert!(
            holder.lock().await.is_some(),
            "session must be parked back after a clean turn"
        );
        // First turn settles as a pure chat (not a build).
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone {
                reply,
                director_build,
            }) => {
                assert_eq!(reply, "hi there");
                assert!(!director_build, "a pure reply is a chat, never a build");
            }
            other => panic!("expected AgenticDone, got {other:?}"),
        }

        // Turn 2 — the SAME held session is reused (no re-open).
        drive_chat_session_turn(chat_turn(
            "再说一句",
            holder.clone(),
            sink.clone(),
            route_tx.clone(),
            tmp.path().to_path_buf(),
        ))
        .await;
        assert!(holder.lock().await.is_some(), "session parked again");

        // The ONE session saw BOTH user turns, bare (no firmware/transcript prefix
        // re-injected on the reuse path — the session is already primed).
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2, "both turns went to the SAME session");
        assert_eq!(sent[0], "你好");
        assert_eq!(sent[1], "再说一句");
        // It was never ended/interrupted (it lives on across the conversation).
        assert!(
            !ended.load(std::sync::atomic::Ordering::SeqCst),
            "a resident chat session is not closed between turns"
        );

        // No chat intent card was ever emitted (the user removed it) — only worker
        // stream text, no `IntentDecided`.
        let mut saw_intent = false;
        while let Ok(ev) = engine_rx.try_recv() {
            if matches!(ev, EngineEvent::IntentDecided { .. }) {
                saw_intent = true;
            }
        }
        assert!(
            !saw_intent,
            "a pure chat turn emits NO intent card (chat card removed)"
        );
    }

    /// Reactive build on the resident path: the FIRST `Write` tool call flips the
    /// turn into a build — a `Build` intent card is surfaced and the terminal
    /// decision carries `director_build: true` (driving the source hard-gate +
    /// Wave-5 hand-back), exactly as the light path did.
    #[tokio::test]
    async fn chat_session_reacts_to_first_write_as_build() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

        let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
            umadev_runtime::SessionEvent::ToolCall {
                name: "Write".into(),
                input: serde_json::json!({ "file_path": "src/main.rs" }),
            },
            umadev_runtime::SessionEvent::TextDelta("created the file".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: None,
            },
        ]]);
        let holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(Some(
            ResidentChat::Primed(Box::new(fake)),
        )));

        drive_chat_session_turn(chat_turn(
            "建个 main",
            holder,
            sink.clone(),
            route_tx,
            tmp.path().to_path_buf(),
        ))
        .await;

        // The terminal decision is a build (the base wrote a file).
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone { director_build, .. }) => {
                assert!(
                    director_build,
                    "a write reactively promotes the turn to a build"
                );
            }
            other => panic!("expected AgenticDone, got {other:?}"),
        }
        // A `Build` intent card was surfaced (the behaviour-derived "构建中" signal)
        // and the write streamed live as a WorkerStream tool row.
        let mut saw_build_card = false;
        let mut saw_write = false;
        while let Ok(ev) = engine_rx.try_recv() {
            match ev {
                EngineEvent::IntentDecided { class, .. } if class == "build" => {
                    saw_build_card = true;
                }
                EngineEvent::WorkerStream {
                    event: umadev_runtime::StreamEvent::ToolUse { name, .. },
                } if name == "Write" => saw_write = true,
                _ => {}
            }
        }
        assert!(
            saw_build_card,
            "the first write surfaces a Build intent card"
        );
        assert!(saw_write, "the write tool call streams live");
    }

    /// ARCHITECTURE UNIFICATION: a chat-build (`became_build`) runs the SAME flagship
    /// post-build QC the `/run` path does — the `team · 构建完成 …` note proves the
    /// governance/slop scan + team review pass fired. A clean lean build settles after
    /// the scan (source present + no slop), so no needless fix turn slows the chat.
    #[tokio::test]
    async fn chat_build_runs_the_post_build_qc_pass() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Pre-seed a real, slop-free source file so the source-present honesty floor
        // PASSES and the governance scan is clean — the QC pass runs and settles clean.
        std::fs::write(tmp.path().join("app.ts"), "export const x = 1;").unwrap();
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

        // The base writes a file (flips to a build) then reports done.
        let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
            umadev_runtime::SessionEvent::ToolCall {
                name: "Write".into(),
                input: serde_json::json!({ "file_path": "app.ts" }),
            },
            umadev_runtime::SessionEvent::TextDelta("built the page".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: None,
            },
        ]]);
        let holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(Some(
            ResidentChat::Primed(Box::new(fake)),
        )));

        drive_chat_session_turn(chat_turn(
            "做个落地页",
            holder.clone(),
            sink.clone(),
            route_tx,
            tmp.path().to_path_buf(),
        ))
        .await;

        // The terminal decision is a build.
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone { director_build, .. }) => {
                assert!(
                    director_build,
                    "a write reactively promotes the turn to a build"
                );
            }
            other => panic!("expected AgenticDone, got {other:?}"),
        }
        // The post-build QC pass fired its entry note — the same flagship pass `/run`
        // runs (governance/slop scan + team review). This is the unification's proof.
        let mut saw_qc = false;
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                if n.contains("构建完成") || n.contains("honesty + QC") || n.contains("team ·")
                {
                    saw_qc = true;
                }
            }
        }
        assert!(
            saw_qc,
            "a chat-build runs the post-build QC pass (the team · … notes fired)"
        );
        // The live session is parked back for reuse after the QC pass.
        assert!(
            holder.lock().await.is_some(),
            "the session is parked back after the post-build QC pass"
        );
    }

    /// The other half of the unification invariant: a PURE chat reply (no write, no
    /// `became_build`) must NOT run the post-build QC pass — it stays light + fast,
    /// with no `team · …` QC notes and no extra fix directives. This guards the
    /// latency: conversation is never slowed by the build-only QC machinery.
    #[tokio::test]
    async fn pure_chat_reply_skips_the_post_build_qc_pass() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, mut engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

        // A pure text reply — no write tool, so `became_build` stays false.
        let (fake, sent, _ended) = FakeChatSession::new(vec![vec![
            umadev_runtime::SessionEvent::TextDelta("here is my answer".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: None,
            },
        ]]);
        let holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(Some(
            ResidentChat::Primed(Box::new(fake)),
        )));

        drive_chat_session_turn(chat_turn(
            "你好,解释一下闭包",
            holder.clone(),
            sink.clone(),
            route_tx,
            tmp.path().to_path_buf(),
        ))
        .await;

        // Settles as a pure chat (not a build).
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone {
                director_build,
                reply,
            }) => {
                assert!(!director_build, "a pure reply is a chat, never a build");
                assert_eq!(reply, "here is my answer");
            }
            other => panic!("expected AgenticDone, got {other:?}"),
        }
        // NO post-build QC note fired — the conversation stayed on the light path.
        while let Ok(ev) = engine_rx.try_recv() {
            if let EngineEvent::Note(n) = ev {
                assert!(
                    !(n.contains("构建完成") || n.contains("honesty + QC")),
                    "a pure chat reply must NOT run the post-build QC pass: {n:?}"
                );
            }
        }
        // EXACTLY one directive was sent (the user turn) — no QC fix directive was
        // ever folded back, so a pure chat is never slowed by rework.
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "a pure chat reply drives exactly one directive — no QC rework"
        );
    }

    /// An interrupted turn (ESC reflected by the base as `TurnStatus::Interrupted`)
    /// PARKS the still-alive session back for reuse and settles `thinking` via a
    /// (non-build) terminal decision — it does NOT close the resident session.
    #[tokio::test]
    async fn chat_session_interrupt_parks_session_for_reuse() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, _engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

        let (fake, _sent, ended) = FakeChatSession::new(vec![vec![
            umadev_runtime::SessionEvent::TextDelta("partial".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Interrupted,
                usage: None,
            },
        ]]);
        let holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(Some(
            ResidentChat::Primed(Box::new(fake)),
        )));

        drive_chat_session_turn(chat_turn(
            "停",
            holder.clone(),
            sink,
            route_tx,
            tmp.path().to_path_buf(),
        ))
        .await;

        assert!(
            holder.lock().await.is_some(),
            "an interrupted turn parks the live session back for reuse"
        );
        assert!(
            !ended.load(std::sync::atomic::Ordering::SeqCst),
            "interrupt does NOT close the resident session"
        );
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone { director_build, .. }) => {
                assert!(!director_build, "an interrupted turn settles as a chat");
            }
            other => panic!("expected AgenticDone, got {other:?}"),
        }
    }

    /// A PRE-LOADED warm session (the latency fix): the holder already carries a
    /// `Warm` session by the time the user sends, so the FIRST turn does NOT
    /// cold-start (`session_for` is never called) — it only sends the first directive
    /// into the already-resident base and parks it back PRIMED for reuse. The first
    /// directive front-loads the bounded conversation transcript so the warm session
    /// inherits the prior dialogue.
    #[tokio::test]
    async fn preloaded_warm_session_is_used_without_a_cold_start() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, _engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

        let (fake, sent, ended) = FakeChatSession::new(vec![vec![
            umadev_runtime::SessionEvent::TextDelta("warm reply".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: None,
            },
        ]]);
        // Park a WARM session (claude → no firmware prefix on the first directive)
        // exactly as the background pre-load would have.
        let holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(Some(
            ResidentChat::Warm(WarmChatSession {
                session: Box::new(fake),
                firmware: None,
            }),
        )));

        // Drive a turn whose snapshot carries a one-line prior conversation so we can
        // assert the FIRST directive front-loads it (the warm session is fresh memory).
        let mut turn = chat_turn(
            "继续",
            holder.clone(),
            sink,
            route_tx,
            tmp.path().to_path_buf(),
        );
        turn.conversation = vec![
            umadev_runtime::Message {
                role: "user".into(),
                content: "之前的问题".into(),
            },
            umadev_runtime::Message {
                role: "assistant".into(),
                content: "之前的回答".into(),
            },
        ];
        drive_chat_session_turn(turn).await;

        // The warm session was consumed and re-parked as `Primed` (alive, reusable).
        assert!(
            matches!(*holder.lock().await, Some(ResidentChat::Primed(_))),
            "a warm session becomes primed after its first turn"
        );
        assert!(
            !ended.load(std::sync::atomic::Ordering::SeqCst),
            "the warm session is reused, never closed, after the first turn"
        );
        // The FIRST directive front-loaded the prior dialogue (warm session has no
        // native memory of it yet) — so it is NOT the bare user turn.
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "exactly one directive into the warm session");
        assert!(
            sent[0].contains("继续") && sent[0].contains("之前的回答"),
            "first directive front-loads the transcript onto the warm session: {:?}",
            sent[0]
        );
        match route_rx.try_recv() {
            Ok(RouteDecision::AgenticDone {
                reply,
                director_build,
            }) => {
                assert_eq!(reply, "warm reply");
                assert!(!director_build);
            }
            other => panic!("expected AgenticDone, got {other:?}"),
        }
    }

    /// The first directive for a warm session: claude gets history ONLY (firmware is
    /// native via `--append-system-prompt`); a non-claude base (no native system
    /// slot) gets the firmware re-prefixed onto the directive too.
    #[test]
    fn first_chat_directive_prefixes_firmware_only_for_non_claude() {
        let convo: Vec<Message> = Vec::new();
        // claude: firmware present but NEVER restated on the directive.
        let claude = first_chat_directive(Some("FW-BLOCK"), "claude-code", &convo, "做个登录页");
        assert!(
            !claude.contains("FW-BLOCK"),
            "claude firmware is native — never re-prefixed: {claude:?}"
        );
        assert!(claude.contains("做个登录页"));
        // codex: no native system slot → firmware is prefixed onto the directive.
        let codex = first_chat_directive(Some("FW-BLOCK"), "codex", &convo, "做个登录页");
        assert!(
            codex.starts_with("FW-BLOCK"),
            "non-claude firmware is front-loaded onto the first directive: {codex:?}"
        );
        assert!(codex.contains("做个登录页"));
        // No firmware → bare goal regardless of base.
        let bare = first_chat_directive(None, "opencode", &convo, "做个登录页");
        assert_eq!(bare, "做个登录页");
    }

    /// The background pre-load is a NO-OP for a non-host (offline) brain — there is no
    /// resident process to keep, so the holder stays empty and the first chat turn
    /// lazily opens exactly as before. (Hermetic: an offline id never spawns a base.)
    #[tokio::test]
    async fn preload_is_a_noop_for_a_non_host_backend() {
        let tmp = tempfile::TempDir::new().unwrap();
        let holder: ChatSessionHolder = Arc::new(tokio::sync::Mutex::new(None));
        spawn_chat_session_preload(
            Some("offline"),
            String::new(),
            tmp.path().to_path_buf(),
            false,
            holder.clone(),
        );
        // Also a `None` backend (no base configured) — both must leave the holder empty.
        spawn_chat_session_preload(
            None,
            String::new(),
            tmp.path().to_path_buf(),
            false,
            holder.clone(),
        );
        // Give any (wrongly-)spawned task a chance to run, then assert nothing landed.
        tokio::task::yield_now().await;
        assert!(
            holder.lock().await.is_none(),
            "a non-host / unconfigured pre-load never lands a session"
        );
    }

    /// `ResidentChat::end` releases the underlying base in BOTH states (warm + primed)
    /// — the cleanup the cancel / `/clear` / backend-switch / quit paths rely on.
    #[tokio::test]
    async fn resident_chat_end_closes_warm_and_primed() {
        let (warm_fake, _s, warm_ended) = FakeChatSession::new(vec![]);
        ResidentChat::Warm(WarmChatSession {
            session: Box::new(warm_fake),
            firmware: None,
        })
        .end()
        .await;
        assert!(
            warm_ended.load(std::sync::atomic::Ordering::SeqCst),
            "ending a warm resident closes its base"
        );
        let (primed_fake, _s2, primed_ended) = FakeChatSession::new(vec![]);
        ResidentChat::Primed(Box::new(primed_fake)).end().await;
        assert!(
            primed_ended.load(std::sync::atomic::Ordering::SeqCst),
            "ending a primed resident closes its base"
        );
    }
}
