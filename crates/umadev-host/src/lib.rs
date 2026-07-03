//! `umadev-host` — drive an already-logged-in host CLI as a subprocess.
//!
//! In base-CLI mode UmaDev does not call any LLM API itself and does not
//! need an API key. Instead it spawns a host CLI the user has already installed
//! and authenticated, in non-interactive mode, and captures the response.
//!
//! UmaDev drives **exactly three** host CLIs as first-class bases:
//!
//! | id            | binary    | non-interactive form                              |
//! |---------------|-----------|---------------------------------------------------|
//! | `claude-code` | `claude`  | `claude --print --output-format text "<p>"`       |
//! | `codex`       | `codex`   | `codex exec --skip-git-repo-check --sandbox …`    |
//! | `opencode`    | `opencode`| `opencode run "<p>"`                              |
//!
//! Each driver implements [`umadev_runtime::Runtime`] so the existing
//! `AgentRunner` machinery drives it unchanged — a host CLI is just
//! another "prompt in, text out" backend. Drivers additionally expose
//! [`HostDriver::probe`] to report whether the underlying CLI is installed +
//! reachable before a run starts.
//!
//! Run `umadev doctor` to see which of the supported CLIs are installed on
//! the current machine.
//!
//! UmaDev drives only these three CLIs and owns no model endpoint of its own.
//! Whatever a base is already configured with — official login OR the customer's
//! own third-party / local-model routing — is exactly what runs.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::doc_markdown
)]

pub mod claude;
/// Continuous-session driver for `claude` (stream-json), alongside the
/// single-shot `claude` module — see `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`.
pub mod claude_session;
pub mod codex;
/// Continuous-session driver for `codex` (`codex app-server` JSON-RPC over
/// stdio), alongside the single-shot `codex` module — see
/// `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`.
pub mod codex_session;
pub mod opencode;
/// Continuous-session driver for `opencode` (`opencode serve` HTTP + SSE),
/// alongside the single-shot `opencode` module — see
/// `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`.
pub mod opencode_session;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub use claude::ClaudeCodeDriver;
pub use claude_session::ClaudeSession;
pub use codex::CodexDriver;
pub use codex_session::CodexSession;
pub use opencode::OpenCodeDriver;
pub use opencode_session::OpenCodeSession;

pub mod process_logs;
pub mod stderr_tail;

/// The env var UmaDev sets on a base subprocess to mark "UmaDev is driving this
/// run/session" — its value is the project root being governed.
///
/// The base (claude) inherits this var and passes it to the `PreToolUse`
/// governance hook it spawns. The hook governs **only** when this var is set, and
/// only files under its value (see `umadev::hook`). So a base UmaDev itself
/// drives is governed, while a base the user runs directly (plain claude /
/// spec-kit / any other project) sees the var UNSET and is completely
/// unaffected. This is how UmaDev's governance stays scoped to its own runs
/// instead of leaking into the user's whole environment.
pub const GOVERN_ROOT_ENV: &str = "UMADEV_GOVERN_ROOT";

/// Build the `[(key, value)]` env entry that scopes the governance hook to
/// `workspace`. Spawn the base with this so the `PreToolUse` hook can tell it is
/// UmaDev driving and which root to govern. Returns a single-element vec so it
/// composes with any existing provider env the caller already passes.
#[must_use]
pub fn govern_root_env(workspace: &std::path::Path) -> Vec<(String, String)> {
    vec![(
        GOVERN_ROOT_ENV.to_string(),
        workspace.to_string_lossy().into_owned(),
    )]
}

/// Whether a host CLI is actually **authenticated** — the honest signal the
/// first-run picker needs (gap G10). `--version` only proves the binary is on
/// `PATH`; it says nothing about login, so a not-logged-in base used to render a
/// green "ready" and then fail mid-run. [`AuthState`] is the missing third axis.
///
/// **Fail-open by contract:** any probe that errors, times out, or can't make a
/// confident call resolves to [`AuthState::Unknown`] — NEVER a false
/// [`AuthState::LoggedIn`]. The picker shows a conservative "login may be
/// required" hint for `Unknown` rather than a green light it can't back up.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AuthState {
    /// The CLI is installed AND has usable credentials (a logged-in
    /// subscription/OAuth session, a credential file, or an auth env var).
    LoggedIn,
    /// The CLI is installed but has NO usable credentials — the user must run
    /// the base's own login command (see [`ProbeResult::login_hint`]). This is
    /// the case the picker must surface honestly instead of a false "ready".
    NotLoggedIn,
    /// The CLI binary is not on `PATH` / not installed — auth is moot until it
    /// is installed (see [`ProbeResult::install_hint`]).
    NotInstalled,
    /// The auth state could not be determined confidently (probe errored, timed
    /// out, the status subcommand is missing, or the output was unrecognised).
    /// The picker shows a conservative "login may be required" hint. We choose
    /// this over guessing `LoggedIn` so the first-run signal never lies.
    Unknown,
}

impl AuthState {
    /// `true` only when the base is confidently authenticated. Used by the
    /// picker to decide whether to show the honest green "ready & logged in".
    #[must_use]
    pub fn is_logged_in(self) -> bool {
        matches!(self, Self::LoggedIn)
    }
}

/// Outcome of probing a host CLI for availability.
///
/// Carries BOTH the install/version signal AND the honest [`AuthState`] (gap
/// G10): a base can be installed (`--version` works) yet not logged in, in which
/// case it must NOT render as a green "ready". The picker reads `auth_state`
/// (and the install/login hints) to show one of three truthful states.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ProbeResult {
    /// The CLI is installed and responded to `--version`.
    Ready {
        /// Raw version string the CLI reported.
        version: String,
        /// Whether the CLI is actually authenticated. Installed-but-not-logged-in
        /// is [`AuthState::NotLoggedIn`]; an indeterminate probe is
        /// [`AuthState::Unknown`] (never a false `LoggedIn`).
        auth_state: AuthState,
    },
    /// The CLI binary was not found on `PATH`.
    NotInstalled {
        /// The program name that was looked up.
        program: String,
    },
    /// The CLI was found but behaved unexpectedly (non-zero `--version`).
    Unhealthy {
        /// Human-readable detail.
        detail: String,
    },
}

impl ProbeResult {
    /// `true` when the host CLI is ready to drive.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// The honest auth state for this probe. `NotInstalled` maps to
    /// [`AuthState::NotInstalled`]; an `Unhealthy` (`--version` failed for some
    /// non-PATH reason) maps to [`AuthState::Unknown`] — we can't tell login
    /// state from a broken binary, and `Unknown` is the conservative answer.
    #[must_use]
    pub fn auth_state(&self) -> AuthState {
        match self {
            Self::Ready { auth_state, .. } => *auth_state,
            Self::NotInstalled { .. } => AuthState::NotInstalled,
            Self::Unhealthy { .. } => AuthState::Unknown,
        }
    }

    /// `true` only when the base is installed AND confidently logged in — the
    /// single check the picker uses to show the honest green "ready" mark
    /// (replacing the old `--version`-only `is_ready`, which lied about login).
    #[must_use]
    pub fn is_ready_and_authed(&self) -> bool {
        self.auth_state().is_logged_in()
    }
}

/// Extension trait every host driver implements on top of [`Runtime`].
///
/// [`Runtime`]: umadev_runtime::Runtime
#[async_trait]
pub trait HostDriver: umadev_runtime::Runtime {
    /// Stable identifier used as the `--backend` flag value.
    fn backend_id(&self) -> &'static str;

    /// Human-facing name.
    fn display_name(&self) -> &'static str;

    /// Check whether the underlying CLI is installed + reachable, AND whether it
    /// is actually authenticated (gap G10). The returned [`ProbeResult`] carries
    /// the honest [`AuthState`] so the first-run picker can tell "ready & logged
    /// in" apart from "installed but not logged in" instead of showing a green
    /// light that fails mid-run. Implementors run [`Self::probe_auth`] only when
    /// the binary is confirmed installed (a `--version` success), so a missing
    /// base never pays for an auth probe.
    async fn probe(&self) -> ProbeResult;

    /// The cheapest **authenticated no-op** for this base — answers "is this CLI
    /// logged in?" WITHOUT running a real generation (no tokens burned, no
    /// model held). Each driver implements the base's own official mechanism
    /// (a credential file / auth env var existence check, falling back to the
    /// base's `auth status` / `login status` subcommand under a short timeout).
    ///
    /// **Fail-open by contract:** on ANY error, timeout, or unrecognised output
    /// it returns [`AuthState::Unknown`] — NEVER a false [`AuthState::LoggedIn`].
    /// The default returns `Unknown` so a non-first-class backend stays
    /// conservative; the three host drivers override it.
    ///
    /// Called by [`Self::probe`] only after the binary is confirmed installed.
    async fn probe_auth(&self) -> AuthState {
        AuthState::Unknown
    }

    /// The command the user runs to **install** this base, for the picker to
    /// show on a [`ProbeResult::NotInstalled`] (e.g.
    /// `npm install -g @anthropic-ai/claude-code`). `None` for a backend with no
    /// canonical install line; the three host drivers override it.
    fn install_hint(&self) -> Option<&'static str> {
        None
    }

    /// The command the user runs to **log in** to this base, for the picker to
    /// show on an [`AuthState::NotLoggedIn`] (e.g. `claude` / `codex login` /
    /// `opencode auth login`). `None` for a backend with no login step; the
    /// three host drivers override it.
    fn login_hint(&self) -> Option<&'static str> {
        None
    }

    /// Ask this driver to **continue its previous session** on the next
    /// `complete` call instead of starting a fresh one.
    ///
    /// This is how UmaDev gives chat real memory without re-stuffing the
    /// transcript: each host CLI persists its own conversation (tool calls,
    /// files read, everything), and resuming it (`claude --continue`,
    /// `codex exec resume --last`, `opencode run --continue`) is strictly
    /// richer than replaying text. The default is a no-op so non-session
    /// backends ignore it; the three first-class drivers override it.
    fn set_continue_session(&mut self, _continue_session: bool) {}

    /// Pin an explicit conversation id (a UUID) for this driver's session.
    ///
    /// Drivers whose CLI lets the caller choose the session id (`claude
    /// --session-id <uuid>` / `--resume <uuid>`) override this so UmaDev
    /// resumes *its own* chat session deterministically, never colliding with
    /// the user's other conversations in the same directory. Drivers that can
    /// only "continue the most recent" session leave the default no-op and
    /// rely on [`Self::set_continue_session`] instead.
    fn set_session_id(&mut self, _session_id: Option<String>) {}

    /// Set the working directory the host CLI subprocess runs in — the
    /// pipeline's project root.
    ///
    /// CRITICAL: the base CLIs read/write files (`output/`, `src/`,
    /// `.mcp.json`) relative to their cwd, so the subprocess MUST run in the
    /// project root, not the launching process's cwd — they differ whenever
    /// `--project-root` points elsewhere. The default is a no-op (drivers fall
    /// back to the cwd); the three first-class drivers override it.
    fn set_workspace(&mut self, _workspace: std::path::PathBuf) {}
}

/// Let a boxed driver be used wherever a [`Runtime`] is expected — e.g.
/// `AgentRunner::new(driver_for("claude-code").unwrap(), opts)`.
///
/// [`Runtime`]: umadev_runtime::Runtime
#[async_trait]
impl umadev_runtime::Runtime for Box<dyn HostDriver> {
    fn kind(&self) -> umadev_runtime::RuntimeKind {
        (**self).kind()
    }

    fn capabilities(&self) -> umadev_runtime::BrainCapabilities {
        (**self).capabilities()
    }

    async fn complete(
        &self,
        req: umadev_runtime::CompletionRequest,
    ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
        (**self).complete(req).await
    }

    async fn complete_streaming(
        &self,
        req: umadev_runtime::CompletionRequest,
        on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
    ) -> Result<umadev_runtime::CompletionResponse, umadev_runtime::RuntimeError> {
        (**self).complete_streaming(req, on_event).await
    }

    fn fork(&self) -> Option<Box<dyn umadev_runtime::Runtime>> {
        // Forward to the concrete driver's fork() (Runtime is a HostDriver
        // supertrait, so this dispatches to ClaudeCode/Codex/OpenCode). WITHOUT
        // this the run path — which boxes the driver as `Box<dyn HostDriver>` —
        // would get the trait-default `None` and the pipeline's parallel docs
        // fan-out would silently never trigger (it falls back to sequential).
        (**self).fork()
    }
}

/// How a host CLI consumes the prompt.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum PromptChannel {
    /// Prompt is passed as the last positional argument.
    Arg,
    /// Prompt is written to the child's stdin.
    Stdin,
}

/// Shared subprocess plumbing used by every driver.
///
/// Spawns `program` with `args`, optionally feeds `prompt` via stdin or
/// as a trailing argument, enforces `timeout`, and returns the captured
/// stdout (trimmed). Stderr is folded into the error on failure.
pub(crate) struct SubprocessCall<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub prompt: &'a str,
    pub channel: PromptChannel,
    pub workspace: &'a std::path::Path,
    pub timeout: Duration,
    /// Environment overrides for the child process (provider routing). Each
    /// `(key, value)` is set; an EMPTY value REMOVES the inherited var (used to
    /// scrub a conflicting `ANTHROPIC_API_KEY` when an auth token is set). This
    /// is how a third-party API is routed THROUGH the base CLI — the base keeps
    /// its own file/bash tools, only the model endpoint is redirected.
    pub env: &'a [(String, String)],
}

/// What a successful subprocess call produced.
///
/// Wall-clock duration is logged via `tracing` rather than carried on
/// this struct; the TUI (M3) will add a structured timing field when it
/// needs to render a per-host latency panel.
#[derive(Debug, Clone)]
pub(crate) struct SubprocessOutput {
    pub stdout: String,
}

/// Truncate `s` to at most `max_bytes`, walking back to a UTF-8 char boundary
/// so it never panics on a multibyte character (CJK / emoji) straddling the
/// cut. `String::truncate` panics on a non-boundary index — host error
/// messages are often localized, so the naive cut is a fail-open violation.
fn truncate_on_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

/// Hard cap on stderr captured from a single-shot base subprocess. A base that
/// floods stderr can't make us buffer unboundedly — the continuous-session
/// drivers use a bounded `StderrTail` ring for the same reason; this is the
/// single-shot equivalent. 256 KiB matches the stdout cap in [`run_subprocess`].
const STDERR_CAPTURE_CAP: usize = 262_144;

