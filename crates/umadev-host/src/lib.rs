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
pub mod codex;
pub mod opencode;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub use claude::ClaudeCodeDriver;
pub use codex::CodexDriver;
pub use opencode::OpenCodeDriver;

/// Outcome of probing a host CLI for availability.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ProbeResult {
    /// The CLI is installed and responded to `--version`.
    Ready {
        /// Raw version string the CLI reported.
        version: String,
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

    /// Check whether the underlying CLI is installed + reachable.
    async fn probe(&self) -> ProbeResult;

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

/// Drain a spawned child's stdout+stderr to EOF AND wait for its exit, all
/// bounded by ONE timeout. The reads are deliberately inside the timeout: a
/// child that writes some output and then hangs while keeping its stdout pipe
/// open (e.g. a grandchild inherits the pipe) would otherwise block
/// `read_to_end` forever — defeating the `child.wait()` timeout and hanging
/// UmaDev. On timeout the child is killed to avoid orphaned processes.
async fn drain_and_wait(
    child: &mut tokio::process::Child,
    timeout: std::time::Duration,
    program: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut child_stdout = child.stdout.take();
    let mut child_stderr = child.stderr.take();
    let collected = tokio::time::timeout(timeout, async {
        let drain_out = async {
            if let Some(s) = child_stdout.as_mut() {
                let _ = s.read_to_end(&mut stdout_buf).await;
            }
        };
        let drain_err = async {
            if let Some(s) = child_stderr.as_mut() {
                let _ = s.read_to_end(&mut stderr_buf).await;
            }
        };
        let ((), (), status) = tokio::join!(drain_out, drain_err, child.wait());
        status
    })
    .await;
    match collected {
        Ok(Ok(s)) => Ok((s, stdout_buf, stderr_buf)),
        Ok(Err(e)) => Err(format!("`{program}` failed: {e}")),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(format!(
                "`{program}` timed out after {}s",
                timeout.as_secs()
            ))
        }
    }
}

/// Apply extra env overrides to a child command before spawn (an empty value
/// removes the inherited var). Always called with an EMPTY slice today — UmaDev
/// injects nothing into the base; the child inherits the user's full environment
/// so the base self-authenticates with its own login / API. Kept as a generic
/// (currently unused) hook.
fn apply_provider_env(cmd: &mut Command, env: &[(String, String)]) {
    for (key, value) in env {
        if value.is_empty() {
            cmd.env_remove(key);
        } else {
            cmd.env(key, value);
        }
    }
}

