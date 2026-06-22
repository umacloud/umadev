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

    let stderr_buf = stderr_task.await.unwrap_or_default();
    Ok((status, stdout_buf, stderr_buf))
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

/// The user's home directory, derived without pulling in the `dirs` crate:
/// `$HOME` on Unix, `%USERPROFILE%` (then `%HOMEDRIVE%%HOMEPATH%`) on Windows.
fn home_dir() -> Option<PathBuf> {
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
        let mut reader = BufReader::new(stdout).lines();
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
            match tokio::time::timeout(wait, reader.next_line()).await {
                Ok(Ok(Some(line))) => {
                    seen_first_line = true;
                    on_line(&line);
                    all_lines.push(line);
                }
                Ok(Ok(None)) => break, // EOF — stdout closed
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
) -> Result<Box<dyn umadev_runtime::BaseSession>, umadev_runtime::SessionError> {
    match backend_id {
        "claude-code" => {
            // The continuous claude session tracks the autonomy tier like codex /
            // opencode: `autonomous` → `--permission-mode acceptEdits` (write
            // unattended), otherwise → `default` (claude raises a `can_use_tool`
            // approval per tool → a `NeedApproval` the orchestrator answers, the
            // guarded human-in-the-loop tier). We append no extra system prompt
            // here — the runner's directives carry the role + spec constraints per
            // phase.
            let s = ClaudeSession::start(workspace, None, autonomous).await?;
            Ok(Box::new(s))
        }
        "codex" => {
            let s = CodexSession::start(workspace, model, autonomous).await?;
            Ok(Box::new(s))
        }
        "opencode" => {
            // `build` agent; pass the model through only when non-empty so the
            // base falls back to its own configured default otherwise.
            let model = (!model.is_empty()).then_some(model);
            let s = OpenCodeSession::start(workspace, Some("build"), model).await?;
            Ok(Box::new(s))
        }
        other => Err(umadev_runtime::SessionError::Start(format!(
            "unknown backend id for continuous session: {other}"
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
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let got = resolve_program("mybase");
        // Restore before asserting so a failure can't poison sibling tests.
        restore_env("HOME", saved_home);

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
        let saved_home = std::env::var_os("HOME");
        let saved_path = std::env::var_os("PATH");
        std::env::set_var("HOME", home.path());
        std::env::set_var("PATH", on_path.path());
        let got = resolve_program("dualbase");
        restore_env("HOME", saved_home);
        restore_env("PATH", saved_path);

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
        let saved_path = std::env::var_os("PATH");
        let saved_ext = std::env::var_os("PATHEXT");
        std::env::set_var("PATH", tmp.path());
        std::env::set_var("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        let got = resolve_program("winbase");
        restore_env("PATH", saved_path);
        restore_env("PATHEXT", saved_ext);

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
        let saved_local = std::env::var_os("LOCALAPPDATA");
        let saved_path = std::env::var_os("PATH");
        std::env::set_var("LOCALAPPDATA", tmp.path());
        std::env::set_var("PATH", ""); // force the known-dir branch
        let got = resolve_program("winstandalone");
        restore_env("LOCALAPPDATA", saved_local);
        restore_env("PATH", saved_path);

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
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "1");
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
        std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS");
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
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "1");
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
        std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS");
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
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "1");
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
        std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS");
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
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "1");
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
        std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS");
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
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "600");
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
        std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS");
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
        std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS");
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
}