/// Bounded grace for draining a child's stderr AFTER it has exited (and for
/// reaping the concurrent stdin writer). The child is already gone, so this only
/// flushes an already-closing pipe — but a GRANDCHILD that inherited the stderr
/// write fd (e.g. a dev/MCP server the base spawned) can hold the pipe open
/// forever, so the post-exit stderr read MUST itself be time-bounded or
/// `complete()` / `probe()` / `consult()` would hang, defeating the per-call
/// timeout. Fail-open: on elapse we abandon stderr (the exit status + stdout are
/// authoritative) and abort the leaked reader so it can't linger.
const STDERR_FLUSH_GRACE: Duration = Duration::from_secs(2);

/// Spawn a task that drains a child's stderr into a byte buffer, bounded by
/// [`STDERR_CAPTURE_CAP`] so a flooding base can't grow it without limit. The
/// caller reaps it via [`reap_bounded`] under [`STDERR_FLUSH_GRACE`] (a leaked
/// grandchild fd can hold the pipe open past the child's own exit — H1).
fn spawn_stderr_capture(
    stderr: Option<tokio::process::ChildStderr>,
) -> tokio::task::JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut se) = stderr {
            let mut chunk = [0u8; 8192];
            // Read until EOF, a read error, or the cap — whichever comes first.
            // A bare `read_to_end` here is the bug: a grandchild holding the pipe
            // open makes it never EOF, and there is no size bound.
            while buf.len() < STDERR_CAPTURE_CAP {
                match se.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let remaining = STDERR_CAPTURE_CAP - buf.len();
                        let take = n.min(remaining);
                        buf.extend_from_slice(&chunk[..take]);
                        if take < n {
                            break;
                        }
                    }
                }
            }
        }
        buf
    })
}

/// Reap a best-effort background task (the stderr capture / the concurrent stdin
/// writer) under [`STDERR_FLUSH_GRACE`], aborting it on overrun so it can never
/// leak. The child is already being torn down when this is called, so it
/// normally returns instantly; the bound exists only so a leaked grandchild fd
/// (stderr) can't wedge the call forever.
async fn reap_bounded<T>(mut task: tokio::task::JoinHandle<T>) -> Option<T> {
    if let Ok(joined) = tokio::time::timeout(STDERR_FLUSH_GRACE, &mut task).await {
        joined.ok()
    } else {
        // Overran the grace (a leaked grandchild fd) — abort so it can't linger.
        task.abort();
        None
    }
}

/// Drain a spawned child's stdout+stderr to EOF AND wait for its exit, bounded
/// by BOTH a per-call hard ceiling AND a per-byte idle watchdog.
///
/// The reads are deliberately bounded: a child that writes some output and then
/// hangs while keeping its stdout pipe open (e.g. a grandchild inherits the
/// pipe) would otherwise block `read_to_end` forever — defeating the
/// `child.wait()` timeout and hanging UmaDev. The hard `timeout` ceiling caught
/// the *truly* silent case, but a base that emits a few bytes then hangs had to
/// wait out the FULL ceiling (600s) before being killed. The streaming path
/// already had a 300s idle watchdog for exactly this; this mirrors it for the
/// non-streaming path (used by `complete`/probe and every advisory critic/judge
/// `consult()` call) so a mid-output hang is killed after
/// `UMADEV_IDLE_TIMEOUT_SECS` of stdout silence instead of the full ceiling.
///
/// **Timeout model (two-phase, "first-byte grace"), mirroring the streaming
/// path.** The idle watchdog (`idle_timeout`, default 300s,
/// `UMADEV_IDLE_TIMEOUT_SECS`) measures byte-to-byte *silence*, so it is armed
/// only AFTER the first stdout byte. Before the first byte the sole bound is the
/// remaining time to the hard `timeout` deadline — a base whose first token is
/// slow is not wrongly killed before it has emitted anything. The hard `timeout`
/// ceiling always applies and the grace can never bypass it (a truly silent hang
/// still trips `timeout`). For a short total `timeout` (e.g. the 10s `probe`)
/// `idle_timeout = min(timeout, 300)` collapses onto the ceiling, so the idle
/// watchdog never fires before the hard timeout there — no false probe kills.
/// On timeout (idle OR hard) the child is killed to avoid orphaned processes.
/// The idle and hard-ceiling errors both contain `timed out` (so they classify
/// as a retriable `Timeout`, preserving the prior write-then-hang behaviour) but
/// are textually distinguishable (`… of stdout silence`).
async fn drain_and_wait(
    child: &mut tokio::process::Child,
    timeout: std::time::Duration,
    program: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let started = Instant::now();

    // Drain stderr on its own task: a child can flood stderr or hold it open,
    // which would stall a single-task stdout+stderr join AND confuse the
    // stdout-only idle watchdog (stderr traffic is not stdout liveness). Reading
    // it independently keeps the stdout idle measurement honest. (Same shape as
    // `run_subprocess_streaming`.) The task ends when stderr closes — which the
    // kill below guarantees on every error path, so it is never orphaned.
    let stderr_task = spawn_stderr_capture(child.stderr.take());

    // Same env + default + collapse semantics as the streaming path.
    let idle_timeout = std::cmp::min(
        timeout,
        Duration::from_secs(
            std::env::var("UMADEV_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
        ),
    );

    // Read stdout in chunks with a per-read idle watchdog + first-byte grace.
    let mut stdout_buf = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut seen_first_byte = false;
        let mut chunk = [0u8; 8192];
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(format!(
                    "`{program}` timed out after {}s",
                    timeout.as_secs()
                ));
            }
            // First-byte grace: before the first byte the only deadline is the
            // hard ceiling's remaining time; after it, the idle watchdog arms
            // (still capped by `remaining` so a steady trickle can't outlive the
            // ceiling).
            let wait = if seen_first_byte {
                idle_timeout.min(remaining)
            } else {
                remaining
            };
            match tokio::time::timeout(wait, stdout.read(&mut chunk)).await {
                Ok(Ok(0)) => break, // EOF — stdout closed
                Ok(Ok(n)) => {
                    seen_first_byte = true;
                    stdout_buf.extend_from_slice(&chunk[..n]);
                }
                Ok(Err(e)) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(format!("`{program}` stdout read error: {e}"));
                }
                Err(_) if !seen_first_byte => {
                    // The wait that elapsed was the hard-ceiling remaining time
                    // (idle is not armed before the first byte) — a true silent
                    // hang, reported as the overall timeout.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(format!(
                        "`{program}` timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                Err(_) => {
                    // **Idle timeout** — output started, then no further byte for
                    // `idle_timeout` (a base that hangs while holding the stdout
                    // pipe open). Kill + return a distinguishable, retriable error.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let bytes = stdout_buf.len();
                    return Err(format!(
                        "`{program}` timed out after {}s of stdout silence (hang while holding the pipe open? bytes so far: {bytes}). Set UMADEV_IDLE_TIMEOUT_SECS to adjust.",
                        idle_timeout.as_secs()
                    ));
                }
            }
        }
    }

    // stdout drained to EOF — the child is normally exiting now. Bound the exit
    // wait by the remaining hard ceiling so a process that closes stdout but then
    // refuses to exit can't hang us either.
    let remaining = timeout.saturating_sub(started.elapsed());
    let status = match tokio::time::timeout(remaining, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("`{program}` failed: {e}")),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(format!(
                "`{program}` timed out after {}s",
                timeout.as_secs()
            ));
        }
    };

    // H1: the child has exited, but a grandchild that inherited the stderr write
    // fd can hold the pipe open so this read never EOFs. Reap under a bounded
    // flush grace so a leaked fd can't hang the call forever — the exit status +
    // stdout are already in hand.
    let stderr_buf = reap_bounded(stderr_task).await.unwrap_or_default();
    Ok((status, stdout_buf, stderr_buf))
}

/// Apply extra env overrides to a child command before spawn (an empty value
/// removes the inherited var). UmaDev injects no *model/auth* env into the base —
/// the child inherits the user's full environment so the base self-authenticates
/// with its own login / API. The one thing UmaDev DOES set on a base it drives is
/// [`GOVERN_ROOT_ENV`] (via [`govern_root_env`]), the signal that scopes the
/// `PreToolUse` governance hook to this run.
fn apply_provider_env(cmd: &mut Command, env: &[(String, String)]) {
    for (key, value) in env {
        if value.is_empty() {
            cmd.env_remove(key);
        } else {
            cmd.env(key, value);
        }
    }
}

/// Resolve a bare program name to a spawnable path — bullet-proof base
/// detection that survives every install method (npm / native installer /
/// Homebrew / pnpm / yarn / bun / deno / volta / nvm / asdf / standalone
/// script / Windows Scoop / Chocolatey / winget).
///
/// Two failure modes this defends against:
///
/// 1. **On `PATH` but mis-shimmed (Windows).** npm installs a base as BOTH
///    `codex` (a no-extension *nix shell shim) and `codex.cmd` (the Windows
///    shim) in the same dir. `CreateProcess` (and thus `Command::new`) only
///    auto-appends `.exe`, so a bare `codex` matched the shell shim — not a PE
///    — and spawning it gave os error 193 ("not a valid Win32 application")
///    → "not installed". We search `PATH` over `PATHEXT` with **extensions
///    first** so `.cmd`/`.exe`/`.bat` win over the bare *nix shim.
///
/// 2. **Installed but not on the *process* `PATH`.** A login shell's `PATH`
///    (with `~/.local/bin`, Homebrew, volta, asdf shims, …) is routinely
///    richer than the env a GUI-launched or service-spawned process inherits.
///    When `PATH` misses, we fail-open-scan every well-known install location
///    for this binary so "installed but not on my PATH" still resolves.
///
/// Order is **`PATH` first, known install dirs second** — a `PATH` hit is
/// authoritative and matches the user's shell. Returns the first hit's full
/// path. Fail-open: a missing/unreadable dir is skipped, and when nothing
/// matches we return the input unchanged so the spawn surfaces the real error
/// (and `probe`'s `--version` stays the final installed-or-not arbiter — a
/// wrong-dir hit just fails `--version`, never a false "installed").
#[must_use]
pub fn resolve_program(program: &str) -> String {
    // An explicit path (relative or absolute) is taken as-is — the caller
    // already pinned the binary; we never second-guess it.
    if program.contains(std::path::is_separator) {
        return program.to_string();
    }
    // 0. Explicit override — the ultimate escape hatch when a base lives
    //    somewhere no heuristic finds it. `UMADEV_<NAME>_BIN` (e.g.
    //    UMADEV_CLAUDE_BIN / UMADEV_CODEX_BIN / UMADEV_OPENCODE_BIN) pins the
    //    exact path; honored only when it points at a real file, so a stale
    //    override falls through to normal detection instead of blocking it. A
    //    `.cmd` is fine — spawn_parts still routes it through `cmd /c`.
    let override_var = format!(
        "UMADEV_{}_BIN",
        program.to_ascii_uppercase().replace('-', "_")
    );
    if let Ok(p) = std::env::var(&override_var) {
        if std::path::Path::new(p.trim()).is_file() {
            return p.trim().to_string();
        }
    }
    let exts = path_extensions();
    // 1. PATH first — authoritative, matches the user's shell.
    if let Ok(path_var) = std::env::var("PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(sep) {
            if let Some(hit) = match_in_dir(std::path::Path::new(dir), program, &exts) {
                return hit;
            }
        }
    }
    // 2. Known install locations second — "installed but not on this
    //    process's PATH" (GUI/service launch, or a richer login-shell PATH).
    for dir in known_install_dirs(program) {
        if let Some(hit) = match_in_dir(&dir, program, &exts) {
            return hit;
        }
    }
    // Fail-open: nothing matched — hand back the bare name so the spawn (and,
    // for probes, the subsequent `--version`) surfaces the real error.
    program.to_string()
}

/// Candidate file extensions to try for `program`, **most-specific first**.
///
/// On Windows we honor `PATHEXT` (defaulting to the standard set) and append a
/// trailing empty extension so a bare-named file is the LAST resort: npm drops
/// both `codex` (a *nix shell shim, not a PE → os error 193) and `codex.cmd`
/// in the same dir, so `.cmd`/`.exe`/`.bat` MUST win over the bare name. Off
/// Windows there are no extensions — just the bare name.
fn path_extensions() -> Vec<String> {
    if cfg!(windows) {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        pathext
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    }
}

/// Return the full path of `program{ext}` for the first `ext` that names a
/// real file in `dir`, or `None`. Fail-open: an empty/unreadable dir yields
/// `None` (the `is_file` probe simply returns false). `exts` is ordered
/// most-specific-first (see [`path_extensions`]).
fn match_in_dir(dir: &std::path::Path, program: &str, exts: &[String]) -> Option<String> {
    if dir.as_os_str().is_empty() {
        return None;
    }
    for ext in exts {
        let candidate = dir.join(format!("{program}{ext}"));
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Read an environment variable into a non-empty `PathBuf`, or `None`. Empty
/// values are treated as unset so we never join onto `""`.
fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `true` when ANY of `keys` names a non-empty environment variable. Used by the
/// auth probes to honor the env-var auth paths a base accepts (e.g. claude's
/// `ANTHROPIC_API_KEY` / cloud-provider toggles) as an instant, subprocess-free
/// "logged in" signal. An empty value counts as unset (a base ignores `KEY=`).
pub(crate) fn any_env_set(keys: &[&str]) -> bool {
    keys.iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// The user's home directory, derived without pulling in the `dirs` crate:
/// `$HOME` on Unix, `%USERPROFILE%` (then `%HOMEDRIVE%%HOMEPATH%`) on Windows.
pub(crate) fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env_dir("USERPROFILE").or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            if drive.is_empty() || path.is_empty() {
                return None;
            }
            let mut p = drive;
            p.push(path);
            Some(PathBuf::from(p))
        })
    } else {
        env_dir("HOME")
    }
}

/// The platform data directory a tool stores per-user state in, following the
/// same resolution the three base CLIs use:
///
/// - Unix (Linux/macOS): `$XDG_DATA_HOME` if set, else `~/.local/share`.
/// - Windows: `%XDG_DATA_HOME%` if set, else `%LOCALAPPDATA%`, else
///   `~\AppData\Local`.
///
/// Used by the opencode auth probe to locate `…/opencode/auth.json` without
/// pulling in the `dirs`/`directories` crates (the dependency-light contract).
/// Returns `None` when no base directory can be derived (fail-open: the caller
/// then falls through to the auth-status subcommand).
fn data_dir() -> Option<PathBuf> {
    if let Some(xdg) = env_dir("XDG_DATA_HOME") {
        return Some(xdg);
    }
    if cfg!(windows) {
        env_dir("LOCALAPPDATA").or_else(|| home_dir().map(|h| h.join("AppData").join("Local")))
    } else {
        home_dir().map(|h| h.join(".local").join("share"))
    }
}

/// Enumerate the immediate subdirectory `bin` dirs under `parent` — used for
/// version-manager layouts like `~/.nvm/versions/node/<v>/bin` where the
/// version segment is unpredictable. Fail-open: an unreadable `parent` yields
/// an empty vec.
fn versioned_node_bins(parent: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|p| p.is_dir())
        .collect()
}