/// Resolve a bare program name to a spawnable path. On Windows the base CLIs
/// installed via npm are `.cmd`/`.exe`/`.bat` shims, but `CreateProcess` (and
/// thus `Command::new`) only auto-appends `.exe` to a bare name -- so a bare
/// `claude` never finds `claude.cmd` and the base reads as "not installed". We
/// search `PATH` over `PATHEXT` ourselves and return the first hit's full path
/// (modern Rust runs `.cmd`/`.bat` via cmd.exe with proper escaping). Returns
/// the input unchanged off Windows, when it already looks like a path, or when
/// nothing matches (so the spawn surfaces the real error).
#[must_use]
pub fn resolve_program(program: &str) -> String {
    if !cfg!(windows) || program.contains(std::path::is_separator) {
        return program.to_string();
    }
    let Ok(path_var) = std::env::var("PATH") else {
        return program.to_string();
    };
    let pathext =
        std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    for dir in path_var.split(';') {
        if dir.is_empty() {
            continue;
        }
        for ext in std::iter::once("").chain(pathext.split(';')) {
            let candidate = std::path::Path::new(dir).join(format!("{program}{ext}"));
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    program.to_string()
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

/// Run a host CLI subprocess. Errors carry a human-readable string suitable for
/// `RuntimeError::HostProcess`.
pub(crate) async fn run_subprocess(call: SubprocessCall<'_>) -> Result<SubprocessOutput, String> {
    let started = Instant::now();
    let (program, lead) = spawn_parts(call.program);
    let mut cmd = Command::new(program);
    cmd.args(&lead);
    cmd.args(call.args);
    if matches!(call.channel, PromptChannel::Arg) {
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

    if matches!(call.channel, PromptChannel::Stdin) {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(call.prompt.as_bytes())
                .await
                .map_err(|e| format!("failed to write prompt to stdin: {e}"))?;
            // CRITICAL: `shutdown` flushes and closes the write half. Without it,
            // a plain `write_all` + drop can leave the bytes unflushed in tokio's
            // pipe writer, so the child reads an EMPTY stdin and bails (codex
            // 0.141: "No prompt provided via stdin" → exit 1). shutdown both
            // flushes the buffered prompt AND signals EOF.
            let _ = stdin.shutdown().await;
        }
    } else {
        // Arg channel: the prompt is a CLI arg, so we never write stdin. But
        // the pipe is still open — take and drop it so the child sees EOF
        // immediately instead of blocking on an idle stdin (some CLIs peek
        // stdin in non-interactive mode and would otherwise hang to timeout).
        drop(child.stdin.take());
    }

    // Drain both pipes AND wait for exit under ONE deadline (see
    // `drain_and_wait`): the reads themselves must be bounded, or a child that
    // emits output then hangs with its stdout pipe open blocks forever and
    // defeats the timeout.
    let (status, stdout_buf, stderr_buf) =
        drain_and_wait(&mut child, call.timeout, call.program).await?;

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
/// Timeout and kill-on-drop behavior mirror [`run_subprocess`].
#[allow(clippy::too_many_lines)] // single coherent subprocess operation
pub(crate) async fn run_subprocess_streaming(
    call: SubprocessCall<'_>,
    on_line: &(dyn Fn(&str) + Send + Sync),
) -> Result<SubprocessOutput, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let started = Instant::now();
    let (program, lead) = spawn_parts(call.program);
    let mut cmd = Command::new(program);
    cmd.args(&lead);
    cmd.args(call.args);
    if matches!(call.channel, PromptChannel::Arg) {
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

    if matches!(call.channel, PromptChannel::Stdin) {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(call.prompt.as_bytes())
                .await
                .map_err(|e| format!("failed to write prompt to stdin: {e}"))?;
            // Flush + close the write half (see `run_subprocess`): a bare
            // write_all + drop can leave the prompt unflushed, starving the child.
            let _ = stdin.shutdown().await;
        }
    } else {
        // Arg channel: the prompt is a CLI arg, so we never write stdin. Drop the
        // pipe so the child sees EOF immediately — otherwise a CLI that peeks
        // stdin in non-interactive `stream-json` mode blocks until the idle
        // watchdog kills it (the same defence `run_subprocess` already has).
        drop(child.stdin.take());
    }

    // Read stderr in a separate task so it doesn't block stdout streaming.
    let stderr_task = {
        let se = child.stderr.take();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut se) = se {
                let _ = se.read_to_end(&mut buf).await;
            }
            buf
        })
    };

    // Stream stdout line by line.
    // **Watchdog**: uses a per-line timeout (not the full call timeout) so a
    // hung process (stream-json hang bug #53584) is detected faster. If no
    // line arrives within `idle_timeout`, we kill + error so the caller can
    // retry. The overall `call.timeout` is still the hard ceiling.
    let idle_timeout = std::cmp::min(
        call.timeout,
        Duration::from_secs(
            std::env::var("UMADEV_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
        ),
    );
    let mut all_lines = Vec::new();
    // Total-time hard ceiling: the per-line idle watchdog alone lets a
    // steady-trickle stream (one line every < idle_timeout, forever) outlive
    // `call.timeout`. Bound each wait by whichever is sooner — the idle timeout
    // or the remaining time to the hard deadline (`started` is the spawn time).
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout).lines();
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
            let wait = idle_timeout.min(remaining);
            match tokio::time::timeout(wait, reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    on_line(&line);
                    all_lines.push(line);
                }
                Ok(Ok(None)) => break, // EOF — stdout closed
                Ok(Err(e)) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(format!("`{}` stdout read error: {e}", call.program));
                }
                Err(_) => {
                    // **Idle timeout** — no output for `idle_timeout`. This is
                    // the stream-json hang scenario (#53584). Kill + return a
                    // distinguishable error so callers can retry.
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

    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => return Err(format!("`{}` failed: {e}", call.program)),
    };

    let stderr_buf = stderr_task.await.unwrap_or_default();

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
    if err.contains("timed out") {
        let secs = err
            .split("after ")
            .nth(1)
            .and_then(|s| s.split('s').next())
            .and_then(|n| n.parse::<u64>().ok())
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

/// All backend ids `driver_for` accepts. UmaDev drives exactly three host
/// CLI bases: Claude Code, Codex, and `OpenCode`.
pub const BACKEND_IDS: &[&str] = &["claude-code", "codex", "opencode"];

/// Default per-call timeout for a host CLI invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

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

#[cfg(test)]
mod tests {
    use super::*;
    use umadev_runtime::{CompletionRequest, Message};

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
}