/// Every well-known install location for a base/tool CLI, across package
/// managers and OSes. Each is tried (over [`path_extensions`]) only after a
/// plain `PATH` lookup misses — so a normal install is unaffected and this is
/// purely a fail-open safety net for "installed but not on my PATH".
///
/// `program` seeds the per-base standalone dirs (`~/.codex/bin`,
/// `~/.opencode/bin`, `%LOCALAPPDATA%\Programs\codex`, …): standalone
/// installers drop the binary under a dir named after the tool.
///
/// Covers (cross-platform HOME-relative): npm-global, volta, bun, deno, yarn
/// (classic + berry global), pnpm, cargo, asdf shims, nvm node versions,
/// `~/.local/bin`, `~/bin`. Unix system dirs: `/usr/local/bin`,
/// `/opt/homebrew/bin` (+ `opt/*/bin` cellar links), `/usr/bin`, `/bin`, and
/// the npm global prefix (`NPM_CONFIG_PREFIX` or common defaults). Windows:
/// `%APPDATA%\npm`, `%LOCALAPPDATA%\Programs\{program}`, Program Files,
/// Volta, Scoop shims, Chocolatey bin, and the winget Links shim dir.
fn known_install_dirs(program: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // ---- Cross-platform, HOME-relative (every package manager's user dir) ----
    if let Some(home) = home_dir() {
        let rel = [
            ".local/bin",
            "bin",
            ".npm-global/bin",
            ".volta/bin",
            ".bun/bin",
            ".deno/bin",
            ".yarn/bin",
            ".config/yarn/global/node_modules/.bin",
            ".local/share/pnpm",
            ".cargo/bin",
            ".asdf/shims",
        ];
        for r in rel {
            dirs.push(home.join(r));
        }
        // Per-base standalone installers: `~/.codex/bin`, `~/.opencode/bin`,
        // `~/.claude/bin`, plus the bare `~/.{program}` dir some scripts use.
        dirs.push(home.join(format!(".{program}/bin")));
        dirs.push(home.join(format!(".{program}")));
        // nvm: node lives under an unpredictable version segment — enumerate.
        dirs.extend(versioned_node_bins(&home.join(".nvm/versions/node")));
    }

    if cfg!(windows) {
        // ---- Windows: package-manager + installer locations ----
        if let Some(appdata) = env_dir("APPDATA") {
            dirs.push(appdata.join("npm")); // npm global on Windows
        }
        if let Some(local) = env_dir("LOCALAPPDATA") {
            dirs.push(local.join(format!("Programs\\{program}")));
            dirs.push(local.join(format!("Programs\\{program}\\bin")));
            dirs.push(local.join("Volta\\bin"));
            dirs.push(local.join("Microsoft\\WinGet\\Links")); // winget shims
        }
        for pf in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(p) = env_dir(pf) {
                dirs.push(p.join(program));
                dirs.push(p.join(format!("{program}\\bin")));
            }
        }
        if let Some(profile) = env_dir("USERPROFILE") {
            dirs.push(profile.join(".local\\bin"));
            dirs.push(profile.join("scoop\\shims")); // Scoop
        }
        if let Some(choco) = env_dir("ChocolateyInstall") {
            dirs.push(choco.join("bin"));
        } else {
            dirs.push(PathBuf::from(r"C:\ProgramData\chocolatey\bin"));
        }
    } else {
        // ---- Unix: system + Homebrew + npm-prefix locations ----
        for d in ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin"] {
            dirs.push(PathBuf::from(d));
        }
        // Homebrew cellar keg-only links: `/opt/homebrew/opt/*/bin`.
        if let Ok(entries) = std::fs::read_dir("/opt/homebrew/opt") {
            for e in entries.flatten() {
                dirs.push(e.path().join("bin"));
            }
        }
        // npm global prefix: explicit override, else the common bin dirs.
        if let Some(prefix) = env_dir("NPM_CONFIG_PREFIX") {
            dirs.push(prefix.join("bin"));
        }
        dirs.push(PathBuf::from("/usr/local/bin")); // common npm default prefix
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
    }

    dirs
}

/// Windows-aware spawn target for a base/tool CLI. `.cmd`/`.bat` shims (how npm
/// installs `claude` / `codex` / `npm` on Windows) are NOT PE executables, so
/// `CreateProcess` refuses them with os error 193 ("not a valid Win32
/// application"). Route those through `cmd /c`. Returns `(program, leading
/// args)`; callers append their own args after. No-op off Windows.
#[must_use]
pub fn spawn_parts(program: &str) -> (String, Vec<String>) {
    let resolved = resolve_program(program);
    let ext = std::path::Path::new(&resolved)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if cfg!(windows) && (ext == "cmd" || ext == "bat") {
        ("cmd".to_string(), vec!["/c".to_string(), resolved])
    } else {
        (resolved, Vec::new())
    }
}

/// A `std::process::Command` for a base/tool CLI, Windows-aware (`.cmd`/`.bat`
/// routed through `cmd /c` -- see [`spawn_parts`]). Used by the binary's npm
/// calls; the host's own async spawns use [`spawn_parts`] directly.
#[must_use]
pub fn std_command(program: &str) -> std::process::Command {
    let (prog, lead) = spawn_parts(program);
    let mut c = std::process::Command::new(prog);
    c.args(lead);
    c
}

/// Conservative command-line length budget, in bytes, above which UmaDev moves a
/// large prompt / system-prompt OFF the command line (to the child's stdin, or
/// for `claude`'s firmware to a temp file passed via `--append-system-prompt-file`).
///
/// Windows `cmd.exe` caps the ENTIRE command line at ~8191 chars, and npm installs
/// the base CLIs as `.cmd` shims that [`spawn_parts`] invokes as
/// `cmd /c <resolved path> <args…>` — so on Windows the whole line (program + every
/// flag + the multi-KB prompt/firmware) must fit under that cap or it is silently
/// truncated → corrupted generation. 7000 leaves >1000 chars of headroom under 8191
/// for the `cmd /c` wrapper, the resolved program path, and quoting expansion, while
/// keeping every normal prompt on the fast argv path. Off Windows there is no such
/// per-line cap; the only real bound is Linux's 128 KiB `MAX_ARG_STRLEN` per single
/// arg, so the budget is a high 120_000 backstop — merged prompts are already capped
/// at 110_000 by [`merge_prompt`], so this never triggers on the normal mac/Linux
/// path and the argv fast path is preserved there. `UMADEV_CMDLINE_BUDGET` overrides
/// the derived value (an escape hatch for a machine whose effective limit differs).
#[must_use]
pub(crate) fn command_line_budget() -> usize {
    command_line_budget_from(std::env::var("UMADEV_CMDLINE_BUDGET").ok().as_deref())
}

/// [`command_line_budget`] with the `UMADEV_CMDLINE_BUDGET` value passed in — pure
/// (no global env read), so the override + platform-default logic is testable
/// without mutating process env (which would race parallel tests). A positive
/// integer override wins; junk / zero / absent → the platform default.
#[must_use]
fn command_line_budget_from(override_val: Option<&str>) -> usize {
    if let Some(v) = override_val
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return v;
    }
    if cfg!(windows) {
        7_000
    } else {
        120_000
    }
}

/// Approximate total command-line length for a token sequence (program + every
/// arg, in the order they are spawned). Adds a small per-token quoting/separator
/// allowance and a fixed wrapper allowance for the Windows `.cmd` → `cmd /c
/// <resolved path>` form (whose resolved path length the caller already includes
/// via [`spawn_parts`]'s `lead`). Deterministic; used only to decide argv vs
/// stdin/file delivery, so a slight over-estimate is the safe direction.
#[must_use]
pub(crate) fn command_line_len<'a, I: IntoIterator<Item = &'a str>>(tokens: I) -> usize {
    // space + up to two quote chars per token, and a wrapper allowance for
    // `cmd /c ` plus quoting expansion the estimate cannot see per-char.
    const PER_TOKEN_OVERHEAD: usize = 3;
    const WRAP_OVERHEAD: usize = 512;
    WRAP_OVERHEAD
        + tokens
            .into_iter()
            .map(|t| t.len() + PER_TOKEN_OVERHEAD)
            .sum::<usize>()
}

/// The EFFECTIVE prompt channel for a call: an [`PromptChannel::Arg`] prompt that
/// would push the whole command line past [`command_line_budget`] is delivered via
/// [`PromptChannel::Stdin`] instead, so the Windows `cmd.exe` ~8191 cap can't
/// truncate it. Both single-shot bases read the prompt from stdin — `claude --print`
/// and `opencode run` (verified) — so the diverted prompt arrives intact. A `Stdin`
/// call stays `Stdin`; an `Arg` prompt that fits stays `Arg` (the fast path, so small
/// prompts and mac/Linux are unchanged). `program`/`lead` are the resolved spawn
/// tokens from [`spawn_parts`], so the wrapped `.cmd` form is accounted for.
fn effective_prompt_channel(
    call: &SubprocessCall<'_>,
    program: &str,
    lead: &[String],
) -> PromptChannel {
    if !matches!(call.channel, PromptChannel::Arg) || call.prompt.is_empty() {
        return call.channel;
    }
    let line = command_line_len(
        std::iter::once(program)
            .chain(lead.iter().map(String::as_str))
            .chain(call.args.iter().map(String::as_str))
            .chain(std::iter::once(call.prompt)),
    );
    if line > command_line_budget() {
        PromptChannel::Stdin
    } else {
        PromptChannel::Arg
    }
}

/// Run a host CLI subprocess. Errors carry a human-readable string suitable for
/// `RuntimeError::HostProcess`.
pub(crate) async fn run_subprocess(call: SubprocessCall<'_>) -> Result<SubprocessOutput, String> {
    let started = Instant::now();
    let (program, lead) = spawn_parts(call.program);
    // An oversized `Arg` prompt is delivered via stdin instead, so a Windows `.cmd`
    // shim (`cmd /c …`, ~8191-char cap) can't truncate it (see
    // `effective_prompt_channel`). Small prompts / mac+Linux keep the argv fast path.
    let channel = effective_prompt_channel(&call, &program, &lead);
    let mut cmd = Command::new(program);
    cmd.args(&lead);
    cmd.args(call.args);
    if matches!(channel, PromptChannel::Arg) && !call.prompt.is_empty() {
        // Skip an EMPTY prompt: appending "" as a CLI arg is never intended and
        // breaks strict tools (e.g. GNU `printenv VAR ""` exits 1 where BSD exits
        // 0). A real base prompt is always non-empty, so this only fixes the edge.
        cmd.arg(call.prompt);
    }
    cmd.current_dir(call.workspace);
    apply_provider_env(&mut cmd, call.env);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("`{}` not found on PATH", call.program)
        } else {
            format!("failed to spawn `{}`: {e}", call.program)
        }
    })?;

    // M4: write the prompt CONCURRENTLY with draining stdout, not before it. A
    // `write_all` that fully completes before any stdout read DEADLOCKS when the
    // prompt exceeds the OS pipe buffer (~64 KiB) AND the base emits output
    // before consuming all of stdin (codex's stdin channel): the base blocks
    // writing stdout (its pipe full, we aren't reading yet) so it stops reading
    // stdin, so our write blocks — and `drain_and_wait`'s ceiling hasn't started.
    // Spawning the writer lets stdout drain while the prompt streams in.
    let stdin_writer = if matches!(channel, PromptChannel::Stdin) {
        child.stdin.take().map(|mut stdin| {
            let prompt = call.prompt.as_bytes().to_vec();
            tokio::spawn(async move {
                // Best-effort: a base that exits early closes its stdin read end
                // (EPIPE) — `drain_and_wait` surfaces the real outcome, not this.
                // `shutdown` flushes the buffered prompt AND signals EOF (without
                // it a plain write + drop can leave bytes unflushed, so the base
                // reads an EMPTY stdin and bails, e.g. codex "No prompt provided
                // via stdin" → exit 1).
                if stdin.write_all(&prompt).await.is_ok() {
                    let _ = stdin.shutdown().await;
                }
            })
        })
    } else {
        // Arg channel: the prompt is a CLI arg, so we never write stdin. But
        // the pipe is still open — take and drop it so the child sees EOF
        // immediately instead of blocking on an idle stdin (some CLIs peek
        // stdin in non-interactive mode and would otherwise hang to timeout).
        drop(child.stdin.take());
        None
    };

    // Drain both pipes AND wait for exit under ONE deadline (see
    // `drain_and_wait`): the reads themselves must be bounded, or a child that
    // emits output then hangs with its stdout pipe open blocks forever and
    // defeats the timeout.
    let drained = drain_and_wait(&mut child, call.timeout, call.program).await;
    // Reap the writer (the child is now dead/exited, so it returns at once) —
    // before propagating any drain error, so the task can never leak.
    if let Some(writer) = stdin_writer {
        let _ = reap_bounded(writer).await;
    }
    let (status, stdout_buf, stderr_buf) = drained?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
        return Err(format!(
            "`{}` exited with code {code}: {}",
            call.program,
            truncate_on_boundary(&stderr, 2048).trim()
        ));
    }

    // When exit-0 but stdout is empty, inspect stderr — many host CLIs
    // (Claude Code, Codex) write auth/logged-out errors to stderr while
    // still returning exit code 0. Surface these so the user gets an
    // actionable error instead of a silent empty-body template fallback.
    let stdout_raw = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr_raw = String::from_utf8_lossy(&stderr_buf).into_owned();
    if stdout_raw.trim().is_empty() && !stderr_raw.trim().is_empty() {
        return Err(format!(
            "`{}` exited 0 but stdout is empty — stderr: {}",
            call.program,
            truncate_on_boundary(&stderr_raw, 2048).trim()
        ));
    }

    let stdout = {
        let orig_len = stdout_raw.len();
        if orig_len > 262_144 {
            let mut s = truncate_on_boundary(&stdout_raw, 262_144).to_string();
            s.push_str("\n...[umadev: stdout truncated at 256 KiB]");
            // Also surface in the log so the truncation isn't only visible
            // in the host's stdout tail (a long run might scroll past it).
            tracing::warn!(
                program = call.program,
                orig_len,
                "host stdout exceeded 256 KiB and was truncated"
            );
            s
        } else {
            stdout_raw
        }
    };
    let cleaned = clean_output(&stdout);
    tracing::debug!(
        program = call.program,
        millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        bytes = cleaned.len(),
        "host subprocess completed"
    );
    Ok(SubprocessOutput { stdout: cleaned })
}

/// Default hard ceiling for an **auth probe** subprocess (`<base> auth status`
/// / `<base> login status`). Deliberately short (5s): an auth-status command is
/// a local credential check, not a model call, so it returns in well under a
/// second when healthy. A short ceiling keeps a hung/misbehaving status command
/// from stalling the first-run picker — on timeout the probe fail-opens to
/// [`AuthState::Unknown`]. Tunable via `UMADEV_AUTH_PROBE_SECS`.
pub(crate) fn auth_probe_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("UMADEV_AUTH_PROBE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&s| s > 0)
            .unwrap_or(5),
    )
}

/// Run a base's auth-status subcommand (e.g. `claude auth status`) purely to
/// read its output — a cheap authenticated no-op, NEVER a real generation.
///
/// Returns the combined `stdout` (and, on a clean exit, nothing else needed) so
/// the caller can pattern-match the base's "logged in" wording. **Fail-open:**
/// any spawn failure, non-zero exit, OR timeout returns `None` so the caller
/// resolves to [`AuthState::Unknown`] rather than guessing. The prompt channel
/// is unused (stdin is closed immediately) so a status command that peeks stdin
/// can't hang. Bounded by [`auth_probe_timeout`].
///
/// `success_required`: when `true`, a non-zero exit yields `None` (the caller
/// treats "command failed" as indeterminate); when `false`, the stdout is
/// returned even on a non-zero exit so the caller can inspect a "Not logged in"
/// message a base prints with a non-zero code. Cross-platform: spawns via
/// [`spawn_parts`] so a Windows `.cmd` shim is routed through `cmd /c`.
pub(crate) async fn run_auth_status(
    program: &str,
    args: &[String],
    success_required: bool,
) -> Option<String> {
    let (prog, lead) = spawn_parts(program);
    let mut cmd = Command::new(prog);
    cmd.args(&lead);
    cmd.args(args);
    cmd.current_dir(default_workspace());
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().ok()?;
    // Close stdin immediately (EOF) so a status command that peeks stdin in a
    // non-interactive context returns instead of blocking to the timeout.
    drop(child.stdin.take());

    let (status, stdout_buf, stderr_buf) =
        drain_and_wait(&mut child, auth_probe_timeout(), program)
            .await
            .ok()?;

    if success_required && !status.success() {
        return None;
    }
    // Some bases print the status to stderr; fold both so the matcher sees it.
    let mut out = String::from_utf8_lossy(&stdout_buf).into_owned();
    let err = String::from_utf8_lossy(&stderr_buf);
    if !err.trim().is_empty() {
        out.push('\n');
        out.push_str(&err);
    }
    Some(out)
}

/// Run a host CLI subprocess in **streaming mode**.
///
/// Unlike [`run_subprocess`] (which waits for the entire stdout via
/// `read_to_end`), this function reads stdout **line by line** and calls
/// `on_line` for each line as it arrives. This is essential for
/// `claude --output-format stream-json` and `codex --json`, which emit
/// newline-delimited JSON events in real time — the user sees the worker's
/// tool calls and text deltas as they happen, not after a 3-minute wait.
///
/// Returns the full concatenated stdout (all lines joined) so the caller
/// can still assemble the final response. Each line is also passed to
/// `on_line` for real-time parsing.
///
/// **Timeout model (two-phase, "first-line grace").** The per-line idle
/// watchdog (`idle_timeout`, default 300s, `UMADEV_IDLE_TIMEOUT_SECS`) measures
/// line-to-line *silence*, so it is armed only AFTER the first stdout line. Before
/// the first line the sole bound is the remaining time to the hard `call.timeout`
/// deadline — a base whose first token is slow (e.g. plain-text `opencode run`,
/// where the first line is the answer itself) is not wrongly killed mid-generation
/// before it has emitted anything. The hard `call.timeout` ceiling always applies
/// and the grace can never bypass it (a true hang with no output still trips
/// `call.timeout`). Kill-on-drop behaviour mirrors [`run_subprocess`].
#[allow(clippy::too_many_lines)] // single coherent subprocess operation
pub(crate) async fn run_subprocess_streaming(
    call: SubprocessCall<'_>,
    on_line: &(dyn Fn(&str) + Send + Sync),
) -> Result<SubprocessOutput, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let started = Instant::now();
    let (program, lead) = spawn_parts(call.program);
    // An oversized `Arg` prompt is delivered via stdin instead, so a Windows `.cmd`
    // shim (`cmd /c …`, ~8191-char cap) can't truncate it (see
    // `effective_prompt_channel`). Small prompts / mac+Linux keep the argv fast path.
    let channel = effective_prompt_channel(&call, &program, &lead);
    let mut cmd = Command::new(program);
    cmd.args(&lead);
    cmd.args(call.args);
    if matches!(channel, PromptChannel::Arg) && !call.prompt.is_empty() {
        // Skip an EMPTY prompt: appending "" as a CLI arg is never intended and
        // breaks strict tools (e.g. GNU `printenv VAR ""` exits 1 where BSD exits
        // 0). A real base prompt is always non-empty, so this only fixes the edge.
        cmd.arg(call.prompt);
    }
    cmd.current_dir(call.workspace);
    apply_provider_env(&mut cmd, call.env);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("`{}` not found on PATH", call.program)
        } else {
            format!("failed to spawn `{}`: {e}", call.program)
        }
    })?;

    // M4: write the prompt CONCURRENTLY with streaming stdout (see
    // `run_subprocess`) — a >64 KiB prompt that fully writes before any stdout
    // read deadlocks a base that emits before draining all of stdin. Spawning
    // the writer lets the stdout loop below drain while the prompt streams in.
    let stdin_writer = if matches!(channel, PromptChannel::Stdin) {
        child.stdin.take().map(|mut stdin| {
            let prompt = call.prompt.as_bytes().to_vec();
            tokio::spawn(async move {
                // Flush + close the write half (a bare write + drop can leave the
                // prompt unflushed, starving the child); best-effort on EPIPE.
                if stdin.write_all(&prompt).await.is_ok() {
                    let _ = stdin.shutdown().await;
                }
            })
        })
    } else {
        // Arg channel: the prompt is a CLI arg, so we never write stdin. Drop the
        // pipe so the child sees EOF immediately — otherwise a CLI that peeks
        // stdin in non-interactive `stream-json` mode blocks until the idle
        // watchdog kills it (the same defence `run_subprocess` already has).
        drop(child.stdin.take());
        None
    };

    // Read stderr in a separate task so it doesn't block stdout streaming,
    // bounded by `STDERR_CAPTURE_CAP` (a flooding base can't grow it unboundedly).
    let stderr_task = spawn_stderr_capture(child.stderr.take());

    // Stream stdout line by line.
    // **Watchdog**: once the stream is live, a per-line idle timeout (not the
    // full call timeout) catches a mid-stream hang (stream-json hang bug #53584)
    // fast — if no further line arrives within `idle_timeout`, we kill + error so
    // the caller can retry. The overall `call.timeout` is always the hard ceiling.
    // Default 300s (was 120s): deep web-research / long "thinking" turns can go
    // silent on stdout for >2min between the lifecycle lines and the next token —
    // a real, healthy long pause, not a hang. A 120s idle watchdog mis-killed
    // those mid-research and forced a full non-streaming re-run from scratch
    // (exactly the multi-minute research stall this addresses). 300s still trips
    // well before the hard `call.timeout` ceiling, so a genuine stream-json hang
    // is still caught — and a tighter value is one `UMADEV_IDLE_TIMEOUT_SECS`
    // away for callers who want the old aggressiveness.
    let idle_timeout = std::cmp::min(
        call.timeout,
        Duration::from_secs(
            std::env::var("UMADEV_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
        ),
    );
    let mut all_lines = Vec::new();
    // **First-line grace.** The idle watchdog measures line-to-line *silence*,
    // which only makes sense once a line has been seen. Some bases (claude /
    // codex with `stream-json` / `--json`) emit lifecycle lines (system/init,
    // thread.started) almost immediately, so for them the watchdog arms within
    // milliseconds and behaviour is unchanged. But a plain-text base (opencode
    // `run`) emits NOTHING until the model produces its first token — the first
    // stdout line is the answer itself. A slow first token (big prompt / slow
    // model / rate-limit) could legitimately exceed `idle_timeout`, and killing
    // there would wrongly fall the driver back to a fresh non-streaming
    // `complete` and re-run the whole generation (doubling wall-clock). So
    // BEFORE the first line we do NOT arm the idle sub-timeout: the only bound is
    // the remaining time to the hard `call.timeout` deadline (real-hang backstop
    // that the grace can never bypass). AFTER the first line, every subsequent
    // wait is bounded by `idle_timeout` (still also capped by the hard ceiling),
    // restoring the mid-stream-hang protection.
    if let Some(stdout) = child.stdout.take() {
        // Read raw bytes per line and decode LOSSY (not `.lines()`/`next_line`):
        // `next_line` returns `Err` on a single invalid UTF-8 byte, which the old
        // `while let Ok(Some)` treated as end-of-stream — discarding the rest of a
        // long stream-json turn AND emitting a spurious "ended unexpectedly". A
        // `read_until('\n')` + `from_utf8_lossy` tolerates the bad byte (mirrors
        // `pump_sse`'s decode) and keeps streaming.
        let mut reader = BufReader::new(stdout);
        let mut line_buf = Vec::new();
        let mut seen_first_line = false;
        loop {
            let remaining = call.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(format!(
                    "`{}` timed out after {}s",
                    call.program,
                    call.timeout.as_secs()
                ));
            }
            // First-line grace: until the first line, the only deadline is the
            // hard ceiling's remaining time. After it, the per-line idle timeout
            // arms — still capped by `remaining` so a steady trickle can never
            // outlive `call.timeout`.
            let wait = if seen_first_line {
                idle_timeout.min(remaining)
            } else {
                remaining
            };
            line_buf.clear();
            match tokio::time::timeout(wait, reader.read_until(b'\n', &mut line_buf)).await {
                Ok(Ok(0)) => break, // EOF — stdout closed
                Ok(Ok(_)) => {
                    seen_first_line = true;
                    let line = String::from_utf8_lossy(&line_buf);
                    let line = line.trim_end_matches(['\r', '\n']).to_string();
                    on_line(&line);
                    all_lines.push(line);
                }
                Ok(Err(e)) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(format!("`{}` stdout read error: {e}", call.program));
                }
                Err(_) if !seen_first_line => {
                    // The wait that elapsed was the hard-ceiling remaining time
                    // (the idle sub-timeout is not armed before the first line),
                    // so a timeout here means we hit `call.timeout` with no output
                    // at all — a true hang, reported as the overall timeout.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(format!(
                        "`{}` timed out after {}s",
                        call.program,
                        call.timeout.as_secs()
                    ));
                }
                Err(_) => {
                    // **Idle timeout** — the stream went live, then no further
                    // line for `idle_timeout` (the stream-json hang scenario,
                    // #53584). Kill + return a distinguishable error so callers
                    // can retry.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let lines_so_far = all_lines.len();
                    return Err(format!(
                        "`{}` idle timeout: no stdout for {}s (stream-json hang? lines so far: {lines_so_far}). Set UMADEV_IDLE_TIMEOUT_SECS to adjust.",
                        call.program,
                        idle_timeout.as_secs()
                    ));
                }
            }
        }
    }

    // H2: bound the exit wait by the remaining hard ceiling (asymmetric with the
    // single-shot `drain_and_wait`, which already does this). A base that closes
    // stdout then lingers in teardown — or a grandchild that keeps the process
    // group busy — would otherwise hang here past `call.timeout` forever.
    let remaining = call.timeout.saturating_sub(started.elapsed());
    let status = match tokio::time::timeout(remaining, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("`{}` failed: {e}", call.program)),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            if let Some(writer) = stdin_writer {
                let _ = reap_bounded(writer).await;
            }
            return Err(format!(
                "`{}` timed out after {}s",
                call.program,
                call.timeout.as_secs()
            ));
        }
    };

    // Reap the concurrent stdin writer (the child has exited, so it returns at
    // once) and the stderr capture under the bounded flush grace (H1 mirror — a
    // leaked grandchild stderr fd must not hang us).
    if let Some(writer) = stdin_writer {
        let _ = reap_bounded(writer).await;
    }
    let stderr_buf = reap_bounded(stderr_task).await.unwrap_or_default();

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
        return Err(format!(
            "`{}` exited with code {code}: {}",
            call.program,
            truncate_on_boundary(&stderr, 2048).trim()
        ));
    }

    let stdout = all_lines.join("\n");
    let stdout = clean_output(&stdout);

    if stdout.trim().is_empty() && !stderr_buf.is_empty() {
        let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
        return Err(format!(
            "`{}` exited 0 but stdout is empty — stderr: {}",
            call.program,
            truncate_on_boundary(&stderr, 2048).trim()
        ));
    }

    tracing::debug!(
        program = call.program,
        millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        lines = all_lines.len(),
        "host streaming subprocess completed"
    );
    Ok(SubprocessOutput { stdout })
}

/// Strip common host-CLI noise: ANSI escape codes and a leading
/// `assistant:` style prefix some CLIs emit.
pub(crate) fn clean_output(raw: &str) -> String {
    let no_ansi = strip_ansi(raw);
    no_ansi.trim().to_string()
}

/// Map a `run_subprocess` error string into a typed [`RuntimeError`],
/// turning "timed out after Ns" into [`RuntimeError::Timeout`] and
/// everything else into [`RuntimeError::HostProcess`].
///
/// Shared by every host driver so the timeout-vs-other-failure split is
/// consistent (previously `codex.rs` mapped *all* errors, including
/// timeouts, to `HostProcess`, which broke caller-side timeout detection).
pub(crate) fn map_subprocess_error(err: String) -> umadev_runtime::RuntimeError {
    if err.contains("timed out") || err.contains("idle timeout") {
        let secs = err
            .split("after ")
            .nth(1)
            .and_then(|s| s.split('s').next())
            .and_then(|n| n.parse::<u64>().ok())
            .or_else(|| {
                err.split("for ")
                    .nth(1)
                    .and_then(|s| s.split('s').next())
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .unwrap_or(300);
        umadev_runtime::RuntimeError::Timeout(secs, err)
    } else {
        umadev_runtime::RuntimeError::HostProcess(err)
    }
}

/// Read the `UMADEV_WORKER_TIMEOUT` env override (seconds). Returns
/// `DEFAULT_TIMEOUT` when unset or unparseable. Used by every driver so
/// the timeout knob works for both backends, not just `claude-code`.
pub(crate) fn worker_timeout_from_env() -> Duration {
    std::env::var("UMADEV_WORKER_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip CSI sequence: ESC [ ... <final byte 0x40-0x7E>
            if chars.peek() == Some(&'[') {
                chars.next();
                for inner in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&inner) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Build `["--model", <id>]` when the request carries a real model id, else an
/// empty vec. Shared by the `claude`/`codex` drivers, whose `--model` flag
/// (`global` on codex, a top-level flag on claude) takes a plain id/alias.
/// Skips an empty id and the internal test/offline placeholders so a default
/// run never injects a bogus `--model`.
#[must_use]
pub(crate) fn model_args(model: &str) -> Vec<String> {
    let m = model.trim();
    if m.is_empty() || matches!(m, "stub" | "flaky" | "m" | "offline") {
        Vec::new()
    } else {
        vec!["--model".to_string(), m.to_string()]
    }
}

/// Merge a [`CompletionRequest`]'s system + user messages into a single
/// prompt string for host CLIs that take only one prompt.
///
/// [`CompletionRequest`]: umadev_runtime::CompletionRequest
#[must_use]
pub(crate) fn merge_prompt(req: &umadev_runtime::CompletionRequest) -> String {
    // The whole merged prompt becomes ONE argv entry; Linux caps a single arg at
    // MAX_ARG_STRLEN (128 KB) and over it the spawn fails with E2BIG. The bloat
    // lives in the SYSTEM (design anti-slop + expert knowledge + lessons + MCP),
    // while the user content (requirement + bounded excerpts) is small and MUST
    // survive — so we trim the system to a ceiling, then backstop the total.
    const MAX_SYSTEM: usize = 90_000;
    const MAX_TOTAL: usize = 110_000;
    const TRIM_MARKER: &str = "[注:较早的对话历史已省略]\n\n";
    let mut buf = String::new();
    if let Some(system) = &req.system {
        buf.push_str(truncate_on_boundary(system, MAX_SYSTEM));
        if system.len() > MAX_SYSTEM {
            buf.push_str("\n\n[注:上文规范过长,已截断尾部]");
        }
        buf.push_str("\n\n---\n\n");
    }
    // Single-message requests (the common pipeline case) are emitted bare so
    // existing phase prompts are byte-for-byte unchanged. A multi-turn request
    // is a routed *conversation* — flattening it without speaker labels would
    // leave the host CLI unable to tell the user's turns from its own past
    // replies, so we prefix `User:` / `Assistant:` to preserve attribution.
    let label_roles = req.messages.len() >= 2;
    let mut convo = String::new();
    for (i, msg) in req.messages.iter().enumerate() {
        if i > 0 {
            convo.push_str("\n\n");
        }
        if label_roles {
            convo.push_str(if msg.role.eq_ignore_ascii_case("assistant") {
                "Assistant: "
            } else {
                "User: "
            });
        }
        convo.push_str(&msg.content);
    }
    // Total backstop — never hand the OS an oversized single arg. The LATEST
    // turn is at the END of `convo`, so a front-kept truncation would drop the
    // very question being asked. Instead keep the system head + the TAIL of the
    // conversation (most-recent turns), trimming OLDER history from the front.
    if buf.len() + convo.len() <= MAX_TOTAL {
        buf.push_str(&convo);
        return buf;
    }
    if label_roles {
        // Multi-turn conversation: keep the TAIL so the current question survives.
        let budget = MAX_TOTAL.saturating_sub(buf.len() + TRIM_MARKER.len());
        let start = convo.len().saturating_sub(budget);
        let start = (start..=convo.len())
            .find(|&i| convo.is_char_boundary(i))
            .unwrap_or(convo.len());
        buf.push_str(TRIM_MARKER);
        buf.push_str(&convo[start..]);
        buf
    } else {
        // A single (huge) requirement: the ask is usually up front, so keep the
        // head — matching the long-standing single-message behaviour.
        buf.push_str(&convo);
        truncate_on_boundary(&buf, MAX_TOTAL).to_string()
    }
}

/// Build a driver for the given backend id, or `None` for an unknown id.
///
/// UmaDev drives exactly three host CLIs as first-class bases:
/// `claude-code`, `codex`, and `opencode`.
#[must_use]
pub fn driver_for(backend_id: &str) -> Option<Box<dyn HostDriver>> {
    match backend_id {
        "claude-code" => Some(Box::new(ClaudeCodeDriver::default())),
        "codex" => Some(Box::new(CodexDriver::default())),
        "opencode" => Some(Box::new(OpenCodeDriver::default())),
        _ => None,
    }
}

/// Open a **continuous [`BaseSession`]** for the given backend id — the long-
/// session model the runner's continuous path drives (see
/// `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`). Returns a boxed trait object so
/// the agent crate (which does NOT depend on this crate) can drive any of the
/// three bases through `umadev_runtime::BaseSession` without naming the concrete
/// type.
///
/// - `backend_id` — `claude-code` / `codex` / `opencode` (anything else →
///   `SessionError::Start`, so the caller can fall back to the single-shot path).
/// - `workspace`  — the project root the base operates inside.
/// - `model`      — provider model id; empty falls back to the base's own
///   configured default (UmaDev injects no model endpoint).
/// - `autonomous` — the trust tier's autonomy: `true` lets the base write code
///   unattended (governed by UmaDev's own rules); `false` raises approval
///   requests at gates. Derived by the caller from [`TrustMode`].
/// - `append_system` — UmaDev's composed FIRMWARE (team identity + craft + JIT
///   knowledge + pitfall memory; see `umadev_agent::compose_firmware`) to inject
///   over the base's system-prompt surface. `None` → no firmware (the
///   pre-Wave-2 behaviour). Injection is **per-base, best-effort**:
///   - **claude-code** injects it NATIVELY via `--append-system-prompt` (Claude
///     Code's documented "append custom text to the default system prompt"
///     flag), so the firmware lives in the system prompt for the whole session.
///   - **codex** / **opencode** have no clean generic system-prompt slot on
///     their session-start handshake (codex `thread/start` exposes only
///     `personality` / collaboration-mode templates; opencode's prompt body has
///     no system field — its system override is the `--system` CLI flag /
///     AGENTS.md, not the per-prompt HTTP payload). For those two the firmware
///     reaches the base via the **first-directive prefix** the caller prepends
///     (the universal fail-open path), NOT here — passing `append_system` for
///     them is accepted but currently a no-op at the session layer, so the
///     caller must still front-load the firmware into the first directive. This
///     keeps the contract honest: nothing is silently dropped, and claude gets
///     the stronger native injection.
///
/// [`BaseSession`]: umadev_runtime::BaseSession
/// [`TrustMode`]: umadev_runtime
///
/// # Errors
/// Returns [`umadev_runtime::SessionError`] when the id is unknown or the
/// underlying base process / server fails to start. **Fail-open by contract:**
/// the error is the caller's signal to degrade to the single-shot path, never a
/// panic.
pub async fn session_for(
    backend_id: &str,
    workspace: &std::path::Path,
    model: &str,
    autonomous: bool,
    append_system: Option<&str>,
) -> Result<Box<dyn umadev_runtime::BaseSession>, umadev_runtime::SessionError> {
    // Treat an empty / whitespace-only firmware as absent so an over-eager caller
    // can't inject a blank `--append-system-prompt`.
    let append_system = append_system.filter(|s| !s.trim().is_empty());
    match backend_id {
        "claude-code" => {
            // The continuous claude session tracks the autonomy tier like codex /
            // opencode: `autonomous` → `--permission-mode acceptEdits` (write
            // unattended), otherwise → `default` (claude raises a `can_use_tool`
            // approval per tool → a `NeedApproval` the orchestrator answers, the
            // guarded human-in-the-loop tier). UmaDev's firmware (when present) is
            // injected NATIVELY via `--append-system-prompt`, so it pins the team
            // identity + craft + JIT knowledge/memory for the whole session; the
            // runner's per-phase directives still carry the step-specific framing.
            // `None` → no `--max-turns`: the long-lived main session stays unbounded
            // (today's behavior, fail-open). The optional per-run turn ceiling
            // (`umadev_agent::router::Depth::max_turns`) is a caller-threaded backstop;
            // the read-only critic fork is already capped LOW at the session layer.
            let s = ClaudeSession::start(workspace, append_system, autonomous, None).await?;
            Ok(Box::new(s))
        }
        "codex" => {
            // codex app-server has no generic system-prompt slot on `thread/start`
            // (only `personality` / collaboration-mode templates), so the firmware
            // is NOT injected here — the caller front-loads it onto the first
            // directive instead (the universal fail-open path). Accepting the param
            // keeps the signature uniform across the three bases.
            let s = CodexSession::start(workspace, model, autonomous).await?;
            Ok(Box::new(s))
        }
        "opencode" => {
            // `build` agent; pass the model through only when non-empty so the
            // base falls back to its own configured default otherwise. `autonomous`
            // selects the permission ruleset (wildcard allow vs guarded ask), so
            // opencode's gate posture matches codex / claude. Like codex, opencode's
            // per-prompt HTTP payload has no system field, so the firmware reaches
            // the base via the caller's first-directive prefix, not here.
            let model = (!model.is_empty()).then_some(model);
            let s = OpenCodeSession::start(workspace, Some("build"), model, autonomous).await?;
            Ok(Box::new(s))
        }
        other => Err(umadev_runtime::SessionError::Start(format!(
            "unknown backend id for continuous session: {other}"
        ))),
    }
}

/// Open a base session by **resuming** an existing base conversation
/// (`resume_session_id`) instead of minting a fresh one — the WRITABLE
/// cross-session resume that powers full-context `/continue`. The base
/// re-supplies its OWN persisted transcript, so a build interrupted mid-way picks
/// up with full context (near-zero extra storage: the resume id is a ~36-byte
/// pointer UmaDev persisted at run-open).
///
/// Per base:
/// - **claude-code** → [`ClaudeSession::resume`] (`--resume <id>`, no
///   `--fork-session`): the writable main line of the pinned conversation.
/// - **codex** → [`CodexSession::resume`] (`thread/resume` with a workspace-write
///   sandbox): re-open the thread WRITABLE with its accumulated context.
/// - **opencode** → **not resumable cross-process**: the per-run `opencode serve`
///   child dies when UmaDev closes (and the session is deleted on `end`), so a
///   fresh process has no live server to re-point at. Returns
///   [`SessionError::Start`] so the caller degrades to a fresh session (the
///   roadmap's "server gone → degrade" path).
///
/// # Errors
/// Returns [`umadev_runtime::SessionError`] when the id is unknown, the base
/// rejects the resume, or the backend has no cross-process resume (opencode).
/// **Fail-open by contract:** the error is the caller's signal to degrade to a
/// fresh [`session_for`], never a panic — a resume that errors silently becomes a
/// fresh run.
pub async fn session_for_resume(
    backend_id: &str,
    workspace: &std::path::Path,
    model: &str,
    autonomous: bool,
    append_system: Option<&str>,
    resume_session_id: &str,
) -> Result<Box<dyn umadev_runtime::BaseSession>, umadev_runtime::SessionError> {
    let append_system = append_system.filter(|s| !s.trim().is_empty());
    let resume_id = resume_session_id.trim();
    if resume_id.is_empty() {
        return Err(umadev_runtime::SessionError::Start(
            "no base session id to resume".to_string(),
        ));
    }
    match backend_id {
        "claude-code" => {
            // `None` → unbounded resumed main line (today's behavior); see `session_for`.
            let s = ClaudeSession::resume(workspace, append_system, resume_id, autonomous, None)
                .await?;
            Ok(Box::new(s))
        }
        "codex" => {
            let s = CodexSession::resume(workspace, model, resume_id, autonomous).await?;
            Ok(Box::new(s))
        }
        "opencode" => Err(umadev_runtime::SessionError::Start(
            "opencode has no cross-process session resume (the per-run server is \
             gone); degrade to a fresh session"
                .to_string(),
        )),
        other => Err(umadev_runtime::SessionError::Start(format!(
            "unknown backend id for session resume: {other}"
        ))),
    }
}

/// All backend ids `driver_for` accepts. UmaDev drives exactly three host
/// CLI bases: Claude Code, Codex, and `OpenCode`.
pub const BACKEND_IDS: &[&str] = &["claude-code", "codex", "opencode"];

/// Default per-call timeout for a host CLI invocation (the hard ceiling a
/// single base call can ever take).
///
/// 600s (was 300s): a generation-class call that does deep web research can
/// legitimately run several minutes, and the per-line idle watchdog already
/// kills a *true* mid-stream hang far sooner (300s by default — see
/// `run_subprocess_streaming`). Keeping the hard ceiling strictly ABOVE the
/// idle default is what preserves mid-stream-hang protection: `idle_timeout =
/// min(call.timeout, 300)` would collapse onto the ceiling if they were equal,
/// silently disabling the watchdog. 600s is still a finite backstop for a base
/// that hangs before ever emitting a line; tune per call via
/// `UMADEV_WORKER_TIMEOUT`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Availability of one host backend, as reported by [`probe_all`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BackendStatus {
    /// Stable backend id (`claude-code` / `codex` / `opencode`).
    pub id: &'static str,
    /// Human-facing name.
    pub display_name: &'static str,
    /// The probe result.
    pub probe: ProbeResult,
}

/// Concurrently probe every known backend. The TUI uses this to render
/// its "backends detected" panel — one `--version` check per host, run
/// in parallel so a slow host never serialises startup.
pub async fn probe_all() -> Vec<BackendStatus> {
    // Probe backends in batches of 5 to avoid spawning too many
    // subprocesses at once (each probe runs `<binary> --version`).
    // 21 concurrent spawns can overwhelm the system on some machines.
    let drivers: Vec<Box<dyn HostDriver>> =
        BACKEND_IDS.iter().filter_map(|id| driver_for(id)).collect();

    let mut results = Vec::with_capacity(drivers.len());
    for chunk in drivers.chunks(5) {
        let mut batch = Vec::with_capacity(chunk.len());
        for d in chunk {
            batch.push(async {
                let probe = d.probe().await;
                BackendStatus {
                    id: d.backend_id(),
                    display_name: d.display_name(),
                    probe,
                }
            });
        }
        let batch_results = futures::future::join_all(batch).await;
        results.extend(batch_results);
    }
    results
}

/// Resolve the workspace a driver should run in. Drivers default to the
/// current directory when a caller does not pin one.
#[must_use]
pub(crate) fn default_workspace() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Process-wide lock serialising EVERY env-mutating auth test across the crate's
/// modules (`lib` / `claude` / `codex` / `opencode`). Process env is global to
/// all test threads, so a per-module mutex can't stop a sibling module's test
/// from observing a half-set `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `CODEX_HOME`
/// mid-mutation. A single shared `tokio::sync::Mutex` (the guard is held across
/// `probe_auth().await`, where a `std` guard would trip clippy's
/// `await_holding_lock`) makes those tests strictly serial and flake-proof.
/// Test-only.
#[cfg(test)]
pub(crate) static AUTH_ENV_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use umadev_runtime::{CompletionRequest, Message};

    /// Serializes tests that mutate process-global env vars (`PATH`/`HOME`/…)
    /// so they don't race each other under the multi-threaded test harness.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restore an env var to its captured prior value (or remove it if it was
    /// previously unset). Keeps env-mutating tests hermetic.
    fn restore_env(key: &str, prior: Option<std::ffi::OsString>) {
        match prior {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    struct EnvRestore {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prior }
        }

        fn remove(key: &'static str) -> Self {
            let prior = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prior }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            restore_env(self.key, self.prior.take());
        }
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let painted = "\x1b[1;32mhello\x1b[0m world";
        assert_eq!(strip_ansi(painted), "hello world");
    }

    #[test]
    fn clean_output_trims_and_strips() {
        let raw = "  \x1b[33m# PRD\x1b[0m\n\nbody  \n";
        assert_eq!(clean_output(raw), "# PRD\n\nbody");
    }

    #[tokio::test]
    async fn session_for_accepts_firmware_and_rejects_unknown_backend() {
        // The Wave-2 `append_system` (firmware) param is accepted on the public
        // signature; an unknown backend id still errors DETERMINISTICALLY (no base
        // process spawned), regardless of whether firmware is present, blank, or
        // absent — so the caller's fail-open fallback path is reachable.
        let ws = std::env::temp_dir();
        for fw in [None, Some(""), Some("   "), Some("YOU ARE UmaDev firmware")] {
            let r = session_for("not-a-real-backend", &ws, "", false, fw).await;
            assert!(
                matches!(r, Err(umadev_runtime::SessionError::Start(_))),
                "unknown backend must error deterministically (firmware={fw:?})"
            );
        }
    }

    #[tokio::test]
    async fn session_for_resume_degrades_deterministically() {
        // Cross-session resume must fail DETERMINISTICALLY (no base process spawned)
        // so the caller's fail-open path to a fresh `session_for` is always reachable:
        // an empty resume id, opencode (no cross-process server), and an unknown
        // backend all error without touching a subprocess.
        let ws = std::env::temp_dir();
        // Empty / whitespace id → error before any spawn, for every backend.
        for backend in ["claude-code", "codex", "opencode"] {
            let r = session_for_resume(backend, &ws, "", false, None, "   ").await;
            assert!(
                matches!(r, Err(umadev_runtime::SessionError::Start(_))),
                "empty resume id must degrade ({backend})"
            );
        }
        // opencode has no cross-process resume → always an honest Start error
        // (degrade to fresh), even with a plausible-looking id.
        let r = session_for_resume("opencode", &ws, "", false, None, "ses_abc123").await;
        assert!(matches!(r, Err(umadev_runtime::SessionError::Start(_))));
        // Unknown backend → deterministic error too.
        let r = session_for_resume("not-a-real-backend", &ws, "", false, None, "sid").await;
        assert!(matches!(r, Err(umadev_runtime::SessionError::Start(_))));
    }

    #[test]
    fn model_args_passes_real_ids_skips_placeholders() {
        // Real ids/aliases are passed as `--model <id>`.
        assert_eq!(
            model_args("claude-opus-4-8"),
            vec!["--model".to_string(), "claude-opus-4-8".to_string()]
        );
        assert_eq!(
            model_args("opus"),
            vec!["--model".to_string(), "opus".to_string()]
        );
        // Empty + internal/test/offline placeholders are skipped so a default
        // run never injects a bogus --model.
        for skip in ["", "  ", "m", "stub", "flaky", "offline"] {
            assert!(model_args(skip).is_empty(), "should skip `{skip}`");
        }
    }

    #[test]
    fn merge_prompt_caps_oversized_system_to_avoid_e2big() {
        // A pathologically large system (e.g. huge knowledge/MCP injection) must
        // be trimmed so the merged arg never exceeds the OS single-arg limit,
        // while the user requirement survives intact.
        let req = CompletionRequest {
            model: "m".into(),
            system: Some("X".repeat(200_000)),
            messages: vec![Message {
                role: "user".into(),
                content: "做一个待办应用".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let merged = super::merge_prompt(&req);
        assert!(merged.len() <= 110_000, "merged len {}", merged.len());
        assert!(
            merged.contains("做一个待办应用"),
            "user requirement must survive"
        );
        assert!(merged.contains("已截断"));
    }

    #[test]
    fn merge_prompt_preserves_latest_turn_when_history_is_huge() {
        // A long multi-turn conversation whose history blows past the total cap.
        // The LATEST user turn is at the TAIL — a front-kept truncation would drop
        // the very question being asked, so it must survive while older history is
        // trimmed from the front.
        let mut messages = Vec::new();
        for i in 0..50 {
            messages.push(Message {
                role: "user".into(),
                content: format!("旧问题{i} ").repeat(1000),
            });
            messages.push(Message {
                role: "assistant".into(),
                content: format!("旧回答{i} ").repeat(1000),
            });
        }
        messages.push(Message {
            role: "user".into(),
            content: "最新的关键问题TAILMARKER".into(),
        });
        let req = CompletionRequest {
            model: "m".into(),
            system: Some("规范".into()),
            messages,
            max_tokens: None,
            temperature: None,
        };
        let merged = super::merge_prompt(&req);
        assert!(merged.len() <= 110_000, "merged len {}", merged.len());
        assert!(
            merged.contains("最新的关键问题TAILMARKER"),
            "the latest turn must survive truncation"
        );
        assert!(merged.contains("已省略"), "older history is marked trimmed");
    }

    #[test]
    fn merge_prompt_joins_system_and_user() {
        let req = CompletionRequest {
            model: "m".into(),
            system: Some("SYSTEM".into()),
            messages: vec![Message {
                role: "user".into(),
                content: "USER".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let merged = merge_prompt(&req);
        assert!(merged.starts_with("SYSTEM"));
        assert!(merged.contains("---"));
        assert!(merged.ends_with("USER"));
    }

    #[test]
    fn merge_prompt_without_system() {
        let req = CompletionRequest {
            model: "m".into(),
            system: None,
            messages: vec![Message {
                role: "user".into(),
                content: "just user".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        assert_eq!(merge_prompt(&req), "just user");
    }

    #[test]
    fn merge_prompt_labels_roles_for_multi_turn() {
        // A routed conversation (≥2 messages) must keep speaker attribution so
        // the host CLI can answer the last turn with the earlier turns in view.
        let req = CompletionRequest {
            model: "m".into(),
            system: None,
            messages: vec![
                Message {
                    role: "user".into(),
                    content: "你好".into(),
                },
                Message {
                    role: "assistant".into(),
                    content: "你好,我是底座".into(),
                },
                Message {
                    role: "user".into(),
                    content: "我刚才说了什么?".into(),
                },
            ],
            max_tokens: None,
            temperature: None,
        };
        let merged = merge_prompt(&req);
        assert!(merged.contains("User: 你好"));
        assert!(merged.contains("Assistant: 你好,我是底座"));
        assert!(merged.ends_with("User: 我刚才说了什么?"));
    }

    #[test]
    fn map_subprocess_error_classifies_streaming_idle_timeout() {
        let err = "`claude` idle timeout: no stdout for 7s (stream-json hang? lines so far: 1). Set UMADEV_IDLE_TIMEOUT_SECS to adjust.".to_string();
        match map_subprocess_error(err) {
            umadev_runtime::RuntimeError::Timeout(secs, msg) => {
                assert_eq!(secs, 7);
                assert!(msg.contains("idle timeout"));
            }
            other => panic!("idle timeout must map to RuntimeError::Timeout, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_capture_cap_is_a_hard_limit() {
        // The contract says 256 KiB; the old implementation appended whole
        // chunks and could exceed the cap by up to 8191 bytes.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(
                "i=0; while [ $i -lt 4000 ]; do \
                 printf '0123456789%.0s' 1 2 3 4 5 6 7 8 9 10 1>&2; \
                 i=$((i+1)); \
                 done",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let task = spawn_stderr_capture(child.stderr.take());
        let _ = child.wait().await.unwrap();
        let buf = reap_bounded(task).await.unwrap();
        assert_eq!(
            buf.len(),
            STDERR_CAPTURE_CAP,
            "stderr capture must not grow past the advertised hard cap"
        );
    }

    #[test]
    fn driver_for_known_and_unknown() {
        for id in BACKEND_IDS {
            assert!(
                driver_for(id).is_some(),
                "BACKEND_IDS contains `{id}` but driver_for can't build it"
            );
        }
        assert!(driver_for("nope").is_none());
        assert!(driver_for("").is_none());
    }

    #[test]
    fn boxed_host_driver_forwards_fork() {
        // The run path boxes each driver as `Box<dyn HostDriver>`; its Runtime
        // impl MUST forward fork() so the pipeline's parallel docs fan-out can
        // trigger. Regression: the forward was missing, so fork() returned the
        // trait-default None and parallel silently fell back to sequential.
        for id in BACKEND_IDS {
            let Some(d) = driver_for(id) else {
                panic!("driver_for({id}) is None");
            };
            assert!(
                umadev_runtime::Runtime::fork(&d).is_some(),
                "Box<dyn HostDriver> for `{id}` must forward fork()"
            );
        }
    }

    #[test]
    fn backend_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for id in BACKEND_IDS {
            assert!(seen.insert(*id), "duplicate id in BACKEND_IDS: {id}");
        }
    }

    #[test]
    fn backend_count_matches_driver_for() {
        assert_eq!(BACKEND_IDS.len(), 3);
    }

    #[test]
    fn backend_ids_match_driver_for() {
        for id in BACKEND_IDS {
            assert!(
                driver_for(id).is_some(),
                "BACKEND_IDS has unbuildable id {id}"
            );
        }
    }

    #[tokio::test]
    async fn probe_all_reports_every_backend() {
        let statuses = probe_all().await;
        assert_eq!(statuses.len(), BACKEND_IDS.len());
        // Every BACKEND_IDS entry is represented exactly once.
        for id in BACKEND_IDS {
            assert_eq!(
                statuses.iter().filter(|s| s.id == *id).count(),
                1,
                "probe_all missing backend {id}"
            );
        }
        // Each status carries a non-empty display name.
        assert!(statuses.iter().all(|s| !s.display_name.is_empty()));
    }

    #[tokio::test]
    async fn run_subprocess_captures_stdout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = run_subprocess(SubprocessCall {
            program: "echo",
            args: &[],
            prompt: "hello-from-test",
            channel: PromptChannel::Arg,
            workspace: tmp.path(),
            timeout: Duration::from_secs(5),
            env: &[],
        })
        .await
        .unwrap();
        assert_eq!(out.stdout, "hello-from-test");
    }

    // ---- AuthState / ProbeResult honest-auth surface (gap G10) ----

    #[test]
    fn auth_state_three_states_map_to_probe_result() {
        // A Ready probe carries its auth_state through faithfully — the honest
        // third axis the picker reads (installed AND logged in vs installed but
        // not logged in vs indeterminate).
        let logged_in = ProbeResult::Ready {
            version: "1.2.3".to_string(),
            auth_state: AuthState::LoggedIn,
        };
        assert_eq!(logged_in.auth_state(), AuthState::LoggedIn);
        assert!(logged_in.is_ready());
        assert!(logged_in.is_ready_and_authed());
        assert!(logged_in.auth_state().is_logged_in());

        let not_logged_in = ProbeResult::Ready {
            version: "1.2.3".to_string(),
            auth_state: AuthState::NotLoggedIn,
        };
        assert_eq!(not_logged_in.auth_state(), AuthState::NotLoggedIn);
        // CRITICAL G10 fix: on PATH (is_ready) but NOT authed — must NOT show the
        // green "ready & logged in" mark.
        assert!(not_logged_in.is_ready());
        assert!(!not_logged_in.is_ready_and_authed());

        // Indeterminate auth never masquerades as logged in.
        let unknown = ProbeResult::Ready {
            version: "1.2.3".to_string(),
            auth_state: AuthState::Unknown,
        };
        assert_eq!(unknown.auth_state(), AuthState::Unknown);
        assert!(!unknown.is_ready_and_authed());
    }

    #[test]
    fn probe_result_not_installed_vs_not_logged_in_are_distinct() {
        // NotInstalled (binary absent → install hint) and NotLoggedIn (binary
        // present, no creds → login hint) are different picker states.
        let absent = ProbeResult::NotInstalled {
            program: "codex".to_string(),
        };
        assert_eq!(absent.auth_state(), AuthState::NotInstalled);
        assert!(!absent.is_ready());
        assert!(!absent.is_ready_and_authed());

        let present_no_creds = ProbeResult::Ready {
            version: "0.1.0".to_string(),
            auth_state: AuthState::NotLoggedIn,
        };
        assert_eq!(present_no_creds.auth_state(), AuthState::NotLoggedIn);
        assert_ne!(absent.auth_state(), present_no_creds.auth_state());
    }

    #[test]
    fn unhealthy_probe_is_unknown_not_a_false_logged_in() {
        // A broken binary (version failed for a non-PATH reason) tells us nothing
        // about login → Unknown, never a false LoggedIn (fail-open).
        let broken = ProbeResult::Unhealthy {
            detail: "exited with code 1".to_string(),
        };
        assert_eq!(broken.auth_state(), AuthState::Unknown);
        assert!(!broken.is_ready_and_authed());
    }

    #[tokio::test]
    async fn install_and_login_hints_present_for_every_backend() {
        // The picker needs an install command (NotInstalled) and a login command
        // (NotLoggedIn) string for each first-class base.
        for id in BACKEND_IDS {
            let d = driver_for(id).unwrap();
            assert!(
                d.install_hint().is_some_and(|h| !h.is_empty()),
                "{id} must expose an install hint for the picker"
            );
            assert!(
                d.login_hint().is_some_and(|h| !h.is_empty()),
                "{id} must expose a login hint for the picker"
            );
        }
    }

    #[tokio::test]
    async fn run_auth_status_fail_opens_to_none_on_missing_binary() {
        // The cheap auth-status no-op must fail-open (→ None → caller resolves
        // Unknown) when the status command can't even spawn — never a panic,
        // never a false positive.
        let got = run_auth_status(
            "umadev-definitely-not-a-real-binary-xyz",
            &["status".to_string()],
            true,
        )
        .await;
        assert!(
            got.is_none(),
            "a missing status binary must fail-open to None"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_auth_status_times_out_to_none_fail_open() {
        // A status command that hangs must be killed by the short auth-probe
        // ceiling and fail-open to None (→ Unknown), never block the picker or
        // wrongly report logged-in.
        let _env = IDLE_ENV_LOCK.lock().await;
        let _restore = EnvRestore::set("UMADEV_AUTH_PROBE_SECS", "1");
        let started = Instant::now();
        let got = run_auth_status(
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            // success not required, but a hang still must not return a value
            false,
        )
        .await;
        assert!(
            got.is_none(),
            "a hung auth-status command must fail-open to None"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "auth probe must honor the short ceiling, not hang the picker"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_auth_status_returns_stdout_on_success() {
        // A status command that prints its state and exits 0 → we get the text to
        // pattern-match against (the "logged in" wording).
        let got = run_auth_status(
            "sh",
            &[
                "-c".to_string(),
                "echo 'Logged in using ChatGPT'".to_string(),
            ],
            true,
        )
        .await
        .expect("a clean status command must return its stdout");
        assert!(got.contains("Logged in"));
    }

    #[test]
    fn govern_root_env_carries_the_workspace_under_the_marker() {
        let env = govern_root_env(std::path::Path::new("/projects/app"));
        assert_eq!(
            env,
            vec![(GOVERN_ROOT_ENV.to_string(), "/projects/app".to_string())]
        );
    }

    // `printenv` exists on macOS/Linux; the env propagation it proves is the
    // same on Windows (`Command::env`), exercised via `govern_root_env` above.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_subprocess_propagates_govern_root_env_to_child() {
        let tmp = tempfile::TempDir::new().unwrap();
        let env = govern_root_env(tmp.path());
        let out = run_subprocess(SubprocessCall {
            program: "printenv",
            args: &[GOVERN_ROOT_ENV.to_string()],
            prompt: "",
            channel: PromptChannel::Arg,
            workspace: tmp.path(),
            timeout: Duration::from_secs(5),
            env: &env,
        })
        .await
        .unwrap();
        // The base subprocess sees UMADEV_GOVERN_ROOT = the workspace, so the
        // PreToolUse hook it spawns will govern (scoped to this root).
        assert_eq!(out.stdout.trim(), tmp.path().to_string_lossy());
    }

    #[tokio::test]
    async fn run_subprocess_reports_missing_program() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = run_subprocess(SubprocessCall {
            program: "umadev-definitely-not-a-real-binary",
            args: &[],
            prompt: "x",
            channel: PromptChannel::Arg,
            workspace: tmp.path(),
            timeout: Duration::from_secs(5),
            env: &[],
        })
        .await
        .unwrap_err();
        assert!(err.contains("not found on PATH"));
    }

    #[tokio::test]
    async fn run_subprocess_feeds_stdin() {
        let tmp = tempfile::TempDir::new().unwrap();
        // `cat` echoes stdin back to stdout.
        let out = run_subprocess(SubprocessCall {
            program: "cat",
            args: &[],
            prompt: "piped-prompt-body",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(5),
            env: &[],
        })
        .await
        .unwrap();
        assert_eq!(out.stdout, "piped-prompt-body");
    }

    #[test]
    fn command_line_budget_default_and_override() {
        let platform_default = if cfg!(windows) { 7_000 } else { 120_000 };
        // Off Windows the budget is a high backstop under Linux's 128 KiB per-arg
        // cap; merged prompts (capped 110_000) never trip it → argv fast path kept.
        assert_eq!(command_line_budget_from(None), platform_default);
        // A positive override wins (an escape hatch for a tighter machine cap).
        assert_eq!(command_line_budget_from(Some("1234")), 1234);
        // Junk / zero fall back to the platform default (never a 0 budget that would
        // divert every prompt) — fail-open.
        assert_eq!(command_line_budget_from(Some("0")), platform_default);
        assert_eq!(command_line_budget_from(Some("nope")), platform_default);
    }

    #[test]
    fn command_line_len_accounts_for_wrapper_and_crosses_windows_budget() {
        // A short, realistic `claude` session command line fits the conservative
        // Windows `cmd.exe` budget (7000) with room to spare — the fast argv path.
        let sid = "00000000-0000-4000-8000-000000000000";
        let small = command_line_len([
            "claude",
            "--print",
            "--input-format",
            "stream-json",
            "--session-id",
            sid,
            "--append-system-prompt",
            "be terse",
        ]);
        assert!(
            small < 7_000,
            "small line unexpectedly over budget: {small}"
        );
        // A multi-KB firmware pushes the WHOLE line (wrapper overhead included) past
        // the Windows budget, so it MUST be diverted off the command line. This is the
        // threshold computed for the `cmd /c <resolved .cmd path>` wrapped case.
        let firmware = "x".repeat(8_000);
        let big = command_line_len(["claude", "--append-system-prompt", firmware.as_str()]);
        assert!(
            big > 7_000,
            "8 KB firmware must exceed the Windows budget: {big}"
        );
    }

    #[tokio::test]
    async fn oversized_arg_prompt_is_delivered_via_stdin_not_truncated() {
        // A prompt that would overflow the command line is routed through stdin (both
        // `claude --print` and `opencode run` read the prompt from stdin), so nothing
        // is truncated. The `sh` probe reports WHERE the prompt landed: `$1` (argv) vs
        // stdin (`cat`). Over the non-Windows 120_000 budget requires a >120 KB prompt.
        let tmp = tempfile::TempDir::new().unwrap();
        let big = "x".repeat(121_000);
        let out = run_subprocess(SubprocessCall {
            program: "sh",
            args: &[
                "-c".to_string(),
                "printf 'ARG<%s>' \"$1\"; printf 'IN<'; cat; printf '>'".to_string(),
                "probe".to_string(),
            ],
            prompt: &big,
            channel: PromptChannel::Arg,
            workspace: tmp.path(),
            timeout: Duration::from_secs(15),
            env: &[],
        })
        .await
        .unwrap();
        // Diverted: `$1` is empty (no positional prompt) and the FULL prompt arrived on
        // stdin — proving it was NOT truncated onto the command line.
        assert!(
            out.stdout.starts_with("ARG<>IN<"),
            "prompt should have left argv"
        );
        assert!(out.stdout.ends_with('>'));
        assert_eq!(out.stdout.len(), "ARG<>IN<>".len() + big.len());
        assert!(out.stdout.contains(&big));
    }

    #[tokio::test]
    async fn small_arg_prompt_keeps_the_argv_fast_path() {
        // A small prompt stays a positional arg (no stdin diversion, no regression),
        // so `$1` carries it and stdin is empty. Mac/Linux behavior is unchanged.
        let tmp = tempfile::TempDir::new().unwrap();
        let out = run_subprocess(SubprocessCall {
            program: "sh",
            args: &[
                "-c".to_string(),
                "printf 'ARG<%s>' \"$1\"; printf 'IN<'; cat; printf '>'".to_string(),
                "probe".to_string(),
            ],
            prompt: "hello",
            channel: PromptChannel::Arg,
            workspace: tmp.path(),
            timeout: Duration::from_secs(5),
            env: &[],
        })
        .await
        .unwrap();
        assert_eq!(out.stdout, "ARG<hello>IN<>");
    }

    #[tokio::test]
    async fn run_subprocess_reports_nonzero_exit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = run_subprocess(SubprocessCall {
            program: "sh",
            args: &["-c".into(), "echo boom >&2; exit 3".into()],
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(5),
            env: &[],
        })
        .await
        .unwrap_err();
        assert!(err.contains("code 3"));
        assert!(err.contains("boom"));
    }

    // ---- resolve_program: bullet-proof base detection ----

    #[test]
    fn resolve_program_returns_input_when_nothing_found() {
        // Fail-open: a name that exists nowhere on PATH or in any known dir
        // comes back unchanged, so the spawn surfaces the real "not found".
        let got = resolve_program("umadev-definitely-not-installed-anywhere-xyz");
        assert_eq!(got, "umadev-definitely-not-installed-anywhere-xyz");
    }

    #[test]
    fn resolve_program_passes_through_explicit_paths() {
        // An explicit path (has a separator) is taken as-is — never rewritten.
        let sep = std::path::MAIN_SEPARATOR;
        let p = format!("some{sep}dir{sep}codex");
        assert_eq!(resolve_program(&p), p);
    }

    #[test]
    fn match_in_dir_skips_empty_and_missing() {
        // Fail-open building blocks: an empty dir name or a non-existent dir
        // yields no hit rather than erroring.
        let exts = path_extensions();
        assert!(match_in_dir(std::path::Path::new(""), "codex", &exts).is_none());
        assert!(
            match_in_dir(
                std::path::Path::new("/umadev/no/such/dir/at/all"),
                "codex",
                &exts
            )
            .is_none(),
            "a missing dir must fail-open to None"
        );
    }

    // On Unix the known-install scan locates a binary that is NOT on PATH but
    // lives in a per-base standalone dir (`~/.codex/bin`). We point HOME at a
    // temp dir and clear PATH so only the known-dir branch can match.
    #[cfg(unix)]
    #[test]
    fn resolve_program_finds_base_in_home_standalone_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = tmp.path().join(".mybase/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("mybase");
        std::fs::write(&exe, "#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        // `mybase` is a unique name absent from the real PATH, so we only need
        // to redirect HOME — the PATH lookup misses and the known-dir branch
        // under our temp HOME is exercised. Not clobbering PATH keeps concurrent
        // spawn-based tests (echo/cat/sh) safe.
        let _guard = ENV_LOCK.lock().unwrap();
        let _home = EnvRestore::set("HOME", tmp.path());
        let got = resolve_program("mybase");

        assert_eq!(
            got,
            exe.to_string_lossy(),
            "a base in ~/.mybase/bin must resolve even when absent from PATH"
        );
    }

    // On Unix, PATH is authoritative and wins over a same-named known dir entry.
    #[cfg(unix)]
    #[test]
    fn resolve_program_prefers_path_over_known_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let on_path = tempfile::TempDir::new().unwrap();
        let exe = on_path.path().join("dualbase");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let home = tempfile::TempDir::new().unwrap();
        let known = home.path().join(".dualbase/bin");
        std::fs::create_dir_all(&known).unwrap();
        std::fs::write(known.join("dualbase"), "#!/bin/sh\n").unwrap();

        let _guard = ENV_LOCK.lock().unwrap();
        let _home = EnvRestore::set("HOME", home.path());
        let _path = EnvRestore::set("PATH", on_path.path());
        let got = resolve_program("dualbase");

        assert_eq!(
            got,
            exe.to_string_lossy(),
            "a PATH hit must win over the known-install-dir fallback"
        );
    }

    // On Windows, when both `codex` (bare *nix shim) and `codex.cmd` exist in
    // the same dir, the `.cmd` must win — the bare shim is not a PE (os 193).
    #[cfg(windows)]
    #[test]
    fn resolve_program_prefers_cmd_over_bare_on_windows() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("winbase"), b"#!/bin/sh\n").unwrap();
        std::fs::write(tmp.path().join("winbase.cmd"), b"@echo off\n").unwrap();

        let _guard = ENV_LOCK.lock().unwrap();
        let _path = EnvRestore::set("PATH", tmp.path());
        let _pathext = EnvRestore::set("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        let got = resolve_program("winbase");

        assert!(
            got.to_ascii_lowercase().ends_with("winbase.cmd"),
            "the .cmd shim must win over the bare *nix shim, got: {got}"
        );
    }

    // On Windows, a base installed only under `%LOCALAPPDATA%\Programs\{prog}`
    // (a standalone installer) resolves even when absent from PATH.
    #[cfg(windows)]
    #[test]
    fn resolve_program_finds_base_in_localappdata_programs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let progdir = tmp.path().join("Programs").join("winstandalone");
        std::fs::create_dir_all(&progdir).unwrap();
        std::fs::write(progdir.join("winstandalone.exe"), b"MZ").unwrap();

        let _guard = ENV_LOCK.lock().unwrap();
        let _local = EnvRestore::set("LOCALAPPDATA", tmp.path());
        let _path = EnvRestore::set("PATH", ""); // force the known-dir branch
        let got = resolve_program("winstandalone");

        assert!(
            got.to_ascii_lowercase().ends_with("winstandalone.exe"),
            "a base under %LOCALAPPDATA%\\Programs must resolve off-PATH, got: {got}"
        );
    }

    #[tokio::test]
    async fn run_subprocess_times_out_when_child_writes_then_hangs() {
        // Regression: a child that emits output and THEN hangs while keeping
        // its stdout pipe open must still hit the per-call timeout. Before the
        // fix, the unbounded `read_to_end` blocked forever and the timeout was
        // dead code. This must return a timeout error in ~1s, not hang.
        let tmp = tempfile::TempDir::new().unwrap();
        let started = Instant::now();
        let err = run_subprocess(SubprocessCall {
            program: "sh",
            args: &["-c".into(), "echo partial; sleep 30".into()],
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(1),
            env: &[],
        })
        .await
        .unwrap_err();
        assert!(err.contains("timed out"), "expected timeout, got: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout did not fire promptly — read_to_end blocked the deadline"
        );
    }

    // A NON-streaming call that emits a byte and THEN goes silent must be killed
    // by the idle watchdog (~idle_timeout after the first byte), NOT made to wait
    // out the full hard ceiling. The error is distinguishable (`stdout silence`)
    // yet still contains `timed out` so it classifies as a retriable Timeout —
    // preserving the prior write-then-hang retry behaviour.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_subprocess_idle_kills_after_first_byte_then_silence() {
        let _env = IDLE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let _restore = EnvRestore::set("UMADEV_IDLE_TIMEOUT_SECS", "1");
        let started = Instant::now();
        let err = run_subprocess(SubprocessCall {
            program: "sh",
            args: &["-c".into(), "printf 'partial'; sleep 30".into()],
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            // Ceiling far above the 1s idle so we KNOW the idle watchdog (not the
            // hard ceiling) did the kill.
            timeout: Duration::from_secs(30),
            env: &[],
        })
        .await
        .unwrap_err();
        assert!(
            err.contains("stdout silence"),
            "expected the idle-silence error, got: {err}"
        );
        assert!(
            err.contains("timed out"),
            "idle error must still classify as a retriable timeout, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "idle watchdog should fire ~1s after the first byte, not wait the 30s ceiling"
        );
    }

    // First-byte grace (non-streaming): a long silence BEFORE the first byte must
    // NOT trip the idle watchdog, as long as it stays under the hard ceiling — a
    // slow first token (big prompt / slow model) is healthy, not a hang.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_subprocess_first_byte_grace_survives_slow_first_output() {
        let _env = IDLE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let _restore = EnvRestore::set("UMADEV_IDLE_TIMEOUT_SECS", "1");
        let started = Instant::now();
        let out = run_subprocess(SubprocessCall {
            program: "sh",
            args: &["-c".into(), "sleep 2; printf 'the answer\\n'".into()],
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(20),
            env: &[],
        })
        .await;
        let out = out.expect("a slow first byte under the ceiling must not be idle-killed");
        assert!(out.stdout.contains("the answer"));
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "the test should have waited out the slow first byte"
        );
    }

    // A truly silent hang (never writes a byte) is bounded only by the hard
    // ceiling and reported as the overall timeout — NOT the idle-silence error
    // (no byte was ever seen, so the idle watchdog never armed).
    #[cfg(unix)]
    #[tokio::test]
    async fn run_subprocess_hard_ceiling_kills_truly_silent_hang() {
        let tmp = tempfile::TempDir::new().unwrap();
        let started = Instant::now();
        let err = run_subprocess(SubprocessCall {
            program: "sh",
            args: &["-c".into(), "sleep 30".into()], // never writes a byte
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(1),
            env: &[],
        })
        .await
        .unwrap_err();
        assert!(
            err.contains("timed out after 1s"),
            "a truly silent hang trips the hard ceiling, got: {err}"
        );
        assert!(
            !err.contains("stdout silence"),
            "no byte was ever seen, so it is the hard ceiling, not idle, got: {err}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    // Normal fast completion is unaffected by the idle watchdog.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_subprocess_fast_completion_unaffected_by_idle_watchdog() {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = run_subprocess(SubprocessCall {
            program: "sh",
            args: &["-c".into(), "printf 'hello world\\n'".into()],
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(10),
            env: &[],
        })
        .await
        .expect("fast completion must not be affected by the idle watchdog");
        assert!(out.stdout.contains("hello world"));
    }

    // H1: the child exits 0, but a GRANDCHILD it backgrounded inherited the
    // stderr write fd and holds it open. The old final `stderr_task.await` was an
    // unbounded `read_to_end` that never EOFs in that case → `complete()` hung
    // forever. The bounded flush-grace reap must return the call (with the stdout
    // we did get) within ~`STDERR_FLUSH_GRACE`, NOT wait out the grandchild.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_subprocess_returns_when_grandchild_holds_stderr_open() {
        let tmp = tempfile::TempDir::new().unwrap();
        let started = Instant::now();
        // `sleep 30 >/dev/null &` keeps the grandchild's stdout off the pipe (so
        // stdout EOFs and `child.wait` returns) but leaves it holding the inherited
        // STDERR write end for 30s; the parent prints to stdout then exits 0. The
        // bounded reap returns in ~`STDERR_FLUSH_GRACE`; the OLD unbounded
        // `read_to_end` would block ~30s — a wide, load-proof separation.
        let out = run_subprocess(SubprocessCall {
            program: "sh",
            args: &[
                "-c".into(),
                "echo hello; sleep 30 >/dev/null & exit 0".into(),
            ],
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(60),
            env: &[],
        })
        .await
        .expect("a clean exit must return even while a grandchild holds stderr open");
        assert!(out.stdout.contains("hello"));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the post-exit stderr drain must be bounded by the flush grace, not the \
             grandchild's 30s hold"
        );
    }

    // M4: a Stdin-channel base that emits >64 KiB on stdout BEFORE it drains all of
    // a >64 KiB prompt would DEADLOCK if the prompt is written in full before
    // stdout draining begins (both pipes wedge at the ~64 KiB buffer and the
    // ceiling is never entered — `write_all` has no timeout). Writing the prompt
    // CONCURRENTLY with draining clears both. The outer timeout turns a regression
    // into a clean FAIL instead of an infinite hang.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_subprocess_large_prompt_does_not_deadlock_when_base_floods_stdout_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Flood ~100 KiB to stdout, THEN drain stdin, THEN print DONE.
        let body = "i=0; while [ $i -lt 100 ]; do printf '%01000d\\n' 0; i=$((i+1)); \
                    done; cat >/dev/null; printf 'DONE\\n'";
        let prompt = "x".repeat(100_000); // > the ~64 KiB stdin pipe buffer
        let args = ["-c".to_string(), body.to_string()];
        let call = run_subprocess(SubprocessCall {
            program: "sh",
            args: &args,
            prompt: &prompt,
            channel: PromptChannel::Stdin,
            workspace: tmp.path(),
            timeout: Duration::from_secs(30),
            env: &[],
        });
        let out = tokio::time::timeout(Duration::from_secs(15), call)
            .await
            .expect("must not deadlock: the prompt write has to run concurrently with draining")
            .expect("the subprocess itself should succeed");
        assert!(
            out.stdout.contains("DONE"),
            "the base consumed the whole prompt and finished"
        );
    }

    // ---- run_subprocess_streaming: first-line grace watchdog ----

    /// Async-safe lock serialising the streaming tests that mutate the
    /// process-global `UMADEV_IDLE_TIMEOUT_SECS`. `ENV_LOCK` is a
    /// `std::sync::Mutex` whose guard can't be held across an `.await`; these
    /// tests `.await` the subprocess while the env must stay set, so they need a
    /// `tokio::sync::Mutex` instead. Only the tests that set this var take this
    /// lock, so serialising them is sufficient. Gated to `unix` like its only
    /// users, so it isn't a dead `static` on Windows (`-D warnings` rejects that).
    #[cfg(unix)]
    static IDLE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Write an executable `#!/bin/sh` fake into `dir` and return its path. The
    /// streaming watchdog tests drive this instead of a real base; gated unix-only
    /// because Windows cannot exec a shell-shebang script (same constraint as the
    /// per-driver streaming tests).
    #[cfg(unix)]
    fn write_sh_fake(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join(name);
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    // A slow FIRST token (long silence BEFORE any stdout line) must NOT trip the
    // idle watchdog, as long as it stays under the hard `call.timeout`. This is
    // the opencode regression: its first stdout line IS the answer, so a slow
    // model would otherwise be idle-killed before producing anything and forced to
    // re-run the whole generation. The fake sleeps past `idle_timeout` (1s here)
    // with zero output, THEN prints the answer — the call must succeed.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_first_line_grace_survives_slow_first_token() {
        let _env = IDLE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        // Sleep 2s (> the 1s idle timeout) with no output, then emit the answer.
        let script = write_sh_fake(
            tmp.path(),
            "slow-first",
            "#!/bin/sh\ncat >/dev/null 2>&1\nsleep 2\nprintf 'the slow answer\\n'\n",
        );
        let _restore = EnvRestore::set("UMADEV_IDLE_TIMEOUT_SECS", "1");
        let started = Instant::now();
        let out = run_subprocess_streaming(
            SubprocessCall {
                program: script.to_str().unwrap(),
                args: &[],
                prompt: "",
                channel: PromptChannel::Stdin,
                // Hard ceiling comfortably above the 2s first-token latency.
                timeout: Duration::from_secs(20),
                workspace: tmp.path(),
                env: &[],
            },
            &|_line: &str| {},
        )
        .await;
        let out = out.expect("a slow first token under call.timeout must not be idle-killed");
        assert!(out.stdout.contains("the slow answer"));
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "the test should have waited out the slow first token"
        );
    }

    // Once the stream is LIVE (first line seen), a mid-stream silence longer than
    // `idle_timeout` MUST trip the idle watchdog — the first-line grace only
    // relaxes the *pre*-first-line wait, never the line-to-line one. The fake
    // prints a first line immediately, then hangs; we expect the distinguishable
    // "idle timeout" error well before the (large) hard ceiling.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_idle_kill_after_first_line_on_midstream_silence() {
        let _env = IDLE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_sh_fake(
            tmp.path(),
            "first-then-hang",
            "#!/bin/sh\ncat >/dev/null 2>&1\nprintf 'first line\\n'\nsleep 30\n",
        );
        let _restore = EnvRestore::set("UMADEV_IDLE_TIMEOUT_SECS", "1");
        let started = Instant::now();
        let err = run_subprocess_streaming(
            SubprocessCall {
                program: script.to_str().unwrap(),
                args: &[],
                prompt: "",
                channel: PromptChannel::Stdin,
                // Hard ceiling far above the 1s idle timeout so we KNOW the idle
                // watchdog — not the ceiling — is what fired.
                timeout: Duration::from_secs(30),
                workspace: tmp.path(),
                env: &[],
            },
            &|_line: &str| {},
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("idle timeout"),
            "mid-stream silence after the first line must trip the idle watchdog, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "idle watchdog should fire ~1s after the first line, not wait for the ceiling"
        );
    }

    // A true hang — NO output at all, runs past the hard `call.timeout` — must
    // still be killed by the overall ceiling. The first-line grace removes the
    // pre-first-line *idle* sub-timeout but can never bypass `call.timeout`; this
    // is the real-hang backstop. The fake produces nothing and sleeps forever; we
    // expect the overall-timeout error in ~1s.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_hard_ceiling_kills_silent_hang_before_first_line() {
        let _env = IDLE_ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_sh_fake(
            tmp.path(),
            "silent-hang",
            "#!/bin/sh\ncat >/dev/null 2>&1\nsleep 30\n",
        );
        // Idle timeout huge so it can't be what fires — only the 1s hard ceiling
        // can stop a never-emitting child before the first line.
        let _restore = EnvRestore::set("UMADEV_IDLE_TIMEOUT_SECS", "600");
        let started = Instant::now();
        let err = run_subprocess_streaming(
            SubprocessCall {
                program: script.to_str().unwrap(),
                args: &[],
                prompt: "",
                channel: PromptChannel::Stdin,
                timeout: Duration::from_secs(1),
                workspace: tmp.path(),
                env: &[],
            },
            &|_line: &str| {},
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("timed out"),
            "a silent child past call.timeout must hit the hard ceiling, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the hard ceiling must fire promptly even before the first line"
        );
    }

    // The hard per-call ceiling defaults to 600s and stays STRICTLY above the
    // 300s idle default — that ordering is load-bearing: `idle_timeout =
    // min(call.timeout, 300)`, so if they were equal the idle watchdog would
    // collapse onto the ceiling and silently stop catching mid-stream hangs.
    #[test]
    fn default_timeout_is_600s_and_above_idle_default() {
        assert_eq!(
            DEFAULT_TIMEOUT,
            Duration::from_secs(600),
            "hard per-call ceiling default changed unexpectedly"
        );
        assert!(
            DEFAULT_TIMEOUT > Duration::from_secs(300),
            "the hard ceiling must stay above the 300s idle default, or the idle \
             watchdog (min(call.timeout, idle)) collapses onto the ceiling"
        );
    }

    // With NO `UMADEV_IDLE_TIMEOUT_SECS` override, the idle watchdog default is
    // now 300s (was 120s). A live stream that goes silent for ~2s after its
    // first line — a perfectly normal long web-research / "thinking" pause —
    // must NOT be idle-killed under the default, because 2s is far below 300s.
    // (The 120s→300s bump is precisely so a >2min healthy research silence is no
    // longer mis-killed; we assert the shape with a fast 2s proxy.) The
    // companion `streaming_idle_kill_after_first_line_on_midstream_silence` test
    // already proves the watchdog still FIRES when the override makes it tiny, so
    // together they pin both the new default and that the mechanism still works.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_idle_default_300s_tolerates_short_midstream_silence() {
        let _env = IDLE_ENV_LOCK.lock().await;
        // Make sure no leftover override is in scope — we want the DEFAULT.
        let _restore = EnvRestore::remove("UMADEV_IDLE_TIMEOUT_SECS");
        let tmp = tempfile::TempDir::new().unwrap();
        // First line immediately, ~2s mid-stream silence, then a second line and
        // exit. Under the OLD 120s default this also passed, but under a 1s
        // default (the kind of value that mis-killed research) it would not — the
        // point is that the unset default is comfortably large.
        let script = write_sh_fake(
            tmp.path(),
            "first-pause-second",
            "#!/bin/sh\ncat >/dev/null 2>&1\nprintf 'first line\\n'\nsleep 2\nprintf 'second line\\n'\n",
        );
        let out = run_subprocess_streaming(
            SubprocessCall {
                program: script.to_str().unwrap(),
                args: &[],
                prompt: "",
                channel: PromptChannel::Stdin,
                // Hard ceiling well above the 2s pause so ONLY the idle default
                // could (wrongly) fire — and with a 300s default it must not.
                timeout: Duration::from_secs(30),
                workspace: tmp.path(),
                env: &[],
            },
            &|_line: &str| {},
        )
        .await
        .expect("a short mid-stream silence must not trip the 300s idle default");
        assert!(out.stdout.contains("first line"));
        assert!(
            out.stdout.contains("second line"),
            "the post-pause line must survive — the idle default did not kill it"
        );
    }

    // H2: the streaming path's exit wait used to be an UNBOUNDED `child.wait()`. A
    // base that closes stdout (so the read loop EOFs) but then LINGERS in teardown
    // would hang the call past `call.timeout` forever. The bounded wait must kill +
    // return a `timed out` error at the ceiling. The outer timeout makes a
    // regression a clean FAIL, not an infinite hang.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_bounded_wait_kills_base_that_closes_stdout_then_lingers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_sh_fake(
            tmp.path(),
            "close-stdout-then-linger",
            // Emit a line, CLOSE stdout (exec 1>&-), then sleep well past the ceiling.
            "#!/bin/sh\ncat >/dev/null 2>&1\nprintf 'line\\n'\nexec 1>&-\nsleep 30\n",
        );
        let started = Instant::now();
        let call = run_subprocess_streaming(
            SubprocessCall {
                program: script.to_str().unwrap(),
                args: &[],
                prompt: "",
                channel: PromptChannel::Stdin,
                timeout: Duration::from_secs(2),
                workspace: tmp.path(),
                env: &[],
            },
            &|_line: &str| {},
        );
        let err = tokio::time::timeout(Duration::from_secs(10), call)
            .await
            .expect("the bounded exit wait must fire — not hang on the lingering base")
            .unwrap_err();
        assert!(
            err.contains("timed out"),
            "expected a ceiling timeout, got: {err}"
        );
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    // Lossy per-line decode (streaming): a single invalid UTF-8 byte mid-stream
    // must NOT abort the stream. `next_line()` returned `Err` on bad UTF-8 and the
    // old `while let Ok(Some)` treated that as EOF — dropping every later line. The
    // `read_until` + `from_utf8_lossy` rewrite keeps reading, so the line AFTER the
    // bad byte still arrives.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_invalid_utf8_byte_does_not_truncate_the_stream() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = write_sh_fake(
            tmp.path(),
            "bad-utf8-midstream",
            // first line, then a raw 0xFF byte + newline (invalid UTF-8), then a
            // line that MUST still be read.
            "#!/bin/sh\ncat >/dev/null 2>&1\nprintf 'first\\n'\nprintf '\\377\\n'\nprintf 'second\\n'\n",
        );
        let out = run_subprocess_streaming(
            SubprocessCall {
                program: script.to_str().unwrap(),
                args: &[],
                prompt: "",
                channel: PromptChannel::Stdin,
                timeout: Duration::from_secs(10),
                workspace: tmp.path(),
                env: &[],
            },
            &|_line: &str| {},
        )
        .await
        .expect("a bad UTF-8 byte must not fail the call");
        assert!(
            out.stdout.contains("first"),
            "the pre-bad-byte line survives"
        );
        assert!(
            out.stdout.contains("second"),
            "the line AFTER the invalid byte must still be read (stream not truncated)"
        );
    }
}
