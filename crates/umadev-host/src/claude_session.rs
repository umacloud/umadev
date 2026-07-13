//! `ClaudeSession` — drives `claude` in the **bidirectional stream-json NDJSON**
//! protocol as ONE long-lived agentic session (the continuous-session model;
//! see `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`).
//!
//! This lives ALONGSIDE the single-shot [`ClaudeCodeDriver`](crate::ClaudeCodeDriver)
//! in `claude.rs`, which is unchanged. Where that one is "prompt in → one text
//! blob out" (a fresh `claude --print` process per phase that re-feeds the whole
//! context and tends to narrate instead of write code), this one:
//!
//! - spawns `claude` **once**, keeps stdin open, and feeds one **directive per
//!   phase** as a stream-json `user` message (the base keeps context across
//!   phases and runs its own agentic tool loop — it WRITES files);
//! - reads stdout NDJSON line-by-line, parsing each into a
//!   [`SessionEvent`](umadev_runtime::SessionEvent) (`ToolCall` = the truth of
//!   what it did; `result` = the turn-done boundary);
//! - exposes the [`BaseSession`] contract the 9-phase runner drives.
//!
//! Launch flags (from the headless stream-json contract):
//! `claude --print --input-format stream-json --output-format stream-json
//! --verbose --session-id <uuid> --permission-mode <acceptEdits|default>
//! --allowedTools <read-only + research + sub-agent set; auto adds the mutating
//! Edit/Write/Bash/NotebookEdit>` (+ optional `--append-system-prompt`). The base's
//! native read/research/delegate tools (incl. `Agent`/`Task` sub-agents) are
//! pre-approved so they run natively instead of eating a per-tool approval — see
//! [`GUARDED_ALLOWED_TOOLS`] / [`AUTO_ALLOWED_TOOLS`].
//! We deliberately use `--append-system-prompt` (NOT `--system-prompt`, which
//! would replace the tool guidance and degrade the base into a chat box).
//!
//! The permission mode tracks the autonomy tier so claude is consistent with the
//! codex / opencode drivers: `autonomous` (auto tier) → `acceptEdits` (the base
//! writes unattended), non-autonomous (guarded / plan tier) → `default` (claude
//! raises a `can_use_tool` approval for each tool, which becomes a
//! `NeedApproval` the orchestrator answers — the human-in-the-loop floor, so the
//! irreversible-action gate is not bypassed). `UMADEV_CLAUDE_PERMISSION_MODE`
//! overrides the derived default when set.
//!
//! Fail-open by contract: a garbled line is skipped, a dead session surfaces a
//! [`TurnStatus::Failed`], never a panic.

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use umadev_runtime::{
    ApprovalDecision, BackgroundTaskSignal, BaseSession, SessionError, SessionEvent, TurnStatus,
    Usage,
};

use crate::spawn_parts;
use crate::stderr_tail::{drain_stderr_into, StderrTail};
use crate::{reap_after_kill, END_REAP_BUDGET};

/// How many events the stdout-reader task may buffer ahead of the consumer.
const EVENT_CHANNEL_CAP: usize = 256;

/// Turn ceiling for a read-only **critic-consult fork** — a RUNAWAY BACKSTOP, not a
/// work budget. A critic seat reads the on-disk blackboard and returns ONE JSON
/// verdict; it must never spin a long agentic loop, so its fork is spawned with a
/// very low `--max-turns` — well below any real build's cap (the deliberate-build
/// tiers live in `umadev_agent::router::Depth::max_turns`: Fast 40 / Standard 150 /
/// Deep 400). claude reports hitting the ceiling as `error_max_turns` →
/// [`TurnStatus::Truncated`], which the critic path already treats as "accept what
/// landed", so a capped critic degrades fail-open, never a panic.
const CRITIC_FORK_MAX_TURNS: u32 = 20;

/// A live, long-lived `claude` stream-json session.
pub struct ClaudeSession {
    /// The base child. Behind a [`std::sync::Mutex`] so the `&self`
    /// [`BaseSession::try_exit_status`] can do a non-blocking `try_wait()` peek
    /// (which needs `&mut Child`) without forcing the whole trait method to take
    /// `&mut self`. `kill_on_drop(true)` still fires when the struct (and so the
    /// `Child`) drops; `end()` kills through the lock.
    child: std::sync::Mutex<Child>,
    stdin: ChildStdin,
    events: mpsc::Receiver<SessionEvent>,
    /// Bounded tail of the base's STDERR, captured by the drain task, surfaced
    /// via [`BaseSession::stderr_tail`] to explain *why* a base went idle.
    stderr: StderrTail,
    /// The pinned conversation id (also usable for `--resume` on recovery). A
    /// read-only critic fork does NOT reuse this — it opens a FRESH independent
    /// session instead (see [`fork`](BaseSession::fork)), so the critic never
    /// inherits the main line's deliberation.
    session_id: String,
    /// The resolved `claude` program string this session was spawned with, kept
    /// so [`fork`](BaseSession::fork) re-spawns the SAME binary (honoring a test
    /// fake / `UMADEV_CLAUDE_BIN` override).
    program: String,
    /// The workspace this session runs in, so a fork operates in the same dir.
    workspace: std::path::PathBuf,
    /// Temp file backing `--append-system-prompt-file` when the composed firmware
    /// was too large for the command line (the Windows `cmd.exe` ~8191 cap; see
    /// [`crate::command_line_budget`]). Held for the whole session lifetime so
    /// `claude` can read it, and deleted when the session drops. `None` on the
    /// normal inline `--append-system-prompt` fast path (small firmware / a fork,
    /// whose args carry no firmware). Never read directly — kept only for its
    /// `Drop` cleanup.
    _firmware_file: Option<FirmwareFile>,
}

impl ClaudeSession {
    /// Start a session driving the default `claude` binary
    /// (`UMADEV_CLAUDE_BIN` override honored), in `workspace`, optionally
    /// appending `append_system` to the base's system prompt. A fresh pinned
    /// session id is generated.
    ///
    /// `autonomous` selects the permission mode (see [`session_args`]): `true` →
    /// `acceptEdits` (write unattended), `false` → `default` (claude asks before
    /// each tool, surfaced as a `NeedApproval` — the guarded human-in-the-loop
    /// tier). This mirrors the codex / opencode drivers' autonomy handling.
    ///
    /// `max_turns` is an OPTIONAL per-run turn ceiling (a runaway backstop): `Some(n)`
    /// spawns claude with `--max-turns <n>`, `None` leaves it unbounded (today's
    /// behavior). The cap is derived by the caller from the route depth
    /// (`umadev_agent::router::Depth::max_turns`); see [`session_args`].
    pub async fn start(
        workspace: &Path,
        append_system: Option<&str>,
        autonomous: bool,
        max_turns: Option<u32>,
    ) -> Result<Self, SessionError> {
        // Resolve the SAME way the single-shot driver does: honor UMADEV_CLAUDE_BIN, else on
        // Windows prefer the REAL `@anthropic-ai/claude-code/bin/claude.exe` over the bare
        // `claude` PATH entry (a `.cmd`/`.ps1` shim). Spawning the shim wraps it as
        // `cmd /c claude.cmd`, which (a) surfaces as os error 193/232 (broken pipe) and (b)
        // makes kill/exit-status target cmd.exe while the real node `claude` orphans. Using
        // the real binary directly fixes both on the continuous (default) path.
        let program = crate::claude::resolve_claude_program();
        Self::start_with_program(
            &program,
            workspace,
            append_system,
            &new_session_id(),
            autonomous,
            max_turns,
        )
        .await
    }

    /// Start a session against an explicit `program` + pinned `session_id`
    /// (mainly for tests, where `program` is a fake stream-json emitter).
    /// `autonomous` chooses the permission mode (see [`session_args`]); `max_turns`
    /// is the optional `--max-turns` runaway backstop (`None` → unbounded).
    pub async fn start_with_program(
        program: &str,
        workspace: &Path,
        append_system: Option<&str>,
        session_id: &str,
        autonomous: bool,
        max_turns: Option<u32>,
    ) -> Result<Self, SessionError> {
        Self::spawn_with_args(
            program,
            workspace,
            &session_args(session_id, append_system, autonomous, max_turns),
            session_id,
        )
        .await
    }

    /// **Cross-session resume** — re-open the WRITABLE main line of an existing
    /// claude conversation (`session_id`) instead of minting a fresh one. The base
    /// re-supplies its OWN persisted transcript (`~/.claude/projects/…/<id>.jsonl`),
    /// so a `/continue` after the TUI closed mid-build gets full context for free —
    /// no re-priming a cold brain that "forgot the task". Uses
    /// [`resume_session_args`] (`--resume <id>` WITHOUT `--fork-session`, so it is
    /// the writable main line, not a read-only critic branch). The struct keeps the
    /// SAME `session_id`, so a later [`session_id`](BaseSession::session_id) re-persist
    /// is idempotent.
    ///
    /// `UMADEV_CLAUDE_BIN` override honored. Fail-open by contract: a spawn failure
    /// surfaces as [`SessionError::Start`] — the caller degrades to a fresh
    /// [`start`](Self::start), never blocks.
    pub async fn resume(
        workspace: &Path,
        append_system: Option<&str>,
        session_id: &str,
        autonomous: bool,
        max_turns: Option<u32>,
    ) -> Result<Self, SessionError> {
        // Resolve the SAME way the single-shot driver does: honor UMADEV_CLAUDE_BIN, else on
        // Windows prefer the REAL `@anthropic-ai/claude-code/bin/claude.exe` over the bare
        // `claude` PATH entry (a `.cmd`/`.ps1` shim). Spawning the shim wraps it as
        // `cmd /c claude.cmd`, which (a) surfaces as os error 193/232 (broken pipe) and (b)
        // makes kill/exit-status target cmd.exe while the real node `claude` orphans. Using
        // the real binary directly fixes both on the continuous (default) path.
        let program = crate::claude::resolve_claude_program();
        Self::spawn_with_args(
            &program,
            workspace,
            &resume_session_args(session_id, append_system, autonomous, max_turns),
            session_id,
        )
        .await
    }

    /// Spawn a `claude` child with an explicit argument vector and wire up the
    /// stdin / stdout-reader / stderr-drain plumbing. Shared by the main-session
    /// start and the read-only [`fork`](BaseSession::fork) start so both paths
    /// use identical, tested process wiring.
    // `tokio::process::Command::spawn` is sync; async kept for a uniform,
    // forward-compatible session-start API the runner awaits.
    #[allow(clippy::unused_async)]
    async fn spawn_with_args(
        program: &str,
        workspace: &Path,
        args: &[String],
        session_id: &str,
    ) -> Result<Self, SessionError> {
        let (prog, lead) = spawn_parts(program);
        // Move an oversized `--append-system-prompt <firmware>` OFF the command line
        // (to a temp file passed as `--append-system-prompt-file <path>`) when the
        // whole line would exceed the Windows `cmd.exe` ~8191 cap — otherwise an npm
        // `.cmd` shim invoked via `cmd /c` truncates the firmware → corrupted system
        // prompt. Small firmware / mac+Linux keep the inline arg (fast path); a fork's
        // args carry no firmware, so this is a no-op there. Fail-open (a temp-write
        // error keeps the inline arg). The guard is held on the session so the file
        // lives for the child's lifetime and is cleaned up on drop.
        let (args, firmware_file) = maybe_divert_firmware(&prog, &lead, args);
        let mut cmd = Command::new(&prog);
        cmd.args(&lead);
        cmd.args(&args);
        cmd.current_dir(workspace);
        // Mark "UmaDev is driving" + the governed root for the PreToolUse hook
        // (see `crate::GOVERN_ROOT_ENV`). The base inherits this var and passes
        // it to the hook subprocess it spawns, so the hook governs THIS session's
        // writes while leaving the user's own claude sessions completely
        // untouched. Set on every spawned `claude` (main + read-only fork) so the
        // governance scope is consistent across the session's process tree.
        cmd.env(crate::GOVERN_ROOT_ENV, workspace);
        // Belt for the base's OWN background sub-agents: in `--print` mode claude
        // waits at wind-down (stdin closed, main thread done) for outstanding
        // background tasks only up to a ceiling (default 600000 ms = 10 min), then
        // sweeps them — killing a still-running background agent mid-write. Raise
        // the ceiling so headless waits longer before sweeping. The user's own
        // value always wins (only set when absent); the PRIMARY guard against a
        // premature final report is the observable outstanding-agents counter +
        // bounded re-drive in the orchestrator, which works on every base.
        if std::env::var_os(crate::claude::PRINT_BG_WAIT_CEILING_ENV).is_none() {
            cmd.env(
                crate::claude::PRINT_BG_WAIT_CEILING_ENV,
                crate::claude::PRINT_BG_WAIT_CEILING_DEFAULT_MS,
            );
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = crate::spawn_retrying_etxtbsy(&mut cmd)
            .map_err(|e| SessionError::Start(spawn_err(program, &e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SessionError::Start("child stdin not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SessionError::Start("child stdout not piped".to_string()))?;
        // stderr drains on its OWN task so a base that floods/holds stderr can
        // never stall the stdout reader (the non-streaming-path lesson). The
        // drain ALSO captures a bounded tail so a config error the base printed
        // to stderr before falling silent can be surfaced as the idle reason.
        let stderr_tail = StderrTail::new();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr_into(stderr, stderr_tail.clone()));
        }

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        tokio::spawn(pump_stdout(stdout, tx));

        Ok(Self {
            child: std::sync::Mutex::new(child),
            stdin,
            events: rx,
            stderr: stderr_tail,
            session_id: session_id.to_string(),
            program: program.to_string(),
            workspace: workspace.to_path_buf(),
            _firmware_file: firmware_file,
        })
    }

    /// The pinned conversation id (e.g. for `--resume` on crash recovery).
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Write one NDJSON line + flush to the live session's stdin.
    async fn write_line(&mut self, line: &str) -> Result<(), SessionError> {
        // Pre-send liveness: if the child already EXITED (a GLM/third-party API error killed
        // `claude --print` between turns), the writes below would fail with a raw broken pipe
        // (os error 232 on Windows / 32 on Unix). Detect the dead child FIRST and return a
        // typed "base session ended" reason so the caller recognizes session loss and reopens
        // a fresh session (+ transcript replay) instead of surfacing the confusing pipe error
        // and re-resuming a corpse every subsequent turn.
        if let Some(status) = self.try_exit_status() {
            return Err(SessionError::Send(format!(
                "base session ended before send (base exited: {status})"
            )));
        }
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| SessionError::Send(e.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| SessionError::Send(e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| SessionError::Send(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl BaseSession for ClaudeSession {
    async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
        // A read-only critic fork that CARRIES THE BUILD CONVERSATION. `--resume
        // <main> --fork-session` re-loads the doer's LIVE transcript into a NEW,
        // isolated forked session, so the critic (QA / security / architect …) judges
        // with everything the doer saw — not just the on-disk `output/*.md` + source
        // tree. The fork is ISOLATED: `--fork-session` mints its OWN session id, so the
        // critic's turns branch off and never touch the parent's writable main line
        // (single-writer invariant). It is READ-ONLY: `--permission-mode plan` (never
        // applies an edit) + the `Read,Grep,Glob` allowlist are two independent fences
        // on that same invariant — only the main session ever writes the blackboard.
        // The inherited maker reasoning is quarantined at the PROMPT boundary by
        // `INDEPENDENT_REVIEW_FIREWALL` (see `umadev_agent::continuous`), so carrying
        // the transcript does not leak the author's framing into the verdict. Spawned
        // with `current_dir(workspace)`, so it also sees the same on-disk blackboard.
        //
        // FAIL-OPEN (critical — a broken fork must NEVER break the critic): when no
        // live parent id is available (empty — no continuous session yet / single-shot
        // path / offline base) we open TODAY's FRESH independent read-only session
        // ([`fork_session_args`]) instead; and if the resume-fork spawn itself fails we
        // degrade to that same fresh fork rather than deny the critic a session. A
        // spawn failure ultimately still surfaces as `Start`, which the caller treats
        // like `ForkUnsupported` (advisory-accept). The fork takes NO run-lock — critics
        // run in parallel, read-only, off the single-writer lock (unchanged invariant).
        let fork_id = new_session_id();
        let carries_transcript = !self.session_id.trim().is_empty();
        let args = critic_fork_args(&self.session_id, &fork_id);
        match Self::spawn_with_args(&self.program, &self.workspace, &args, &fork_id).await {
            Ok(s) => Ok(Box::new(s)),
            // A resume-fork that failed to SPAWN degrades to the fresh read-only fork
            // (fail-open). When we already chose fresh, `carries_transcript` is false
            // and the error propagates unchanged (there is no cleaner fallback left).
            Err(e) if carries_transcript => {
                tracing::debug!(
                    error = %e,
                    "resume-fork spawn failed; degrading to a fresh read-only critic fork"
                );
                let fresh = fork_session_args(&fork_id);
                let s =
                    Self::spawn_with_args(&self.program, &self.workspace, &fresh, &fork_id).await?;
                Ok(Box::new(s))
            }
            Err(e) => Err(e),
        }
    }

    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        self.write_line(&user_message_line(&directive)).await
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        // No internal timeout BY DESIGN — the runner owns phase/run budgets and
        // races this against them (then calls `interrupt`). Keeping the session
        // a pure relay avoids a synthetic TurnDone racing a real one.
        self.events.recv().await
    }

    async fn respond(
        &mut self,
        req_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError> {
        let behavior = match decision {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::Deny => "deny",
        };
        let line = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": req_id,
                "response": { "behavior": behavior }
            }
        })
        .to_string();
        self.write_line(&line).await
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        let line = serde_json::json!({
            "type": "control_request",
            "request_id": new_session_id(),
            "request": { "subtype": "interrupt" }
        })
        .to_string();
        self.write_line(&line).await
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        // Best-effort: kill the child (drops stdin → EOF, tears down the
        // reader/stderr tasks) AND wait (bounded) for it to be reaped so shutdown
        // is deterministic and leaves no orphan. On overrun we fail open to
        // kill_on_drop. Consistent with codex / opencode `end()`.
        reap_after_kill(&self.child, END_REAP_BUDGET).await;
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        self.stderr.snapshot()
    }

    fn try_exit_status(&self) -> Option<std::process::ExitStatus> {
        // Non-blocking peek (the lock + try_wait both never block): a contended
        // lock or a try_wait error fails open to None; `Ok(Some(status))` = the
        // base exited, `Ok(None)` = still alive.
        self.child.try_lock().ok()?.try_wait().ok().flatten()
    }

    fn session_id(&self) -> Option<&str> {
        // The pinned conversation id — the pointer a later `/continue` resumes
        // via [`ClaudeSession::resume`] (`--resume <id>`), restoring claude's OWN
        // accumulated transcript for full-context cross-session resume.
        Some(&self.session_id)
    }
}

/// Reader task: parse stdout NDJSON → events forever. On EOF (the base process
/// died / the session ended) emit a terminal `Failed` so a crash mid-turn
/// surfaces as `TurnDone{Failed}` rather than a silent hang.
///
/// Lines flow through a per-session [`SubagentGrouper`] (NOT the stateless
/// [`parse_stdout_line`] directly): a NESTED sub-agent's streamed frames are
/// buffered and flushed as ONE grouped block instead of interleaving
/// fragmentarily with the main agent's output, while MAIN-line frames yield
/// byte-for-byte the events `parse_stdout_line` produces (see the grouper's
/// contract + tests).
async fn pump_stdout(stdout: ChildStdout, tx: mpsc::Sender<SessionEvent>) {
    // Read raw bytes per line and decode LOSSY: `next_line` returns `Err` on a
    // single invalid UTF-8 byte, and the old `while let Ok(Some)` treated that as
    // end-of-stream — discarding the rest of the NDJSON turn AND emitting a
    // spurious "base session ended unexpectedly". `read_until('\n')` +
    // `from_utf8_lossy` tolerates a bad byte (a non-JSON line is ignored by
    // `parse_stdout_line`, not the whole stream).
    let mut reader = BufReader::new(stdout);
    let mut line_buf = Vec::new();
    let mut grouper = SubagentGrouper::default();
    loop {
        line_buf.clear();
        match reader.read_until(b'\n', &mut line_buf).await {
            Ok(0) | Err(_) => break, // EOF or read error → the base process is gone
            Ok(_) => {
                let line = String::from_utf8_lossy(&line_buf);
                for ev in grouper.on_line(line.trim_end_matches(['\r', '\n'])) {
                    if tx.send(ev).await.is_err() {
                        return; // consumer dropped → stop
                    }
                }
            }
        }
    }
    // The base died / the stream ended: flush any still-held sub-agent buffers
    // FIRST so nothing a sub-agent produced is ever silently dropped, then the
    // synthetic terminal Failed.
    for ev in grouper.flush_all() {
        if tx.send(ev).await.is_err() {
            return; // consumer dropped → stop
        }
    }
    let _ = tx
        .send(SessionEvent::TurnDone {
            status: TurnStatus::Failed("base session ended unexpectedly".to_string()),
            usage: None,
        })
        .await;
}

fn spawn_err(program: &str, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("`{program}` not found on PATH")
    } else {
        format!("failed to spawn `{program}`: {e}")
    }
}

/// A temp file that carries the composed firmware to `claude` via
/// `--append-system-prompt-file` instead of on the command line, deleted when this
/// guard drops. The owning [`ClaudeSession`] holds it for the child's whole lifetime
/// (claude reads the file at startup) and cleans it up when the session ends.
struct FirmwareFile {
    /// Absolute path of the written temp file.
    path: std::path::PathBuf,
}

impl FirmwareFile {
    /// Write `text` to a freshly, uniquely named temp file under `dir`. Fail-open:
    /// propagates the I/O error so the caller can fall back to the inline arg.
    fn write_in(dir: &Path, text: &str) -> std::io::Result<Self> {
        // A UUID name avoids collisions across concurrent sessions / critic forks.
        let path = dir.join(format!("umadev-firmware-{}.txt", new_session_id()));
        std::fs::write(&path, text)?;
        Ok(Self { path })
    }
}

impl Drop for FirmwareFile {
    fn drop(&mut self) {
        // Best-effort cleanup; a leftover temp file must never crash the session.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Rewrite an inline `--append-system-prompt <firmware>` pair in `args` to
/// `--append-system-prompt-file <tempfile>` (written under `dir`), returning the
/// temp-file guard the caller must keep alive for the child's lifetime. This moves a
/// multi-KB firmware OFF the command line so a Windows `.cmd` shim invoked via
/// `cmd /c` (cap ~8191) cannot truncate it. `--append-system-prompt-file` is a
/// documented `claude` flag (verified via `claude --help`: "via:
/// --system-prompt[-file], --append-system-prompt[-file]").
///
/// **Fail-open by contract:** when the flag is absent (e.g. a read-only fork's args),
/// has no value, or the temp write fails, `args` is returned UNCHANGED with no guard —
/// the inline arg stays (mac/Linux tolerate the big arg; on Windows this is the
/// pre-existing behavior, never a crash). Deterministic given `dir`; exposed for tests.
fn divert_append_system_to_file_in(
    mut args: Vec<String>,
    dir: &Path,
) -> (Vec<String>, Option<FirmwareFile>) {
    let Some(flag_idx) = args.iter().position(|a| a == "--append-system-prompt") else {
        return (args, None);
    };
    let val_idx = flag_idx + 1;
    if val_idx >= args.len() {
        return (args, None);
    }
    match FirmwareFile::write_in(dir, &args[val_idx]) {
        Ok(file) => {
            args[flag_idx] = "--append-system-prompt-file".to_string();
            args[val_idx] = file.path.to_string_lossy().into_owned();
            (args, Some(file))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not write firmware temp file; passing --append-system-prompt inline (may exceed the Windows command-line limit)"
            );
            (args, None)
        }
    }
}

/// Move an oversized firmware off the command line when the spawn tokens
/// (`prog` + `lead` + `args`) would exceed the platform command-line budget (the
/// Windows `cmd.exe` ~8191 cap; see [`crate::command_line_budget`]). Under budget →
/// the fast argv path is kept unchanged (the inline `--append-system-prompt`, no temp
/// file). Fail-open (see [`divert_append_system_to_file_in`]).
fn maybe_divert_firmware(
    prog: &str,
    lead: &[String],
    args: &[String],
) -> (Vec<String>, Option<FirmwareFile>) {
    let line = crate::command_line_len(
        std::iter::once(prog)
            .chain(lead.iter().map(String::as_str))
            .chain(args.iter().map(String::as_str)),
    );
    if line <= crate::command_line_budget() {
        return (args.to_vec(), None);
    }
    divert_append_system_to_file_in(args.to_vec(), &std::env::temp_dir())
}

/// The read-only + research + delegate native tools UmaDev ALWAYS pre-approves —
/// even in Guarded — so the base keeps its native capabilities under UmaDev instead
/// of eating a `can_use_tool` round-trip (and, in interactive Guarded chat, a
/// confusing user pause that fail-open DENIES) for every `Grep` / `Glob` /
/// `WebSearch` / `WebFetch` / `TodoWrite` and every sub-agent spawn. `Agent` / `Task`
/// (the current + legacy sub-agent tool names) are pre-approved so the base's OWN
/// sub-agents (Explore etc.) run natively; a sub-agent's own `Edit` / `Write` /
/// `Bash` still pass through governance (it runs in the SAME claude process, so the
/// PreToolUse hook + the per-tool floor still gate its mutations). Every tool here is
/// read-only / side-effect-free, so pre-approving them bypasses NO write governance.
/// Honors the "inject NOTHING — the base's native capabilities run" contract.
/// `TaskOutput` / `BashOutput` / `AgentOutput` (current + legacy names) READ a
/// background task's status/output — pre-approved so the base can collect its own
/// background sub-agents' results (the outstanding-agents settle guard re-drives it
/// to do exactly that) without eating an approval pause; `KillShell` mutates (stops
/// a task) and stays gated.
const GUARDED_ALLOWED_TOOLS: &str =
    "Read,Grep,Glob,WebSearch,WebFetch,TodoWrite,Agent,Task,TaskOutput,BashOutput,AgentOutput";

/// AUTO additionally pre-approves the MUTATING working set (`Edit` / `Write` / `Bash`
/// / `NotebookEdit`) so an unattended autonomous run is never interrupted by a
/// per-tool prompt — the autonomy tier the user opted into.
const AUTO_ALLOWED_TOOLS: &str = "Read,Edit,Write,Bash,Grep,Glob,WebSearch,WebFetch,TodoWrite,\
     NotebookEdit,Agent,Task,TaskOutput,BashOutput,AgentOutput";

/// The `--allowedTools` value for an autonomy tier: AUTO pre-approves the mutating set
/// too; GUARDED / plan pre-approves only the read-only + research + sub-agent set so
/// `Edit` / `Write` / `Bash` still hit UmaDev's per-tool trust floor.
#[must_use]
fn allowed_tools_for(autonomous: bool) -> String {
    if autonomous {
        AUTO_ALLOWED_TOOLS.to_string()
    } else {
        GUARDED_ALLOWED_TOOLS.to_string()
    }
}

/// The argument vector preceding any input — the stream-json continuous-session
/// flags. Exposed for tests. `--append-system-prompt` (NOT `--system-prompt`).
///
/// `autonomous` picks the permission mode so claude tracks the trust tier like
/// the codex / opencode drivers: `true` → `acceptEdits` (write unattended),
/// `false` → `default` (claude raises a `can_use_tool` approval per tool, which
/// the orchestrator answers — keeping the human-in-the-loop / irreversible-action
/// floor live). `UMADEV_CLAUDE_PERMISSION_MODE`, when set, overrides the derived
/// default for both tiers.
///
/// `max_turns` is the OPTIONAL per-run turn ceiling (a runaway backstop): `Some(n)`
/// appends `--max-turns <n>`, `None` omits the flag entirely — leaving claude's
/// default unbounded agentic loop (today's behavior). The caller derives the cap from
/// the route depth (`umadev_agent::router::Depth::max_turns` — Fast 40 / Standard 150
/// / Deep 400); hitting it is reported as `error_max_turns` → [`TurnStatus::Truncated`]
/// (already handled by [`parse_result`]), so no new parsing is needed. Fail-open: no
/// cap → no flag → unchanged behavior.
#[must_use]
pub fn session_args(
    session_id: &str,
    append_system: Option<&str>,
    autonomous: bool,
    max_turns: Option<u32>,
) -> Vec<String> {
    let permission_mode = claude_permission_mode(autonomous);
    let mut args = vec![
        "--print".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // Stream incremental text. WITHOUT this, claude buffers the whole assistant
        // text and emits it as a SINGLE `assistant` block only when generation
        // completes — so a pure-text chat reply produces ZERO events until the end,
        // the 60s stall fires, the spinner goes red + freezes, and the answer floods
        // in at once. With it, claude emits `stream_event` content_block_delta frames
        // we surface as `TextDelta`s, so the reply renders token-by-token and the
        // stall clock keeps resetting. (The final aggregate `assistant` text block is
        // then suppressed in `block_to_event` to avoid doubling the text.)
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
        "--permission-mode".to_string(),
        permission_mode,
        "--allowedTools".to_string(),
        allowed_tools_for(autonomous),
    ];
    push_max_turns(&mut args, max_turns);
    if let Some(sys) = append_system.filter(|s| !s.is_empty()) {
        args.push("--append-system-prompt".to_string());
        args.push(sys.to_string());
    }
    args
}

/// Append `--max-turns <n>` to `args` when a cap is set; a `None` cap appends
/// NOTHING (fail-open by omission → claude's default unbounded loop, today's
/// behavior). Shared by the main-session, resume, and critic-fork arg builders so the
/// optional turn ceiling is shaped identically everywhere. Deterministic.
fn push_max_turns(args: &mut Vec<String>, max_turns: Option<u32>) {
    if let Some(n) = max_turns {
        args.push("--max-turns".to_string());
        args.push(n.to_string());
    }
}

/// Resolve claude's `--permission-mode` for an autonomy tier. `autonomous` →
/// `acceptEdits` (write unattended); otherwise `default` (claude asks before
/// each tool → a `NeedApproval` the orchestrator answers, the guarded
/// human-in-the-loop tier). `UMADEV_CLAUDE_PERMISSION_MODE` overrides both.
///
/// **Full-bypass passthrough (documented contract).** Because UmaDev always
/// passes an EXPLICIT `--permission-mode`, a user who configured claude's own
/// full bypass (`permissions.defaultMode: "bypassPermissions"` in their claude
/// settings) is otherwise DOWNGRADED — the CLI flag beats their settings. The
/// supported way to keep full bypass under UmaDev is
/// `UMADEV_CLAUDE_PERMISSION_MODE=bypassPermissions` (or `dontAsk`): the
/// override is passed through verbatim on BOTH tiers (locked by test below).
/// UmaDev deliberately does not auto-read the user's claude settings to infer
/// bypass — an explicit opt-in keeps the irreversible-action floor from being
/// silently dropped.
///
/// **Guarded-tier awareness guard (labeling fix, not a lifecycle change).**
/// UmaDev's Guarded tier drives the base through per-tool `NeedApproval` prompts
/// and does NOT model the base's OWN plan mode. A stale or explicit
/// `UMADEV_CLAUDE_PERMISSION_MODE=plan` on the guarded path would silently open the
/// base in a plan mode UmaDev can't track — the base's `ExitPlanMode` would then
/// surface under the wrong "guarded" framing. So a `plan` override is IGNORED for
/// the guarded tier: Guarded always opens with the tracked `default`. Every other
/// override, and the autonomous tier (including a `plan` override), is honored
/// unchanged — this does not alter what UmaDev's `TrustMode::Plan` does (that tier
/// stops the run at the docs/plan gate and is a separate mechanism entirely).
fn claude_permission_mode(autonomous: bool) -> String {
    let derived = if autonomous { "acceptEdits" } else { "default" };
    match std::env::var("UMADEV_CLAUDE_PERMISSION_MODE") {
        Ok(over) if !over.is_empty() => {
            if !autonomous && over.eq_ignore_ascii_case("plan") {
                // Guarded must never silently enter the base's untracked plan mode.
                derived.to_string()
            } else {
                over
            }
        }
        _ => derived.to_string(),
    }
}

/// The argument vector for a WRITABLE cross-session resume: re-open `session_id`
/// with `--resume <id>` and **NO** `--fork-session` (this IS the main writable
/// line, not a read-only critic branch) and **NO** fresh `--session-id` (we are
/// continuing the existing conversation, not pinning a new one). All the other
/// stream-json + permission + allowed-tools flags mirror [`session_args`] exactly,
/// so a resumed session writes files identically to a fresh one — it just inherits
/// the base's accumulated transcript. `max_turns` shapes the optional `--max-turns`
/// runaway backstop exactly like [`session_args`] (`None` → unbounded). Exposed for
/// tests.
#[must_use]
pub fn resume_session_args(
    session_id: &str,
    append_system: Option<&str>,
    autonomous: bool,
    max_turns: Option<u32>,
) -> Vec<String> {
    let permission_mode = claude_permission_mode(autonomous);
    let mut args = vec![
        "--print".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
        // Re-open the existing conversation on its WRITABLE main line. No
        // `--fork-session` (that branches read-only), no new `--session-id` (that
        // mints a fresh one) — `--resume <id>` alone resumes + continues writing it.
        "--resume".to_string(),
        session_id.to_string(),
        "--permission-mode".to_string(),
        permission_mode,
        "--allowedTools".to_string(),
        allowed_tools_for(autonomous),
    ];
    push_max_turns(&mut args, max_turns);
    if let Some(sys) = append_system.filter(|s| !s.is_empty()) {
        args.push("--append-system-prompt".to_string());
        args.push(sys.to_string());
    }
    args
}

/// The argument vector for the FALLBACK read-only critic fork: a FRESH,
/// INDEPENDENT session pinned to `fork_session_id` with **NO** `--resume <main>`
/// and **NO** `--fork-session`. This is the FAIL-OPEN degrade of
/// [`resume_fork_session_args`] — used when there is no live parent transcript to
/// branch (no continuous session yet / single-shot path / offline base), or when
/// the resume-fork spawn itself failed. A fresh session starts on a clean context
/// and reviews only the on-disk artifact (the produced `output/*.md` + the source
/// tree, read via `Read,Grep,Glob`) plus the judge directive. It is spawned with
/// `current_dir(workspace)` (see [`ClaudeSession::spawn_with_args`]), so the clean
/// session still SEES the same on-disk blackboard the main line wrote.
/// `--permission-mode plan` + `--allowedTools "Read,Grep,Glob"` are two
/// independent fences on the single-writer invariant (read the workspace, never
/// write a file). Mirrors opencode's fresh-independent-session fork. Exposed for
/// tests.
#[must_use]
pub fn fork_session_args(fork_session_id: &str) -> Vec<String> {
    let mut args = vec![
        "--print".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // Stream incremental text here too (a critic fork's verdict text must arrive
        // as deltas, not buffered — see `session_args`). Keeps `block_to_event`'s
        // text-suppression invariant: text always comes via `stream_event` deltas.
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
        // A FRESH pinned conversation — NO `--resume <main>` (that re-loads the
        // doer's transcript) and NO `--fork-session` (that branches the live main
        // line). The critic's context is genuinely clean at the host level; it reads
        // the artifact from disk + the directive instead of the main deliberation.
        "--session-id".to_string(),
        fork_session_id.to_string(),
        // Read-only: plan mode never applies an edit; the tool allowlist is
        // read-only too. Two independent fences on the single-writer invariant.
        "--permission-mode".to_string(),
        "plan".to_string(),
        "--allowedTools".to_string(),
        "Read,Grep,Glob".to_string(),
    ];
    // A read-only verdict consult is turn-capped LOW — a runaway backstop so a critic
    // can never spin a long agentic loop (see `CRITIC_FORK_MAX_TURNS`).
    push_max_turns(&mut args, Some(CRITIC_FORK_MAX_TURNS));
    args
}

/// The argument vector for the DEFAULT read-only critic fork — the one that
/// CARRIES THE BUILD CONVERSATION. `--resume <main_session_id> --fork-session`
/// re-loads the doer's LIVE transcript into a NEW, isolated forked session, so the
/// critic judges with everything the doer saw (QA/security/architect seats see the
/// whole build, not just the on-disk `output/*.md`). `--fork-session` is what keeps
/// the single-writer invariant: the fork mints its OWN session id, so the critic's
/// turns branch off and never touch the parent's writable main line. It is
/// READ-ONLY: `--permission-mode plan` (never applies an edit) + the `Read,Grep,Glob`
/// allowlist are two independent fences on that invariant. The maker's inherited
/// reasoning is quarantined at the PROMPT boundary by `INDEPENDENT_REVIEW_FIREWALL`
/// (in `umadev_agent::continuous`), so carrying the transcript does not bias the
/// verdict. When there is no live parent id, [`critic_fork_args`] degrades to the
/// FRESH [`fork_session_args`] fallback instead. Exposed for tests.
#[must_use]
pub fn resume_fork_session_args(main_session_id: &str) -> Vec<String> {
    let mut args = vec![
        "--print".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // Stream the verdict text as deltas here too (see `fork_session_args` /
        // `session_args`), so a critic's verdict renders token-by-token.
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
        // Branch the LIVE build conversation: `--resume <main>` re-loads the doer's
        // transcript; `--fork-session` writes it into a FRESH forked session id so the
        // critic's turns never mutate the parent (the single-writer invariant). We do
        // NOT also pin a fresh `--session-id` — that would start an empty conversation
        // and drop the very transcript this fork exists to carry.
        "--resume".to_string(),
        main_session_id.to_string(),
        "--fork-session".to_string(),
        // Read-only: plan mode never applies an edit; the tool allowlist is read-only
        // too. Two independent fences on the single-writer invariant.
        "--permission-mode".to_string(),
        "plan".to_string(),
        "--allowedTools".to_string(),
        "Read,Grep,Glob".to_string(),
    ];
    // Same low turn ceiling as the fresh fork — a carried-transcript critic is still a
    // one-verdict consult, never a long agentic loop (see `CRITIC_FORK_MAX_TURNS`).
    push_max_turns(&mut args, Some(CRITIC_FORK_MAX_TURNS));
    args
}

/// Choose the critic fork's argument vector. With a live parent `main_session_id`
/// the fork BRANCHES the build conversation read-only ([`resume_fork_session_args`])
/// so the critic judges with everything the doer saw; with NO parent id (empty /
/// whitespace — no continuous session yet, the single-shot path, or an offline base)
/// it degrades to TODAY's FRESH independent read-only session ([`fork_session_args`]
/// pinned to `fresh_id`). Both shapes are read-only (plan mode + the read-only tool
/// allowlist), so the single-writer invariant holds either way. Deterministic —
/// exposed for tests.
#[must_use]
fn critic_fork_args(main_session_id: &str, fresh_id: &str) -> Vec<String> {
    if main_session_id.trim().is_empty() {
        fork_session_args(fresh_id)
    } else {
        resume_fork_session_args(main_session_id)
    }
}

/// Build the stream-json `user` message line for a phase directive. This is the
/// REAL wire shape (`{type:"user",message:{role,content},...}`) — the simplified
/// `{type:"user_message",message:"..."}` from some docs is wrong and claude
/// would reject it (and `exit(1)`). Exposed for tests.
#[must_use]
pub fn user_message_line(directive: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": directive },
        "parent_tool_use_id": Value::Null,
        "session_id": ""
    })
    .to_string()
}

/// Parse one stdout NDJSON line into zero or more [`SessionEvent`]s.
/// Fail-open: an unparseable / unknown line yields `vec![]` (skipped noise),
/// never an error or panic. Exposed for tests.
///
/// This is the STATELESS parse. The live pump wraps it in a [`SubagentGrouper`],
/// which buffers a nested sub-agent's frames into one grouped block; main-line
/// frames yield exactly what this function yields (locked by the equality tests).
#[must_use]
pub fn parse_stdout_line(line: &str) -> Vec<SessionEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return vec![]; // not JSON (a stray log line) → skip
    };
    parse_frame(&v)
}

/// The frame-level dispatch behind [`parse_stdout_line`]: one parsed stream-json
/// frame → zero or more [`SessionEvent`]s. Split out so the stateful
/// [`SubagentGrouper`] can reuse the EXACT same dispatch for main-line frames
/// (byte-for-byte parity) without re-serializing. Fail-open: an unknown frame
/// type yields `vec![]`, never an error or panic.
#[must_use]
fn parse_frame(v: &Value) -> Vec<SessionEvent> {
    match v.get("type").and_then(Value::as_str) {
        // Incremental text deltas (we launch with `--include-partial-messages`), so
        // a reply streams token-by-token instead of arriving all at once.
        Some("stream_event") => parse_stream_event(v),
        // The tool-noise frames (a base `Agent`/`Task` spawns a NESTED sub-agent
        // whose `tool_use` / `tool_result` blocks otherwise masquerade as the main
        // agent's output — the file-tree garble). `attribute_if_subagent` is PURELY
        // additive: a MAIN-line frame (no / null `parent_tool_use_id`) returns the
        // parser's output UNCHANGED; only a genuine sub-agent frame gets its tool
        // events visually attributed. See `attribute_if_subagent`. On the live pump
        // sub-agent frames are normally intercepted by the [`SubagentGrouper`]
        // BEFORE reaching this dispatch — this per-event attribution remains as the
        // fallback for any sub-agent frame that bypasses the buffer, so leakage can
        // never regress to unattributed.
        Some("assistant") => attribute_if_subagent(v, parse_assistant(v)),
        Some("user") => attribute_if_subagent(v, parse_user_tool_results(v)),
        Some("result") => vec![parse_result(v)],
        Some("control_request") => parse_control_request(v),
        // Item 2 — observability: an inbound `control_response` (claude's ACK to our
        // `interrupt` / other control acks) and the session `system`/init frame used
        // to fall through the `_ => vec![]` arm and be silently dropped. Surface them
        // to the tracing log so they're OBSERVABLE, but emit NO `SessionEvent` — the
        // control FLOW (`can_use_tool` → `NeedApproval` → `respond`) is untouched; these
        // still produce zero events. Fail-open: the describers never panic on a
        // malformed frame.
        Some("control_response") => {
            tracing::debug!(
                control = %describe_control_response(v),
                "inbound base control ack (no event)"
            );
            vec![]
        }
        Some("system") => {
            tracing::debug!(
                system = %describe_system_event(v),
                "inbound base system message"
            );
            // The session `init` frame carries the EXACT model claude resolved for
            // this session (e.g. `claude-sonnet-4-5-20250929`). Surface it ONCE as a
            // `SessionModel` event so the TUI can display the real driving model;
            // context-window capacity still requires explicit base/provider config.
            // the control flow is untouched (still no event for any other system
            // frame). Fail-open: a missing / non-string / empty `model`, or any
            // non-init system frame, yields no event exactly as before.
            if v.get("subtype").and_then(Value::as_str) == Some("init") {
                if let Some(model) = v
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                {
                    return vec![SessionEvent::SessionModel(model.to_string())];
                }
            }
            // Background sub-agent lifecycle frames (`task_started` /
            // `task_notification` / `background_tasks_changed`) — surfaced so the
            // orchestrator can refuse to settle a turn as "done" while the base's
            // OWN background agents are still running (the premature-final-report
            // fix). Fail-open: a non-task system frame yields no event, as before.
            if let Some(ev) = background_task_event(v) {
                return vec![ev];
            }
            vec![]
        }
        // keep_alive, status, tool_progress, … → not events.
        _ => vec![],
    }
}

/// Compact sub-agent attribution marker prefixed onto a NESTED sub-agent's rendered
/// tool-call name / tool-result summary. `↳` is an ASCII-art arrow (U+21B3) and the
/// label is plain CJK text — deliberately NO emoji (repo rule: emoji are never used
/// as functional markers). Applied ONLY to sub-agent frames so their nested tool
/// noise (e.g. a directory Read's file tree) is attributed to the sub-agent instead
/// of masquerading as the main agent's output.
const SUBAGENT_MARKER: &str = "↳ 子代理 · ";

/// The `parent_tool_use_id` of a stream-json frame, read at the SAME top level
/// UmaDev sets it OUTBOUND ([`user_message_line`] — `{…,"parent_tool_use_id":…}`).
/// claude tags every frame a NESTED sub-agent produces (its `Agent`/`Task` tool
/// spawns the sub-agent) with a non-null id here; a MAIN-line frame carries `null`
/// or omits the field. Returns `Some(id)` ONLY for a non-empty string — `null`,
/// absent, or an empty string all yield `None`. This is the single gate for the
/// additive sub-agent branch: a frame that is NOT a sub-agent frame can never enter
/// it, so main-line behavior is provably unchanged. Exposed for tests.
#[must_use]
fn parent_tool_use_id(v: &Value) -> Option<&str> {
    v.get("parent_tool_use_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Attribute `events` to a sub-agent IFF the frame `v` carries a non-null
/// [`parent_tool_use_id`]. **Purely additive:** a MAIN-line frame (`None`) returns
/// `events` byte-for-byte unchanged — the exact events UmaDev produced before this
/// fix; a sub-agent frame (`Some`) routes them through [`mark_subagent_events`].
/// This is the only place the two lines diverge. Exposed for tests.
#[must_use]
fn attribute_if_subagent(v: &Value, events: Vec<SessionEvent>) -> Vec<SessionEvent> {
    match parent_tool_use_id(v) {
        Some(_) => mark_subagent_events(events),
        None => events,
    }
}

/// Prefix each `ToolCall` name / `ToolResult` summary with [`SUBAGENT_MARKER`] so the
/// nested tool row is visually distinguishable from the main agent's. Non-tool events
/// (text / thinking deltas, turn boundaries) pass through UNCHANGED — sub-agent text
/// is left as-is rather than prefixed per-token (that would be noise). Exposed for
/// tests.
#[must_use]
fn mark_subagent_events(events: Vec<SessionEvent>) -> Vec<SessionEvent> {
    events
        .into_iter()
        .map(|ev| match ev {
            SessionEvent::ToolCall { name, input } => SessionEvent::ToolCall {
                name: format!("{SUBAGENT_MARKER}{name}"),
                input,
            },
            SessionEvent::ToolResult { ok, summary } => SessionEvent::ToolResult {
                ok,
                summary: format!("{SUBAGENT_MARKER}{summary}"),
            },
            other => other,
        })
        .collect()
}

/// Cap on the bytes one sub-agent buffer may hold before an EARLY partial flush
/// (a fail-open backstop): a huge exploration can neither hold memory unbounded
/// nor keep the transcript silent for its whole run. On exceed the held content
/// flushes as a grouped block carrying [`SUBAGENT_EARLY_FLUSH_NOTE`] and the
/// buffer stays open, so later output keeps grouping — nothing is ever dropped.
const SUBAGENT_BUFFER_CAP_BYTES: usize = 32 * 1024;

/// Bound on the remembered spawn-label map (`tool_use` id → sub-agent label), so
/// a very long turn cannot grow it unbounded. Oldest entries are evicted first;
/// a missed label degrades to the plain marker header, never an error.
const SUBAGENT_LABELS_CAP: usize = 128;

/// Suffix on the ONE lightweight "working" row yielded when a sub-agent buffer
/// OPENS, so the spawn is visible immediately while its output is grouped.
/// Hardcoded CJK next to [`SUBAGENT_MARKER`] by the same convention (driver-level
/// attribution text; no emoji).
const SUBAGENT_WORKING: &str = "工作中…";

/// Note appended when the byte cap forces an early partial flush, telling the
/// reader the block was cut here and the rest keeps grouping. Appended AFTER the
/// preview cap so it always survives truncation.
const SUBAGENT_EARLY_FLUSH_NOTE: &str = "[注:子代理输出较长,已先行刷出,其余继续汇总]";

/// Inline flag appended to a FAILED nested tool row in the compacted block.
const SUBAGENT_ROW_FAILED: &str = "(失败)";

/// Per-row cap (chars) for one compacted `name(target) → summary` line, so a
/// single chatty tool result cannot dominate the grouped block.
const SUBAGENT_ROW_CAP: usize = 160;

/// One captured sub-agent event, held in a [`SubagentBuffer`] until it flushes.
enum SubagentEntry {
    /// Assistant text (a sub-agent's `stream_event` text deltas — the orphan
    /// fragments of the interleaving bug). Concatenated at render time.
    Text(String),
    /// Extended-thinking reasoning. Captured so it can never leak into the main
    /// transcript; re-emitted at flush as ONE `ThinkingDelta`, which joins the
    /// collapsed `[thinking]` block exactly like main-line reasoning.
    Thinking(String),
    /// A nested tool call → rendered as a `name(target)` row, completed by the
    /// next `Result` into `name(target) → summary`.
    Call {
        /// Tool id (`Read`, `Grep`, …).
        name: String,
        /// Short human target ([`summarize_input`]: file path / command / …).
        target: String,
    },
    /// A nested tool result → completes the pending call row.
    Result {
        /// Whether the nested tool call succeeded.
        ok: bool,
        /// Truncated result preview (already capped by [`summarize_tool_content`]).
        summary: String,
    },
}

impl SubagentEntry {
    /// Approximate held bytes, for the [`SUBAGENT_BUFFER_CAP_BYTES`] backstop.
    fn cost(&self) -> usize {
        match self {
            Self::Text(t) | Self::Thinking(t) => t.len(),
            Self::Call { name, target } => name.len() + target.len(),
            Self::Result { summary, .. } => summary.len(),
        }
    }
}

/// One sub-agent's held output while its spawn is in flight, keyed by the
/// spawning `tool_use` id (the `parent_tool_use_id` every nested frame carries).
struct SubagentBuffer {
    /// The spawning `tool_use` id — the buffer key AND the terminal-signal match
    /// (the main-line `tool_result` answering it / a terminal `task_notification`
    /// whose `task_id` equals it).
    id: String,
    /// Human header label (`subagent_type` / task description / tool name),
    /// resolved from the spawning `tool_use` block when it was seen; empty when
    /// unknown (degrades to the plain marker header).
    label: String,
    /// Captured events in arrival order.
    entries: Vec<SubagentEntry>,
    /// Approximate held bytes (see [`SUBAGENT_BUFFER_CAP_BYTES`]).
    bytes: usize,
}

/// Stateful de-interleaver for a base's NESTED sub-agents (`Agent`/`Task`) — the
/// fix for sub-agent streamed output interleaving fragmentarily with the main
/// agent's transcript (orphan text deltas + tool-result chunks as bare main-line
/// bullets).
///
/// Contract:
/// - **Main-line frames** (no / null `parent_tool_use_id`) yield byte-for-byte
///   the events [`parse_stdout_line`] yields — locked by the equality tests.
/// - **Sub-agent frames**: `TextDelta` / `ThinkingDelta` / `ToolCall` /
///   `ToolResult` are CAPTURED into a per-sub-agent buffer instead of being
///   yielded; the buffer flushes as ONE grouped block (header row + one compacted
///   `ToolResult`) when its terminating signal arrives — the main-line
///   `tool_result` answering the spawning `tool_use` id (sync sub-agents) or a
///   terminal `task_notification` (background ones). Fail-open backstops: the
///   byte cap forces an early partial flush, and the turn boundary (`TurnDone`) /
///   stream EOF flush everything still held BEFORE the terminal event — nothing
///   is ever silently dropped.
/// - When a buffer OPENS, ONE lightweight attributed "working" row is yielded so
///   the spawn stays visible while output is grouped.
/// - Any sub-agent event that is NOT bufferable (an approval request, a turn
///   boundary, a non-bufferable frame type) passes through IMMEDIATELY with the
///   existing per-event attribution ([`mark_subagent_events`]) — buffering can
///   never hold a control-flow event (that would deadlock the approval loop) and
///   can never regress to unattributed leakage.
#[derive(Default)]
struct SubagentGrouper {
    /// Open buffers in spawn order (linear scan — a turn has few sub-agents).
    buffers: Vec<SubagentBuffer>,
    /// Recent `tool_use` id → label, bounded by [`SUBAGENT_LABELS_CAP`].
    labels: std::collections::VecDeque<(String, String)>,
}

impl SubagentGrouper {
    /// One raw stdout NDJSON line → the events to yield NOW (possibly empty while
    /// a sub-agent's output is being held). Fail-open exactly like
    /// [`parse_stdout_line`]: a non-JSON / empty line yields nothing.
    fn on_line(&mut self, line: &str) -> Vec<SessionEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return vec![];
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            return vec![]; // not JSON (a stray log line) → skip, same as the pure parse
        };
        match parent_tool_use_id(&v).map(str::to_string) {
            Some(pid) => self.capture_frame(&pid, &v),
            None => self.main_line_frame(&v),
        }
    }

    /// A MAIN-line frame: yield exactly what [`parse_frame`] yields, PLUS any
    /// grouped-block flushes its content triggers (a sub-agent's terminating
    /// signal / the turn boundary), emitted BEFORE the main-line events so the
    /// transcript reads "grouped sub-agent block → its final report / turn end".
    fn main_line_frame(&mut self, v: &Value) -> Vec<SessionEvent> {
        let events = parse_frame(v);
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str) {
            // Remember spawn labels (`Agent`/`Task` `tool_use` blocks) so a later
            // buffer can name its sub-agent.
            Some("assistant") => self.record_spawn_labels(v),
            // Sync terminal: the main-line `tool_result` answering the spawning
            // `tool_use` id — that sub-agent is done; flush its grouped block
            // first, then the final report streams as before.
            Some("user") => {
                for id in tool_result_ids(v) {
                    out.extend(self.flush_buffer(&id));
                }
            }
            // Background terminal: a terminal `task_notification` (completed /
            // failed / stopped). Key-matched fail-open: an id that names no held
            // buffer flushes nothing (the turn boundary backstop still covers it).
            Some("system") => {
                if let Some(id) = terminal_task_id(v) {
                    out.extend(self.flush_buffer(id));
                }
            }
            _ => {}
        }
        // Turn boundary backstop: the turn is over — nothing may stay held, and
        // every grouped block must precede the `TurnDone` event.
        if events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnDone { .. }))
        {
            out.extend(self.flush_all());
        }
        out.extend(events);
        out
    }

    /// A SUB-AGENT frame (non-null `parent_tool_use_id` = `pid`): capture its
    /// bufferable events; pass anything else through with per-event attribution.
    fn capture_frame(&mut self, pid: &str, v: &Value) -> Vec<SessionEvent> {
        let events = match v.get("type").and_then(Value::as_str) {
            Some("stream_event") => parse_stream_event(v),
            Some("assistant") => {
                // A sub-agent can spawn its OWN nested sub-agent — remember those
                // labels too so the nested buffer gets a real header.
                self.record_spawn_labels(v);
                parse_assistant(v)
            }
            Some("user") => {
                // A NESTED sub-agent's terminating tool_result arrives inside its
                // PARENT sub-agent's frames — flush the nested buffer here so it
                // is not held until the turn boundary.
                let mut out = Vec::new();
                for id in tool_result_ids(v) {
                    out.extend(self.flush_buffer(&id));
                }
                out.extend(self.capture_events(pid, parse_user_tool_results(v)));
                return out;
            }
            // Not a bufferable producer (`result` / `control_request` / `system` /
            // unknown) → exactly today's path: the shared dispatch, whose
            // sub-agent arms apply the per-event marker fallback.
            _ => return parse_frame(v),
        };
        self.capture_events(pid, events)
    }

    /// Route parsed sub-agent events into the `pid` buffer. Only the four
    /// transcript kinds are held; any other event (an approval request, a turn
    /// boundary, a background-task signal) passes through IMMEDIATELY — holding
    /// one would deadlock control flow — with the per-event attribution fallback.
    fn capture_events(&mut self, pid: &str, events: Vec<SessionEvent>) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        for ev in events {
            let entry = match ev {
                SessionEvent::TextDelta(t) => SubagentEntry::Text(t),
                SessionEvent::ThinkingDelta(t) => SubagentEntry::Thinking(t),
                SessionEvent::ToolCall { name, input } => SubagentEntry::Call {
                    target: summarize_input(&input),
                    name,
                },
                SessionEvent::ToolResult { ok, summary } => SubagentEntry::Result { ok, summary },
                other => {
                    out.extend(mark_subagent_events(vec![other]));
                    continue;
                }
            };
            out.extend(self.push_entry(pid, entry));
        }
        out
    }

    /// Append one entry to the `pid` buffer, opening it (and yielding the ONE
    /// visible "working" row) on first use, and early-flushing on the byte cap.
    fn push_entry(&mut self, pid: &str, entry: SubagentEntry) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        let pos = if let Some(p) = self.buffers.iter().position(|b| b.id == pid) {
            p
        } else {
            let label = self.label_for(pid);
            out.push(SessionEvent::ToolCall {
                name: subagent_working_row(&label),
                input: Value::Null,
            });
            self.buffers.push(SubagentBuffer {
                id: pid.to_string(),
                label,
                entries: Vec::new(),
                bytes: 0,
            });
            self.buffers.len() - 1
        };
        let buf = &mut self.buffers[pos];
        buf.bytes = buf.bytes.saturating_add(entry.cost());
        buf.entries.push(entry);
        if buf.bytes > SUBAGENT_BUFFER_CAP_BYTES {
            // Early partial flush: emit what is held (with the continuation note)
            // and keep the buffer OPEN so later output keeps grouping. Also acts
            // as a periodic liveness signal during a very chatty sub-agent run.
            let held = std::mem::take(&mut buf.entries);
            buf.bytes = 0;
            out.extend(render_subagent_flush(&buf.label, &held, true));
        }
        out
    }

    /// Flush ONE buffer (its terminating signal arrived) as a grouped block.
    /// Unknown id / nothing held → no events (fail-open).
    fn flush_buffer(&mut self, id: &str) -> Vec<SessionEvent> {
        match self.buffers.iter().position(|b| b.id == id) {
            Some(p) => {
                let buf = self.buffers.remove(p);
                render_subagent_flush(&buf.label, &buf.entries, false)
            }
            None => vec![],
        }
    }

    /// Flush EVERY held buffer (turn boundary / stream EOF) — the backstop that
    /// guarantees nothing a sub-agent produced is ever silently dropped.
    fn flush_all(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.buffers)
            .into_iter()
            .flat_map(|b| render_subagent_flush(&b.label, &b.entries, false))
            .collect()
    }

    /// The remembered label for a spawning `tool_use` id (newest wins); empty
    /// when the spawn frame was never seen (degrades to the plain header).
    fn label_for(&self, pid: &str) -> String {
        self.labels
            .iter()
            .rev()
            .find(|(id, _)| id == pid)
            .map(|(_, l)| l.clone())
            .unwrap_or_default()
    }

    /// Remember `tool_use` id → human label for every tool call in an assistant
    /// frame, so a buffer opened by that id can name its sub-agent. Label
    /// preference: `input.subagent_type` (e.g. `Explore`) → `input.description`
    /// (the short task summary) → the tool name. Bounded FIFO eviction.
    fn record_spawn_labels(&mut self, v: &Value) {
        let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for b in blocks {
            if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(id) = b
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let input = b.get("input");
            let label = input
                .and_then(|i| i.get("subagent_type"))
                .and_then(Value::as_str)
                .or_else(|| {
                    input
                        .and_then(|i| i.get("description"))
                        .and_then(Value::as_str)
                })
                .or_else(|| b.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .trim();
            self.labels.push_back((id.to_string(), truncate(label, 60)));
            while self.labels.len() > SUBAGENT_LABELS_CAP {
                self.labels.pop_front();
            }
        }
    }
}

/// The `tool_use_id`s of every `tool_result` block in a `user` frame — the sync
/// terminating signals a grouped buffer matches against. Fail-open: a malformed
/// frame yields an empty list.
fn tool_result_ids(v: &Value) -> Vec<String> {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The `task_id` of a TERMINAL `task_notification` system frame (completed /
/// failed / stopped — the background sub-agent's terminating signal), or `None`
/// for any other frame. Mirrors [`background_task_event`]'s terminal test.
fn terminal_task_id(v: &Value) -> Option<&str> {
    if v.get("subtype").and_then(Value::as_str) != Some("task_notification") {
        return None;
    }
    let status = v.get("status").and_then(Value::as_str).unwrap_or("");
    if status == "running" || status == "pending" {
        return None; // not terminal — the task is still live
    }
    v.get("task_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The ONE visible row yielded when a sub-agent buffer opens: marker + label +
/// "working", so the user sees the spawn immediately while output is grouped.
fn subagent_working_row(label: &str) -> String {
    if label.is_empty() {
        format!("{SUBAGENT_MARKER}{SUBAGENT_WORKING}")
    } else {
        format!("{SUBAGENT_MARKER}{label} · {SUBAGENT_WORKING}")
    }
}

/// The grouped block's header-row name: marker + label, or the bare marker stem
/// (`↳ 子代理`) when the spawn label was never seen.
fn subagent_header_row(label: &str) -> String {
    if label.is_empty() {
        SUBAGENT_MARKER
            .trim_end()
            .trim_end_matches('·')
            .trim_end()
            .to_string()
    } else {
        format!("{SUBAGENT_MARKER}{label}")
    }
}

/// Render one buffer's held entries as the grouped block: a header `ToolCall`
/// row, the captured reasoning as ONE `ThinkingDelta` (joins the collapsed
/// thinking channel — never the transcript), and ONE `ToolResult` whose summary
/// is the compacted content ([`render_subagent_body`]) bounded by the same
/// preview-cap conventions as any tool result ([`crate::process_logs`] — the
/// process-logs verbose toggle widens it). `early` appends
/// [`SUBAGENT_EARLY_FLUSH_NOTE`] AFTER the cap so it always survives. Nothing
/// held → no events.
fn render_subagent_flush(label: &str, entries: &[SubagentEntry], early: bool) -> Vec<SessionEvent> {
    if entries.is_empty() {
        return vec![];
    }
    let on = crate::process_logs::show_process_logs();
    let cap = crate::process_logs::cap_for(on);
    let mut events = vec![SessionEvent::ToolCall {
        name: subagent_header_row(label),
        input: Value::Null,
    }];
    let thinking: String = entries
        .iter()
        .filter_map(|e| match e {
            SubagentEntry::Thinking(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    if !thinking.is_empty() {
        events.push(SessionEvent::ThinkingDelta(
            crate::process_logs::truncate_preview(&thinking, cap, on),
        ));
    }
    let (ok, body) = render_subagent_body(entries);
    let mut summary = crate::process_logs::truncate_preview(&body, cap, on);
    if early {
        summary.push('\n');
        summary.push_str(SUBAGENT_EARLY_FLUSH_NOTE);
    }
    events.push(SessionEvent::ToolResult {
        ok,
        summary: format!("{SUBAGENT_MARKER}{summary}"),
    });
    events
}

/// Compact one buffer's entries into the grouped block body: text runs
/// concatenated, tool rows as `name(target) → summary` lines (per-row capped),
/// failed rows flagged inline. The block's `ok` reflects how the sub-agent
/// ENDED (its LAST tool result) — a single failed probe mid-exploration does not
/// paint the whole block as failed; the authoritative verdict is the main-line
/// `tool_result` that follows it. Thinking entries are handled separately (see
/// [`render_subagent_flush`]).
fn render_subagent_body(entries: &[SubagentEntry]) -> (bool, String) {
    let mut lines: Vec<String> = Vec::new();
    let mut text_run = String::new();
    let mut pending_call: Option<String> = None;
    for e in entries {
        match e {
            SubagentEntry::Text(t) => {
                if let Some(call) = pending_call.take() {
                    lines.push(call);
                }
                text_run.push_str(t);
            }
            SubagentEntry::Thinking(_) => {}
            SubagentEntry::Call { name, target } => {
                push_text_run(&mut lines, &mut text_run);
                if let Some(call) = pending_call.take() {
                    lines.push(call);
                }
                pending_call = Some(if target.is_empty() {
                    name.clone()
                } else {
                    format!("{name}({})", truncate(target, 80))
                });
            }
            SubagentEntry::Result { ok, summary } => {
                push_text_run(&mut lines, &mut text_run);
                let s = truncate(first_line(summary), SUBAGENT_ROW_CAP);
                let mut line = match pending_call.take() {
                    Some(call) => format!("{call} → {s}"),
                    None => format!("→ {s}"),
                };
                if !ok {
                    line.push_str(SUBAGENT_ROW_FAILED);
                }
                lines.push(line);
            }
        }
    }
    push_text_run(&mut lines, &mut text_run);
    if let Some(call) = pending_call.take() {
        lines.push(call);
    }
    let ended_ok = !matches!(
        entries
            .iter()
            .rev()
            .find(|e| matches!(e, SubagentEntry::Result { .. })),
        Some(SubagentEntry::Result { ok: false, .. })
    );
    (ended_ok, lines.join("\n"))
}

/// Push the accumulated text run (trimmed) as one body block, then clear it.
fn push_text_run(lines: &mut Vec<String>, run: &mut String) {
    let t = run.trim();
    if !t.is_empty() {
        lines.push(t.to_string());
    }
    run.clear();
}

/// The first non-empty rendering line of a (possibly multiline) tool summary.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// A short, fail-open one-line description of an inbound `control_response` (claude's
/// ACK to a `control_request` we sent — e.g. the reply to our `interrupt`) for the
/// tracing log. Reads the ack `subtype` + the `request_id` it answers, tolerating
/// both the nested (`response.request_id`) and top-level shapes. NEVER panics: a
/// missing / wrong-typed field degrades to `"?"`. Pure; drives NO control flow.
/// Exposed for tests.
#[must_use]
fn describe_control_response(v: &Value) -> String {
    let resp = v.get("response");
    let subtype = resp
        .and_then(|r| r.get("subtype"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let request_id = resp
        .and_then(|r| r.get("request_id"))
        .and_then(Value::as_str)
        .or_else(|| v.get("request_id").and_then(Value::as_str))
        .unwrap_or("?");
    format!("subtype={subtype} request_id={request_id}")
}

/// A short, fail-open one-line description of an inbound `system` frame (claude's
/// session `init` + status messages) for the tracing log. Reads the `subtype` and,
/// when present, the `session_id`. NEVER panics on a malformed frame. Pure; produces
/// NO `SessionEvent` (kept off the event stream exactly as before). Exposed for tests.
#[must_use]
fn describe_system_event(v: &Value) -> String {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("?");
    let session = v.get("session_id").and_then(Value::as_str).unwrap_or("");
    if session.is_empty() {
        format!("subtype={subtype}")
    } else {
        format!("subtype={subtype} session_id={session}")
    }
}

/// Whether a claude background-task type string names a SUB-AGENT (vs a
/// background shell / teammate). claude's task-type vocabulary:
/// `local_agent` / `remote_agent` / `agent` are sub-agents; `bash` /
/// `local_bash` (background shells), `local_workflow`, `in_process_teammate`
/// are not. A shell must never be counted as an outstanding agent — a dev
/// server the base deliberately leaves running would otherwise wedge every
/// settle. Conservative: an unknown type is NOT an agent (fail-open toward
/// never over-waiting).
fn task_type_is_agent(task_type: &str, subagent_type: &str) -> bool {
    task_type.contains("agent") || !subagent_type.trim().is_empty()
}

/// Translate one `system` background-task frame into a
/// [`SessionEvent::BackgroundTask`], or `None` for any other system frame.
///
/// Ground truth (claude 2.1.x stream-json):
/// - `{"type":"system","subtype":"task_started","task_id":…,"task_type":…,
///   "subagent_type":…}` — a background task started. Surfaced ONLY when the
///   task is a sub-agent ([`task_type_is_agent`]).
/// - `{"type":"system","subtype":"task_notification","task_id":…,"status":…}`
///   — a task reached a state; `completed` / `failed` / `stopped` are
///   terminal → `Finished`. A non-terminal `running` / `pending` yields no
///   event.
/// - `{"type":"system","subtype":"background_tasks_changed","tasks":
///   [{"task_id":…,"task_type":…},…]}` — the LEVEL signal: the full live set,
///   filtered here to sub-agents. Claude's own contract says consumers should
///   REPLACE their set with each payload so a missed edge can't wedge a stale
///   count.
///
/// Fail-open: a missing / non-string `task_id`, an unknown subtype, or any
/// malformed payload yields `None` — never a panic. Exposed for tests.
#[must_use]
fn background_task_event(v: &Value) -> Option<SessionEvent> {
    let subtype = v.get("subtype").and_then(Value::as_str)?;
    match subtype {
        "task_started" => {
            let id = v
                .get("task_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let task_type = v.get("task_type").and_then(Value::as_str).unwrap_or("");
            let subagent = v.get("subagent_type").and_then(Value::as_str).unwrap_or("");
            if !task_type_is_agent(task_type, subagent) {
                return None; // a background shell / workflow — never waited on
            }
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started { id: id.to_string() },
            ))
        }
        "task_notification" => {
            let id = v
                .get("task_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let status = v.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "running" || status == "pending" {
                return None; // not terminal — the task is still live
            }
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Finished { id: id.to_string() },
            ))
        }
        "background_tasks_changed" => {
            let tasks = v.get("tasks").and_then(Value::as_array)?;
            let agent_ids = tasks
                .iter()
                .filter(|t| {
                    task_type_is_agent(
                        t.get("task_type").and_then(Value::as_str).unwrap_or(""),
                        t.get("subagent_type").and_then(Value::as_str).unwrap_or(""),
                    )
                })
                .filter_map(|t| t.get("task_id").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            Some(SessionEvent::BackgroundTask(BackgroundTaskSignal::Live {
                agent_ids,
            }))
        }
        _ => None,
    }
}

/// A `stream_event` frame (present with `--include-partial-messages`) → a
/// `TextDelta` for each `content_block_delta` carrying a `text_delta`, OR a
/// `ThinkingDelta` for a `thinking_delta` (the base's extended-thinking
/// reasoning, surfaced as a collapsed `[thinking]` block in the TUI). Tool-arg
/// (`input_json_delta`) / `signature_delta` deltas and the start/stop frames are
/// ignored — tool calls are surfaced from the final aggregate `assistant` block.
fn parse_stream_event(v: &Value) -> Vec<SessionEvent> {
    let Some(event) = v.get("event") else {
        return vec![];
    };
    if event.get("type").and_then(Value::as_str) != Some("content_block_delta") {
        return vec![];
    }
    let delta = event.get("delta");
    match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
        Some("text_delta") => delta
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| vec![SessionEvent::TextDelta(t.to_string())])
            .unwrap_or_default(),
        // Extended-thinking reasoning chunk: the text lives under `thinking`.
        // Routed to the collapsed `[thinking]` block, NOT the answer stream.
        Some("thinking_delta") => delta
            .and_then(|d| d.get("thinking"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| vec![SessionEvent::ThinkingDelta(t.to_string())])
            .unwrap_or_default(),
        _ => vec![], // input_json_delta / signature_delta → not displayed
    }
}

/// Assistant content blocks → text deltas + tool calls.
fn parse_assistant(v: &Value) -> Vec<SessionEvent> {
    let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
        return vec![];
    };
    // A plain-string content would be the WHOLE buffered text — skip it: the text
    // already streamed via `stream_event` text deltas, so re-emitting it here would
    // double the reply.
    if content.is_string() {
        return vec![];
    }
    content
        .as_array()
        .map(|blocks| blocks.iter().filter_map(block_to_event).collect())
        .unwrap_or_default()
}

/// One assistant content block → a tool-call event, or `None`. TEXT blocks are
/// intentionally skipped: with `--include-partial-messages` the text already
/// arrived as `stream_event` `TextDelta`s, so emitting the final aggregate text
/// block here would double the reply. Only tool calls (which we read from the
/// assembled block) are surfaced.
fn block_to_event(block: &Value) -> Option<SessionEvent> {
    match block.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            Some(SessionEvent::ToolCall { name, input })
        }
        _ => None,
    }
}

/// `user` messages carrying tool_result blocks → ToolResult events.
fn parse_user_tool_results(v: &Value) -> Vec<SessionEvent> {
    let Some(blocks) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return vec![];
    };
    blocks.iter().filter_map(tool_result_event).collect()
}

/// One block → a ToolResult event if it is a tool_result, else `None`.
fn tool_result_event(block: &Value) -> Option<SessionEvent> {
    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
        return None;
    }
    let ok = !block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(SessionEvent::ToolResult {
        ok,
        summary: summarize_tool_content(block.get("content")),
    })
}

/// A `result` envelope → the turn-done boundary.
///
/// claude flags an errored turn with `is_error: true` and writes the human-facing
/// cause into the `result` string (e.g. `"API Error: Request rejected (429) · You
/// have exceeded the 5-hour usage quota …"`). A mid-turn API error commonly arrives
/// as `{"subtype":"success","is_error":true,"result":"API Error: …"}` — so keying
/// the status off `subtype` ALONE mapped that to [`TurnStatus::Completed`], and the
/// turn read as a silent, empty success (the "完成 / 本轮无文件变更" swallow) while
/// the real cause never reached the user. We therefore honor `is_error`: a clean
/// finish is `subtype:"success"` AND not flagged as an error; anything flagged (or
/// an explicit error subtype) becomes a [`TurnStatus::Failed`] carrying the base's
/// OWN error text. The soft caps (`error_max_*`) stay [`TurnStatus::Truncated`] —
/// the turn hit a turn/budget ceiling, not an API failure, so we accept what landed.
fn parse_result(v: &Value) -> SessionEvent {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let status = match subtype {
        // A clean finish: success AND not flagged as an error.
        "success" if !is_error => TurnStatus::Completed,
        // Soft caps — partial work, accept it (the deterministic floor downstream
        // is the real stop). claude flags these `is_error:true`, so this arm MUST
        // come before the generic error fall-through below.
        "error_max_turns" | "error_max_budget_usd" | "error_max_structured_output_retries" => {
            TurnStatus::Truncated
        }
        // Either an explicit error subtype, OR `success` with `is_error:true` — a
        // real failure. Carry the base's actual error text (the 429 / auth /
        // overloaded message), never swallow it as a clean completion.
        other => TurnStatus::Failed(result_error_text(v, other)),
    };
    // F3: surface the REAL per-turn token usage off the `result` line so `/usage`
    // is truthful on the DEFAULT continuous loop (claude reports it; previously
    // only the legacy single-shot `claude.rs` path read it). Fail-open: a result
    // line with no `usage` object yields `None` → the consumer estimates instead.
    SessionEvent::TurnDone {
        status,
        usage: parse_result_usage(v),
    }
}

/// The human-readable error text off an errored `result` envelope. Prefers the
/// base's own `result` string (where claude writes the API error, e.g. "API Error:
/// Request rejected (429) …") so the user sees the REAL cause; falls back to naming
/// the `subtype` when no message text is present. Never empty → never a silent
/// failure.
fn result_error_text(v: &Value, subtype: &str) -> String {
    v.get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                if subtype.is_empty() {
                    "base error".to_string()
                } else {
                    format!("base error ({subtype})")
                }
            },
            str::to_string,
        )
}

/// Extract the per-turn token usage from a stream-json `result` envelope.
///
/// The `{"type":"result", "usage":{"input_tokens":…, "output_tokens":…,
/// "cache_read_input_tokens":…, "cache_creation_input_tokens":…}, …}` line carries
/// real usage. We fold cache reads/writes into input (they ARE consumed input) so
/// the count matches the legacy single-shot driver ([`crate::claude`]'s
/// `extract_usage`). Returns `None` (→ estimate) when no `usage` object is present.
fn parse_result_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?;
    let field = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let input = field("input_tokens")
        + field("cache_read_input_tokens")
        + field("cache_creation_input_tokens");
    let output = field("output_tokens");
    Some(Usage {
        input_tokens: u32::try_from(input).unwrap_or(u32::MAX),
        output_tokens: u32::try_from(output).unwrap_or(u32::MAX),
    })
}

/// A `control_request{can_use_tool}` → a NeedApproval the orchestrator answers.
fn parse_control_request(v: &Value) -> Vec<SessionEvent> {
    let req = v.get("request");
    if req.and_then(|r| r.get("subtype")).and_then(Value::as_str) != Some("can_use_tool") {
        return vec![]; // interrupt acks etc. — not an approval prompt
    }
    let req_id = v
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let action = req
        .and_then(|r| r.get("tool_name"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let target = req
        .and_then(|r| r.get("input"))
        .map(summarize_input)
        .unwrap_or_default();
    vec![SessionEvent::NeedApproval {
        req_id,
        action,
        target,
    }]
}

/// Truncated preview of a tool_result `content` (string or block array). The cap
/// widens to the full captured output when the user opts into process logs
/// (`UMADEV_SHOW_PROCESS_LOGS`), so a long-running command's build log reaches the
/// transcript instead of a 200-char clip; OFF (the default) keeps the tight clip.
fn summarize_tool_content(content: Option<&Value>) -> String {
    let raw = match content {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    // Direction follows the path: verbose (process logs ON) keeps the TAIL so a long
    // build's failure verdict at the END survives instead of being clipped; OFF
    // keeps the tight head clip (a summary/preview), unchanged.
    let on = crate::process_logs::show_process_logs();
    crate::process_logs::truncate_preview(&raw, crate::process_logs::cap_for(on), on)
}

/// A short, human-readable target for an approval prompt (file path / command).
/// Includes `plan` so an `ExitPlanMode` approval shows the proposed plan text
/// instead of a bare "ExitPlanMode" / truncated JSON blob.
fn summarize_input(input: &Value) -> String {
    for key in ["file_path", "path", "command", "pattern", "url", "plan"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    truncate(&input.to_string(), 120)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// A fresh UUID-v4 session id (pure: nanos + counter + pid, avalanched — no
/// `uuid` dependency), matching the format claude's `--session-id` expects.
fn new_session_id() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that MUTATE `UMADEV_CLAUDE_PERMISSION_MODE` against the
    /// ones that ASSERT the env-derived permission mode. The process env is global,
    /// so without this a concurrent test could observe another's mid-test `set_var`
    /// and read the wrong permission mode. Held only by the setter test and the
    /// env-dependent reader tests (others don't assert the derived value).
    static PERM_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvRestore {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn remove(key: &'static str) -> Self {
            let prior = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prior }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn user_message_line_is_valid_ndjson_user_shape() {
        let line = user_message_line("do the thing");
        assert!(!line.contains('\n'));
        let v: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "do the thing");
        assert!(v["parent_tool_use_id"].is_null());
    }

    #[test]
    fn session_args_use_append_not_replace_system_prompt() {
        let args = session_args("sid-1", Some("be terse"), true, None);
        assert!(args.contains(&"--input-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"sid-1".to_string()));
        assert!(args.contains(&"--append-system-prompt".to_string()));
        assert!(!args.contains(&"--system-prompt".to_string()));
        assert!(args.contains(&"be terse".to_string()));
    }

    #[test]
    fn divert_append_system_moves_large_firmware_off_the_command_line() {
        // A multi-KB firmware would overflow a Windows `.cmd` command line, so it is
        // written to a temp file and passed via `--append-system-prompt-file <path>`
        // instead of `--append-system-prompt <firmware>`.
        let firmware = "FIRMWARE-".repeat(1_000); // ~9 KB, distinctive
        let args = session_args("sid-x", Some(&firmware), true, None);
        assert!(args.contains(&"--append-system-prompt".to_string()));

        let dir = tempfile::TempDir::new().unwrap();
        let (out, guard) = divert_append_system_to_file_in(args, dir.path());
        let guard = guard.expect("a large firmware must be diverted to a file");

        // The flag flipped to the `-file` form, and the multi-KB firmware is NO LONGER
        // anywhere on the argv (the whole bug: it must leave the command line).
        assert!(out.contains(&"--append-system-prompt-file".to_string()));
        assert!(!out.contains(&"--append-system-prompt".to_string()));
        assert!(
            !out.iter().any(|a| a.contains("FIRMWARE-")),
            "the firmware text must not remain on the command line"
        );
        // The path is on the argv and the file holds the exact firmware.
        let path = guard.path.clone();
        assert!(out.iter().any(|a| a == &path.to_string_lossy()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), firmware);

        // Dropping the guard (as the session does on end) removes the temp file.
        drop(guard);
        assert!(
            !path.exists(),
            "the temp firmware file must be cleaned up on drop"
        );
    }

    #[test]
    fn divert_append_system_is_fail_open_on_write_error() {
        // A temp-write failure (here: a non-existent target dir) must fall back to the
        // inline `--append-system-prompt` arg UNCHANGED — never a crash, never a
        // silently-dropped firmware.
        let firmware = "x".repeat(9_000);
        let args = session_args("sid-y", Some(&firmware), true, None);
        let bad_dir = std::path::Path::new("/umadev-no-such-dir-1a2b3c/nested");
        let (out, guard) = divert_append_system_to_file_in(args.clone(), bad_dir);
        assert!(guard.is_none(), "a write error yields no guard");
        assert_eq!(
            out, args,
            "args stay unchanged (inline arg preserved) on failure"
        );
        assert!(out.contains(&"--append-system-prompt".to_string()));
        assert!(out.contains(&firmware));
    }

    #[test]
    fn maybe_divert_firmware_keeps_small_firmware_inline() {
        // A small firmware fits the command-line budget on every platform, so the fast
        // inline `--append-system-prompt` argv path is kept (no temp file).
        let args = session_args("sid-s", Some("be terse"), true, None);
        let (out, guard) = maybe_divert_firmware("claude", &[], &args);
        assert!(guard.is_none(), "small firmware must stay inline");
        assert_eq!(out, args);
        assert!(out.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn maybe_divert_firmware_diverts_oversized_firmware() {
        // An oversized firmware pushes the whole spawn line past the budget, so the
        // budget gate triggers the off-command-line diversion end to end.
        let firmware = "y".repeat(130_000); // over the non-Windows 120_000 backstop too
        let args = session_args("sid-o", Some(&firmware), true, None);
        let (out, guard) = maybe_divert_firmware("claude", &[], &args);
        assert!(
            guard.is_some(),
            "oversized firmware must be diverted to a file"
        );
        assert!(out.contains(&"--append-system-prompt-file".to_string()));
        assert!(!out.iter().any(|a| a.contains(&firmware)));
    }

    #[test]
    fn maybe_divert_firmware_ignores_forkless_args_without_firmware() {
        // A read-only critic fork's args carry NO `--append-system-prompt`, so even
        // over budget there is nothing to divert (fail-open no-op).
        let args = fork_session_args("fork-sid");
        let (out, guard) = maybe_divert_firmware("claude", &[], &args);
        assert!(guard.is_none());
        assert_eq!(out, args);
    }

    /// The permission mode tracks the autonomy tier (claude consistent with
    /// codex / opencode): autonomous → `acceptEdits` (write unattended), guarded
    /// → `default` (claude asks per tool → a NeedApproval the orchestrator
    /// answers, so the human-in-the-loop / irreversible-action floor is live).
    #[test]
    fn guarded_gates_mutating_tools_but_auto_pre_approves_all() {
        // P1: under GUARDED (autonomous=false) the allowlist pre-approves the read-only +
        // research + sub-agent set but NOT the MUTATING tools (Edit/Write/Bash/NotebookEdit),
        // so each mutation still raises a `can_use_tool` control request that UmaDev's trust
        // floor gates (the guarded gate must not be silently bypassed). The base's native
        // read/research/delegate tools (incl. Agent/Task sub-agents) ARE pre-approved so they
        // run natively instead of eating a per-tool pause. AUTO pre-approves the full set.
        let guarded = session_args("sid", None, false, None);
        let t = guarded.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(guarded[t + 1], GUARDED_ALLOWED_TOOLS);
        for mutating in ["Edit", "Write", "Bash", "NotebookEdit"] {
            assert!(
                !guarded[t + 1].split(',').any(|x| x == mutating),
                "guarded must NOT pre-approve the mutating tool {mutating} (it must hit the gate)"
            );
        }
        for native in ["Agent", "Task", "Grep", "Glob", "WebSearch"] {
            assert!(
                guarded[t + 1].split(',').any(|x| x == native),
                "guarded must pre-approve the read-only/delegate tool {native} so it runs natively"
            );
        }
        let auto = session_args("sid", None, true, None);
        let t = auto.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(auto[t + 1], AUTO_ALLOWED_TOOLS);
        // Auto pre-approves the mutating set too (the autonomy tier the user opted into).
        for tool in ["Edit", "Write", "Bash", "Agent", "Task"] {
            assert!(
                auto[t + 1].split(',').any(|x| x == tool),
                "auto must pre-approve {tool}"
            );
        }
    }

    #[test]
    fn session_args_permission_mode_tracks_autonomy() {
        // This test MUTATES the shared permission-mode env; serialize it against the
        // env-dependent reader tests so a concurrent read can't see a mid-test value.
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Guard against the env override leaking in from a sibling process.
        let _env = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");

        let auto = session_args("sid-a", None, true, None);
        let auto_idx = auto.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(auto[auto_idx + 1], "acceptEdits", "auto → acceptEdits");

        let guarded = session_args("sid-g", None, false, None);
        let g_idx = guarded
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(
            guarded[g_idx + 1],
            "default",
            "guarded → default (claude asks → NeedApproval, human in the loop)"
        );

        // The explicit override beats the derived default for the AUTONOMOUS tier.
        std::env::set_var("UMADEV_CLAUDE_PERMISSION_MODE", "plan");
        let overridden = session_args("sid-o", None, true, None);
        let o_idx = overridden
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(
            overridden[o_idx + 1],
            "plan",
            "env override wins (autonomous)"
        );

        // Guarded-tier awareness guard: a `plan` override on the GUARDED tier is
        // ignored so UmaDev's Guarded never silently enters the base's untracked
        // plan mode — it opens with the tracked `default` instead.
        let guarded_plan = session_args("sid-gp", None, false, None);
        let plan_pos = guarded_plan
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(
            guarded_plan[plan_pos + 1],
            "default",
            "guarded ignores a `plan` override (base plan mode is untracked in guarded)"
        );

        // A non-`plan` override still wins on the guarded tier (only `plan` is guarded).
        std::env::set_var("UMADEV_CLAUDE_PERMISSION_MODE", "acceptEdits");
        let guarded_accept = session_args("sid-ga", None, false, None);
        let accept_pos = guarded_accept
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(
            guarded_accept[accept_pos + 1],
            "acceptEdits",
            "a non-plan override is honored on the guarded tier"
        );
    }

    #[test]
    fn bypass_permissions_override_passes_through_on_both_tiers() {
        // Report-2 contract: a user-level full bypass is expressible as
        // `UMADEV_CLAUDE_PERMISSION_MODE=bypassPermissions` and must pass through
        // VERBATIM on both tiers (UmaDev's explicit `--permission-mode` otherwise
        // downgrades a claude-settings `defaultMode: bypassPermissions`).
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");
        std::env::set_var("UMADEV_CLAUDE_PERMISSION_MODE", "bypassPermissions");
        for autonomous in [true, false] {
            let args = session_args("sid-b", None, autonomous, None);
            let p = args.iter().position(|a| a == "--permission-mode").unwrap();
            assert_eq!(
                args[p + 1],
                "bypassPermissions",
                "bypassPermissions must pass through (autonomous={autonomous})"
            );
        }
    }

    #[test]
    fn background_task_frames_surface_as_background_task_events() {
        // task_started for an AGENT-typed task → Started.
        let started = parse_stdout_line(
            r#"{"type":"system","subtype":"task_started","task_id":"a1","task_type":"local_agent","description":"docs"}"#,
        );
        assert_eq!(
            started,
            vec![SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started {
                    id: "a1".to_string()
                }
            )]
        );
        // A background SHELL (a dev server) must NOT be surfaced — waiting on it
        // would wedge every settle.
        assert!(parse_stdout_line(
            r#"{"type":"system","subtype":"task_started","task_id":"b1","task_type":"local_bash"}"#,
        )
        .is_empty());
        // A subagent_type alone also marks an agent (older/newer shapes).
        assert_eq!(
            parse_stdout_line(
                r#"{"type":"system","subtype":"task_started","task_id":"a2","subagent_type":"Explore"}"#,
            ),
            vec![SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started {
                    id: "a2".to_string()
                }
            )]
        );
        // Terminal task_notification → Finished (for ANY id — removal from an
        // agents-only set is a harmless no-op for a shell id).
        for status in ["completed", "failed", "stopped"] {
            let line = format!(
                r#"{{"type":"system","subtype":"task_notification","task_id":"a1","status":"{status}"}}"#
            );
            assert_eq!(
                parse_stdout_line(&line),
                vec![SessionEvent::BackgroundTask(
                    BackgroundTaskSignal::Finished {
                        id: "a1".to_string()
                    }
                )],
                "status {status} must be terminal"
            );
        }
        // A non-terminal notification is NOT a completion.
        for status in ["running", "pending"] {
            let line = format!(
                r#"{{"type":"system","subtype":"task_notification","task_id":"a1","status":"{status}"}}"#
            );
            assert!(parse_stdout_line(&line).is_empty());
        }
        // The LEVEL signal replaces the set, filtered to agents only.
        let level = parse_stdout_line(
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a1","task_type":"local_agent"},{"task_id":"sh1","task_type":"bash"},{"task_id":"a3","task_type":"remote_agent"}]}"#,
        );
        assert_eq!(
            level,
            vec![SessionEvent::BackgroundTask(BackgroundTaskSignal::Live {
                agent_ids: vec!["a1".to_string(), "a3".to_string()]
            })]
        );
        // Fail-open: malformed frames yield no event, never a panic.
        for bad in [
            r#"{"type":"system","subtype":"task_started"}"#,
            r#"{"type":"system","subtype":"task_notification","status":"completed"}"#,
            r#"{"type":"system","subtype":"background_tasks_changed"}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":"x"}"#,
        ] {
            assert!(parse_stdout_line(bad).is_empty(), "must skip: {bad}");
        }
        // The init frame still surfaces the model (the task branch must not
        // shadow it).
        assert_eq!(
            parse_stdout_line(r#"{"type":"system","subtype":"init","model":"m-1"}"#),
            vec![SessionEvent::SessionModel("m-1".to_string())]
        );
    }

    #[test]
    fn resume_session_args_writable_main_line_no_fork() {
        // A WRITABLE cross-session resume re-opens the existing conversation with
        // `--resume <id>` and must NOT branch it (`--fork-session`) nor mint a fresh
        // `--session-id`. The write toolset + stream-json flags match a fresh start,
        // so the resumed session writes files identically — it just inherits context.
        // Asserts the env-derived permission mode → serialize against the setter test.
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");

        let args = resume_session_args("sid-resume", Some("be terse"), true, None);
        // Resumes the SAME conversation id.
        let r = args
            .iter()
            .position(|a| a == "--resume")
            .expect("--resume present");
        assert_eq!(args[r + 1], "sid-resume");
        // NOT a read-only fork, and NOT a fresh pinned id.
        assert!(
            !args.contains(&"--fork-session".to_string()),
            "a writable resume must not branch read-only"
        );
        assert!(
            !args.contains(&"--session-id".to_string()),
            "a writable resume continues the existing id, never mints a new one"
        );
        // Writable toolset (Write/Edit), NOT the read-only fork allowlist.
        let tools = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(args[tools + 1], AUTO_ALLOWED_TOOLS);
        // Permission mode tracks autonomy exactly like a fresh start.
        let perm = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[perm + 1], "acceptEdits", "autonomous → acceptEdits");
        // Streams partial messages so a resumed reply renders token-by-token.
        assert!(args.iter().any(|a| a == "--include-partial-messages"));
        // Firmware still injects natively on resume.
        assert!(args.contains(&"--append-system-prompt".to_string()));
        assert!(args.contains(&"be terse".to_string()));
    }

    #[test]
    fn fork_session_args_is_a_fresh_independent_read_only_session() {
        let args = fork_session_args("fork-sid");
        // The host-level fix for the maker-checker reasoning leak: a critic fork
        // must NOT resume the main session id nor branch the live main line —
        // either would inherit the doer's full deliberation/transcript.
        assert!(
            !args.contains(&"--resume".to_string()),
            "fork must NOT --resume the main conversation: {args:?}"
        );
        assert!(
            !args.contains(&"--fork-session".to_string()),
            "fork must NOT branch the live main line: {args:?}"
        );
        // It still gets its own pinned id so the fresh conversation is independent.
        assert!(args.contains(&"--session-id".to_string()));
        assert!(args.contains(&"fork-sid".to_string()));
        // Read-only: plan mode + a read-only tool allowlist (no Write / Edit).
        let perm = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[perm + 1], "plan");
        let tools = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(args[tools + 1], "Read,Grep,Glob");
        assert!(!args[tools + 1].contains("Write"));
        assert!(!args[tools + 1].contains("Edit"));
    }

    #[test]
    fn resume_fork_session_args_carries_the_build_conversation_read_only() {
        // The DEFAULT critic fork carries the build conversation: `--resume <main>`
        // re-loads the doer's transcript, `--fork-session` isolates it into a new id.
        let args = resume_fork_session_args("sid-main");
        let r = args
            .iter()
            .position(|a| a == "--resume")
            .expect("--resume present");
        assert_eq!(args[r + 1], "sid-main");
        assert!(
            args.contains(&"--fork-session".to_string()),
            "must BRANCH the resumed conversation, not continue writing it: {args:?}"
        );
        // READ-ONLY: plan mode + a read-only tool allowlist (no Write / Edit).
        let perm = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[perm + 1], "plan");
        let tools = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(args[tools + 1], "Read,Grep,Glob");
        assert!(!args[tools + 1].contains("Write"));
        assert!(!args[tools + 1].contains("Edit"));
        // It does NOT mint a fresh pinned `--session-id` — that would start an empty
        // conversation and drop the transcript this fork exists to carry.
        assert!(
            !args.contains(&"--session-id".to_string()),
            "a resume-fork carries context; it must not pin a fresh empty session: {args:?}"
        );
        // Streams the verdict token-by-token, like the other session shapes.
        assert!(args.iter().any(|a| a == "--include-partial-messages"));
    }

    #[test]
    fn critic_fork_args_resumes_with_a_live_id_and_falls_back_fresh_without_one() {
        // A live parent id → the resume-fork that carries the build conversation.
        let live = critic_fork_args("sid-main", "fresh-1");
        assert!(live.windows(2).any(|w| w == ["--resume", "sid-main"]));
        assert!(live.contains(&"--fork-session".to_string()));
        // No parent id (empty / whitespace-only) → TODAY's FRESH independent read-only
        // session (the fail-open fallback), pinned to the fresh id, never resuming.
        for empty in ["", "   "] {
            let fresh = critic_fork_args(empty, "fresh-1");
            assert!(
                !fresh.contains(&"--resume".to_string()),
                "no live id → no resume (fresh fallback): {fresh:?}"
            );
            assert!(!fresh.contains(&"--fork-session".to_string()));
            assert!(fresh.windows(2).any(|w| w == ["--session-id", "fresh-1"]));
        }
        // Both shapes are READ-ONLY (plan mode) — the single-writer invariant holds
        // whichever branch is taken.
        for a in [critic_fork_args("sid-main", "f"), critic_fork_args("", "f")] {
            let p = a.iter().position(|x| x == "--permission-mode").unwrap();
            assert_eq!(a[p + 1], "plan");
        }
    }

    /// A fake `claude` that REPORTS whether it was launched with `--resume` (so a
    /// test can tell a real resume-fork from the fresh fallback), then streams a
    /// JSON verdict and ends the turn. Emits `resumed` or `fresh` as the first
    /// text delta, followed by `{"accepts":true}`.
    #[cfg(unix)]
    const RESUME_REPORTING_FAKE: &str = "#!/bin/sh\n\
         case \"$*\" in *--resume*) MODE=resumed ;; *) MODE=fresh ;; esac\n\
         read _line\n\
         printf '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"%s \"}}}\\n' \"$MODE\"\n\
         printf '%s\\n' '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"accepts\\\":true}\"}}}'\n\
         printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}'\n\
         cat >/dev/null\n";

    /// Drain a driven fork's events until its `TurnDone`, collecting the text.
    #[cfg(unix)]
    async fn drain_fork_text(fork: &mut Box<dyn BaseSession>) -> String {
        let mut text = String::new();
        while let Some(ev) = fork.next_event().await {
            match ev {
                SessionEvent::TextDelta(t) => text.push_str(&t),
                SessionEvent::TurnDone { .. } => break,
                _ => {}
            }
        }
        text
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fork_carries_the_build_conversation_via_resume_fork() {
        // With a LIVE parent session id, fork() branches the BUILD CONVERSATION
        // read-only: the fork process is launched with `--resume <main> --fork-session`
        // so the critic inherits the doer's transcript (isolated: --fork-session mints
        // a NEW session id, so the fork never writes the parent's main line). The fake
        // `claude` reports it saw `--resume`, then streams a JSON verdict.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(&tmp, RESUME_REPORTING_FAKE);
        let mut main = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-main",
            true,
            None,
        )
        .await
        .expect("start main");
        let mut fork = main
            .fork()
            .await
            .expect("fork must spawn a read-only session");
        fork.send_turn("review from the architect seat, return JSON".to_string())
            .await
            .expect("fork send");
        let text = drain_fork_text(&mut fork).await;
        assert!(
            text.contains("resumed"),
            "the fork must carry the build conversation (launched with --resume): {text}"
        );
        assert!(
            text.contains("accepts"),
            "fork relayed the verdict text: {text}"
        );
        let _ = fork.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fork_falls_back_to_fresh_when_no_live_session_id() {
        // FAIL-OPEN: with NO live parent id (an empty session id — the not-yet-started
        // / single-shot / offline case) fork() degrades to TODAY's FRESH independent
        // read-only session — it does NOT `--resume`. The fake reports `fresh`.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(&tmp, RESUME_REPORTING_FAKE);
        let mut main =
            ClaudeSession::start_with_program(fake.to_str().unwrap(), &tmp, None, "", true, None)
                .await
                .expect("start main");
        let mut fork = main
            .fork()
            .await
            .expect("fork must spawn a fresh read-only session");
        fork.send_turn("review, return JSON".to_string())
            .await
            .expect("fork send");
        let text = drain_fork_text(&mut fork).await;
        assert!(
            text.contains("fresh"),
            "no live id → the fresh fallback, never --resume: {text}"
        );
        assert!(
            text.contains("accepts"),
            "fork still relayed the verdict text: {text}"
        );
        let _ = fork.end().await;
    }

    #[test]
    fn parse_assistant_yields_toolcall_only_text_streams_separately() {
        // The text block is suppressed here — it already streamed as `stream_event`
        // deltas (see `parse_stream_event_yields_text_delta`). Only the tool call
        // surfaces from the assembled assistant block, so the reply isn't doubled.
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"writing the page"},
            {"type":"tool_use","name":"Write","input":{"file_path":"src/App.tsx"}}
        ]}}"#;
        let evs = parse_stdout_line(line);
        assert_eq!(evs.len(), 1, "text suppressed, only the tool call: {evs:?}");
        let SessionEvent::ToolCall { name, input } = &evs[0] else {
            panic!("expected ToolCall, got {:?}", evs[0]);
        };
        assert_eq!(name, "Write");
        assert_eq!(input["file_path"], "src/App.tsx");
    }

    #[test]
    fn init_frame_yields_session_model_and_is_fail_open() {
        // The session `init` frame carries the EXACT resolved model — surfaced ONCE
        // as a `SessionModel` event so the UI can display the real driving model.
        let init = r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-sonnet-4-5-20250929","tools":["Bash"]}"#;
        assert_eq!(
            parse_stdout_line(init),
            vec![SessionEvent::SessionModel(
                "claude-sonnet-4-5-20250929".to_string()
            )],
            "init frame's model id flows through to a SessionModel event"
        );
        // Fail-open: an init frame with no `model` field yields no event (the UI
        // simply keeps its prior display model, if any).
        let no_model = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
        assert!(
            parse_stdout_line(no_model).is_empty(),
            "missing model → no event (fail-open)"
        );
        // Fail-open: an empty model string is treated as absent.
        let empty = r#"{"type":"system","subtype":"init","model":""}"#;
        assert!(
            parse_stdout_line(empty).is_empty(),
            "empty model → no event"
        );
        // A non-init system frame (status / other) still produces no event.
        let status = r#"{"type":"system","subtype":"status","model":"claude-sonnet-4-5-20250929"}"#;
        assert!(
            parse_stdout_line(status).is_empty(),
            "only the init frame carries the authoritative model"
        );
    }

    #[test]
    fn parse_stream_event_yields_text_delta() {
        // `--include-partial-messages` makes claude stream text as content_block_delta
        // frames — the fix for the 60s-stall freeze on a plain chat reply.
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#;
        assert_eq!(
            parse_stdout_line(line),
            vec![SessionEvent::TextDelta("Hello".to_string())]
        );
        // A thinking delta is surfaced as ITS OWN reasoning event (the collapsed
        // `[thinking]` block), NOT mixed into the answer text stream.
        let think = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#;
        assert_eq!(
            parse_stdout_line(think),
            vec![SessionEvent::ThinkingDelta("hmm".to_string())]
        );
        // A tool-arg (`input_json_delta`) / signature delta is still NOT displayed.
        let arg = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}}"#;
        assert!(parse_stdout_line(arg).is_empty());
        // The session is actually launched with the flag (both main + fork).
        assert!(session_args("sid", None, false, None)
            .iter()
            .any(|a| a == "--include-partial-messages"));
        assert!(fork_session_args("f")
            .iter()
            .any(|a| a == "--include-partial-messages"));
    }

    #[test]
    fn parse_result_maps_subtype_to_status() {
        let done =
            parse_stdout_line(r#"{"type":"result","subtype":"success","stop_reason":"end_turn"}"#);
        assert_eq!(
            done,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                // No `usage` object on this line → None (the consumer estimates).
                usage: None,
            }]
        );
        let trunc = parse_stdout_line(r#"{"type":"result","subtype":"error_max_turns"}"#);
        assert_eq!(
            trunc,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Truncated,
                usage: None,
            }]
        );
        let failed = parse_stdout_line(r#"{"type":"result","subtype":"error_during_execution"}"#);
        assert!(matches!(
            failed.as_slice(),
            [SessionEvent::TurnDone {
                status: TurnStatus::Failed(_),
                ..
            }]
        ));
    }

    #[test]
    fn result_with_is_error_true_is_failed_carrying_the_real_error_text() {
        // The rate-limit / API-error surface: claude ends the turn with
        // `subtype:"success"` BUT `is_error:true`, and the human error in `result`.
        // WITHOUT honoring `is_error` this read as a silent empty Completed (the
        // "完成 / 本轮无文件变更" swallow); it must be a Failed carrying the text.
        let line = r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: Request rejected (429) · You have exceeded the 5-hour usage quota. It will reset at 2026-06-28."}"#;
        match parse_stdout_line(line).as_slice() {
            [SessionEvent::TurnDone {
                status: TurnStatus::Failed(m),
                ..
            }] => {
                assert!(m.contains("429"), "carries the base's real error: {m}");
                assert!(m.contains("usage quota"), "carries the full message: {m}");
            }
            other => panic!("expected TurnDone(Failed) carrying the 429 text, got {other:?}"),
        }
        // An error subtype with no `result` text still fails open to a named reason.
        match parse_stdout_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#,
        )
        .as_slice()
        {
            [SessionEvent::TurnDone {
                status: TurnStatus::Failed(m),
                ..
            }] => assert!(
                m.contains("error_during_execution"),
                "names the subtype: {m}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn genuinely_successful_empty_turn_stays_completed_not_a_false_failure() {
        // A real "no changes needed" turn — success, is_error false (or absent),
        // empty text — must NOT be turned into a failure by the is_error check.
        let explicit = parse_stdout_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":""}"#,
        );
        assert_eq!(
            explicit,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }]
        );
        // is_error absent entirely (defaults false) → still a clean completion.
        let absent = parse_stdout_line(r#"{"type":"result","subtype":"success"}"#);
        assert_eq!(
            absent,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }]
        );
    }

    #[test]
    fn parse_result_reads_real_usage_off_the_result_line() {
        // F3: the stream-json `result` line carries REAL per-turn token usage. The
        // continuous session must surface it on `TurnDone` so `/usage` is truthful
        // on the DEFAULT loop, not just the legacy single-shot path.
        let line = r#"{"type":"result","subtype":"success","usage":{"input_tokens":1200,"cache_read_input_tokens":300,"output_tokens":450},"total_cost_usd":0.02}"#;
        let evs = parse_stdout_line(line);
        match evs.as_slice() {
            [SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: Some(u),
            }] => {
                // cache_read folds into input (consumed input), mirroring claude.rs.
                assert_eq!(u.input_tokens, 1500);
                assert_eq!(u.output_tokens, 450);
            }
            other => panic!("expected TurnDone(Completed) with real usage, got {other:?}"),
        }
        // A result line with no usage object → None (fail-open: estimate downstream).
        let bare = parse_stdout_line(r#"{"type":"result","subtype":"success"}"#);
        assert_eq!(
            bare,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }]
        );
    }

    #[test]
    fn parse_control_request_can_use_tool_is_need_approval() {
        let line = r#"{"type":"control_request","request_id":"req-9","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"}}}"#;
        let evs = parse_stdout_line(line);
        assert_eq!(
            evs,
            vec![SessionEvent::NeedApproval {
                req_id: "req-9".to_string(),
                action: "Bash".to_string(),
                target: "rm -rf /".to_string(),
            }]
        );
    }

    #[test]
    fn parse_user_tool_result_maps_is_error() {
        let line = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","is_error":true,"content":"boom"}]}}"#;
        let evs = parse_stdout_line(line);
        assert_eq!(
            evs,
            vec![SessionEvent::ToolResult {
                ok: false,
                summary: "boom".to_string()
            }]
        );
    }

    #[test]
    fn garbage_and_unknown_lines_fail_open_to_empty() {
        assert!(parse_stdout_line("not json at all").is_empty());
        assert!(parse_stdout_line("").is_empty());
        assert!(parse_stdout_line(r#"{"type":"keep_alive"}"#).is_empty());
        assert!(parse_stdout_line(r#"{"type":"system","subtype":"init"}"#).is_empty());
    }

    #[test]
    fn parent_tool_use_id_only_non_empty_string_is_some() {
        // The single gate for the additive sub-agent branch. A non-null string is the
        // ONLY thing that enters attribution; every main-line shape stays `None`.
        assert_eq!(
            parent_tool_use_id(&serde_json::json!({"parent_tool_use_id":"toolu_abc"})),
            Some("toolu_abc")
        );
        assert_eq!(
            parent_tool_use_id(&serde_json::json!({"parent_tool_use_id":Value::Null})),
            None,
            "explicit null (the main-line shape) → None"
        );
        assert_eq!(
            parent_tool_use_id(&serde_json::json!({})),
            None,
            "absent field → None"
        );
        assert_eq!(
            parent_tool_use_id(&serde_json::json!({"parent_tool_use_id":""})),
            None,
            "empty string → None (never a spurious sub-agent mark)"
        );
    }

    #[test]
    fn main_line_frames_are_byte_for_byte_unchanged_by_the_subagent_fix() {
        // HARD SAFETY CONTRACT: a frame with NO / null `parent_tool_use_id` must
        // produce EXACTLY the events it did before the sub-agent fix existed. We pin
        // that against the literal expected events (the same ones the pre-fix parser
        // tests assert), for both the absent-field and explicit-null shapes.
        let expected_call = vec![SessionEvent::ToolCall {
            name: "Write".to_string(),
            input: serde_json::json!({"file_path": "src/App.tsx"}),
        }];
        // Assistant `tool_use`, field ABSENT (today's frame shape).
        let absent = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Write","input":{"file_path":"src/App.tsx"}}]}}"#;
        assert_eq!(parse_stdout_line(absent), expected_call);
        // Assistant `tool_use`, field explicit-null (the shape claude tags MAIN-line
        // frames with — exactly what we set OUTBOUND in `user_message_line`).
        let null_parent = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[
            {"type":"tool_use","name":"Write","input":{"file_path":"src/App.tsx"}}]}}"#;
        assert_eq!(parse_stdout_line(null_parent), expected_call);
        // User `tool_result`, field explicit-null → unchanged ToolResult.
        let result_null = r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[
            {"type":"tool_result","is_error":true,"content":"boom"}]}}"#;
        assert_eq!(
            parse_stdout_line(result_null),
            vec![SessionEvent::ToolResult {
                ok: false,
                summary: "boom".to_string()
            }]
        );
    }

    #[test]
    fn subagent_frames_attribute_tool_events_with_the_marker() {
        // A NESTED sub-agent's frames carry a non-null `parent_tool_use_id`. Their
        // discrete tool events (where the file-tree garble lives) are prefixed with
        // the sub-agent marker so they read as sub-agent work, not the main agent's.
        let call = r#"{"type":"assistant","parent_tool_use_id":"toolu_sub1","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#;
        let evs = parse_stdout_line(call);
        let SessionEvent::ToolCall { name, input } = &evs[0] else {
            panic!("expected ToolCall, got {evs:?}");
        };
        assert_eq!(name, &format!("{SUBAGENT_MARKER}Read"));
        assert!(
            name.starts_with(SUBAGENT_MARKER),
            "sub-agent tool name carries the attribution marker"
        );
        // The raw tool input is NOT touched — only the rendered name is attributed.
        assert_eq!(input["file_path"], "src/lib.rs");

        // The tool_result (the file-tree summary) is attributed too.
        let result = r#"{"type":"user","parent_tool_use_id":"toolu_sub1","message":{"content":[
            {"type":"tool_result","content":"src/\n  App.tsx\n  main.rs"}]}}"#;
        let evs = parse_stdout_line(result);
        let SessionEvent::ToolResult { ok, summary } = &evs[0] else {
            panic!("expected ToolResult, got {evs:?}");
        };
        assert!(*ok, "success flag preserved");
        assert!(
            summary.starts_with(SUBAGENT_MARKER),
            "sub-agent tool-result summary carries the marker: {summary}"
        );
        assert!(
            summary.contains("App.tsx"),
            "the original summary content is preserved after the marker: {summary}"
        );

        // Repo rule: the marker is ASCII/CJK, never an emoji.
        assert!(
            !SUBAGENT_MARKER.chars().any(|c| c as u32 >= 0x1F000),
            "sub-agent marker must contain no emoji"
        );
    }

    #[test]
    fn subagent_stream_buffers_and_flushes_as_one_block_on_spawning_tool_result() {
        let mut g = SubagentGrouper::default();
        // Main line spawns the sub-agent: an `Agent` tool_use with id `toolu_spawn`.
        let spawn = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"toolu_spawn","name":"Agent",
             "input":{"subagent_type":"Explore","description":"scan the repo"}}]}}"#;
        assert_eq!(
            g.on_line(spawn),
            parse_stdout_line(spawn),
            "the main-line spawn frame passes through unchanged"
        );

        let delta = |text: &str| {
            format!(
                r#"{{"type":"stream_event","parent_tool_use_id":"toolu_spawn","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{text}"}}}}}}"#
            )
        };
        // First sub-agent event → exactly ONE lightweight working row (buffer opens).
        let opened = g.on_line(&delta("exploring the tree "));
        assert_eq!(
            opened.len(),
            1,
            "one working row, no leaked text: {opened:?}"
        );
        let SessionEvent::ToolCall { name, .. } = &opened[0] else {
            panic!("expected the working row, got {opened:?}");
        };
        assert!(
            name.starts_with(SUBAGENT_MARKER)
                && name.contains("Explore")
                && name.contains(SUBAGENT_WORKING),
            "the working row names the sub-agent: {name}"
        );

        // Everything else the sub-agent streams is HELD: zero events mid-run —
        // this is exactly the fragmentary interleave the fix removes.
        assert!(g.on_line(&delta("in src/")).is_empty());
        let call = r#"{"type":"assistant","parent_tool_use_id":"toolu_spawn","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#;
        assert!(
            g.on_line(call).is_empty(),
            "a sub-agent tool call is buffered, not yielded"
        );
        let result = r#"{"type":"user","parent_tool_use_id":"toolu_spawn","message":{"content":[
            {"type":"tool_result","content":"17 | fn main() {}"}]}}"#;
        assert!(
            g.on_line(result).is_empty(),
            "a sub-agent tool result is buffered, not yielded"
        );

        // The MAIN-line tool_result answering the spawn id terminates the
        // sub-agent: the grouped block (header + ONE compacted ToolResult)
        // flushes FIRST, then the untouched main-line final report.
        let report = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"toolu_spawn","content":"final report"}]}}"#;
        let evs = g.on_line(report);
        assert_eq!(
            evs.len(),
            3,
            "header + grouped result + main-line report: {evs:?}"
        );
        let SessionEvent::ToolCall { name, .. } = &evs[0] else {
            panic!("expected the grouped-block header, got {evs:?}");
        };
        assert_eq!(name, &format!("{SUBAGENT_MARKER}Explore"));
        let SessionEvent::ToolResult { ok, summary } = &evs[1] else {
            panic!("expected the grouped result, got {evs:?}");
        };
        assert!(*ok);
        assert!(summary.starts_with(SUBAGENT_MARKER));
        assert!(
            summary.contains("exploring the tree in src/"),
            "text deltas concatenate into one coherent run: {summary}"
        );
        assert!(
            summary.contains("Read(src/lib.rs) → 17 | fn main() {}"),
            "tool rows compact as `name(target) → summary`: {summary}"
        );
        assert_eq!(
            &evs[2],
            &parse_stdout_line(report)[0],
            "the main-line final report event is untouched"
        );
    }

    #[test]
    fn background_subagent_flushes_on_terminal_task_notification() {
        let mut g = SubagentGrouper::default();
        let spawn = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"task_bg1","name":"Task",
             "input":{"description":"write the docs","run_in_background":true}}]}}"#;
        assert_eq!(g.on_line(spawn), parse_stdout_line(spawn));
        let delta = r#"{"type":"stream_event","parent_tool_use_id":"task_bg1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"drafting"}}}"#;
        let opened = g.on_line(delta);
        assert_eq!(opened.len(), 1, "one working row: {opened:?}");

        // A non-terminal notification flushes nothing (today's no-event parity).
        let running = r#"{"type":"system","subtype":"task_notification","task_id":"task_bg1","status":"running"}"#;
        assert_eq!(g.on_line(running), parse_stdout_line(running));

        // The terminal notification flushes the grouped block BEFORE the
        // Finished lifecycle signal.
        let done = r#"{"type":"system","subtype":"task_notification","task_id":"task_bg1","status":"completed"}"#;
        let evs = g.on_line(done);
        assert_eq!(evs.len(), 3, "header + grouped result + Finished: {evs:?}");
        assert!(
            matches!(&evs[0], SessionEvent::ToolCall { name, .. }
                if name.starts_with(SUBAGENT_MARKER) && name.contains("write the docs")),
            "the header carries the task description label: {evs:?}"
        );
        assert!(
            matches!(&evs[1], SessionEvent::ToolResult { summary, .. } if summary.contains("drafting")),
            "the grouped result carries the buffered output: {evs:?}"
        );
        assert_eq!(
            evs[2],
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished {
                id: "task_bg1".to_string()
            })
        );
    }

    #[test]
    fn turn_done_flushes_held_buffers_before_the_turn_done_event() {
        let mut g = SubagentGrouper::default();
        // A sub-agent with NO recorded spawn label (its frame was missed) —
        // degrades to the plain marker, never blocks the buffering.
        let delta = r#"{"type":"stream_event","parent_tool_use_id":"toolu_lost","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"orphan work"}}}"#;
        let opened = g.on_line(delta);
        assert_eq!(opened.len(), 1, "working row: {opened:?}");
        assert!(
            matches!(&opened[0], SessionEvent::ToolCall { name, .. }
                if name == &format!("{SUBAGENT_MARKER}{SUBAGENT_WORKING}")),
            "label-less working row is marker + working: {opened:?}"
        );

        // No terminating signal ever arrives; the turn ends → the held buffer
        // flushes BEFORE the TurnDone event, so nothing is silently dropped.
        let result_line = r#"{"type":"result","subtype":"success","stop_reason":"end_turn"}"#;
        let evs = g.on_line(result_line);
        assert_eq!(evs.len(), 3, "header + grouped result + TurnDone: {evs:?}");
        assert!(
            matches!(&evs[0], SessionEvent::ToolCall { name, .. } if name == "↳ 子代理"),
            "label-less header is the bare marker stem: {evs:?}"
        );
        assert!(
            matches!(&evs[1], SessionEvent::ToolResult { summary, .. } if summary.contains("orphan work"))
        );
        assert!(
            matches!(
                evs.last(),
                Some(SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    ..
                })
            ),
            "TurnDone comes AFTER every flushed block: {evs:?}"
        );
    }

    #[test]
    fn buffer_cap_triggers_early_flush_with_truncation_note_and_one_working_row() {
        let mut g = SubagentGrouper::default();
        let chunk = "x".repeat(1024);
        let line = |text: &str| {
            format!(
                r#"{{"type":"stream_event","parent_tool_use_id":"toolu_big","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{text}"}}}}}}"#
            )
        };
        // 33 KB of held text crosses the 32 KB cap on the 33rd chunk → ONE early
        // partial flush; the buffer stays open.
        let mut all = Vec::new();
        for _ in 0..33 {
            all.extend(g.on_line(&line(&chunk)));
        }
        let working_rows = all
            .iter()
            .filter(|e| {
                matches!(e, SessionEvent::ToolCall { name, .. } if name.contains(SUBAGENT_WORKING))
            })
            .count();
        assert_eq!(
            working_rows,
            1,
            "the buffer-open working row appears exactly once: {}",
            all.len()
        );
        let early: Vec<&String> = all
            .iter()
            .filter_map(|e| match e {
                SessionEvent::ToolResult { summary, .. } => Some(summary),
                _ => None,
            })
            .collect();
        assert_eq!(early.len(), 1, "exactly one early partial flush block");
        assert!(
            early[0].contains(SUBAGENT_EARLY_FLUSH_NOTE),
            "the early flush carries the continuation note: {}",
            early[0]
        );

        // The buffer stays OPEN after the cap flush: further output is still
        // grouped (zero events, no second working row) and the terminal flush
        // carries ONLY the remainder, without the note.
        assert!(g.on_line(&line("tail-after-cap")).is_empty());
        let report = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_big","content":"done"}]}}"#;
        let evs = g.on_line(report);
        assert!(
            evs.iter().any(|e| matches!(e, SessionEvent::ToolResult { summary, .. }
                if summary.contains("tail-after-cap") && !summary.contains(SUBAGENT_EARLY_FLUSH_NOTE))),
            "the terminal flush groups the remainder without the note: {evs:?}"
        );
    }

    #[test]
    fn grouper_yields_identical_events_for_main_line_frames() {
        // The pump routes every line through the grouper; for MAIN-line frames it
        // must be event-for-event identical to the stateless parse (the
        // byte-for-byte contract of the de-interleaving fix).
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"x","model":"claude-sonnet-4-5"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"src/App.tsx"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"boom"}]}}"#,
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
            r#"{"type":"system","subtype":"task_started","task_id":"a1","task_type":"local_agent"}"#,
            r#"{"type":"system","subtype":"task_notification","task_id":"a1","status":"completed"}"#,
            r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
            "not json at all",
            "",
            r#"{"type":"result","subtype":"success","stop_reason":"end_turn"}"#,
        ];
        let mut g = SubagentGrouper::default();
        for line in lines {
            assert_eq!(
                g.on_line(line),
                parse_stdout_line(line),
                "main-line parity broken for: {line}"
            );
        }
    }

    #[test]
    fn subagent_grouper_labels_contain_no_emoji() {
        // Repo rule: no emoji as functional markers — same bar as SUBAGENT_MARKER.
        for s in [
            SUBAGENT_WORKING,
            SUBAGENT_EARLY_FLUSH_NOTE,
            SUBAGENT_ROW_FAILED,
        ] {
            assert!(
                !s.chars().any(|c| c as u32 >= 0x1F000),
                "no emoji in the grouper label: {s}"
            );
        }
    }

    #[test]
    fn subagent_approval_request_is_never_buffered() {
        // A `control_request` raised while a sub-agent runs must pass through
        // IMMEDIATELY (holding it would deadlock the approval loop), even if the
        // frame carries a `parent_tool_use_id`.
        let mut g = SubagentGrouper::default();
        let delta = r#"{"type":"stream_event","parent_tool_use_id":"toolu_a","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"working"}}}"#;
        assert_eq!(g.on_line(delta).len(), 1, "working row only");
        let approval = r#"{"type":"control_request","parent_tool_use_id":"toolu_a","request_id":"r9","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#;
        let evs = g.on_line(approval);
        assert!(
            matches!(&evs[..], [SessionEvent::NeedApproval { req_id, .. }] if req_id == "r9"),
            "the approval request passes through immediately: {evs:?}"
        );
    }

    #[test]
    fn new_session_ids_look_like_uuid_v4_and_are_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
    }

    // ── Integration: a fake `claude` stream-json emitter (unix-only sh). ──
    #[cfg(unix)]
    fn write_fake_claude(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-claude.sh");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn tempfile_dir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("umadev-sess-{}", new_session_id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_relays_toolcall_sequence_then_turn_done() {
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nread _line\n\
             printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\n\
             printf '%s\\n' '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}}'\n\
             printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"},{\"type\":\"tool_use\",\"name\":\"Write\",\"input\":{\"file_path\":\"App.tsx\"}}]}}'\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}'\n\
             cat >/dev/null\n",
        );
        let mut s = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-test",
            true,
            None,
        )
        .await
        .expect("start");
        s.send_turn("build a todo page".to_string())
            .await
            .expect("send");

        let mut got = Vec::new();
        while let Some(ev) = s.next_event().await {
            let done = matches!(ev, SessionEvent::TurnDone { .. });
            got.push(ev);
            if done {
                break;
            }
        }
        assert_eq!(got[0], SessionEvent::TextDelta("hi".to_string()));
        assert!(matches!(&got[1], SessionEvent::ToolCall { name, .. } if name == "Write"));
        assert_eq!(
            got.last().unwrap(),
            &SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }
        );
        let _ = s.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_groups_subagent_stream_into_one_block() {
        // End-to-end through the REAL pump: a sub-agent's streamed frames must
        // arrive as ONE grouped block (working row → header → compacted result)
        // instead of interleaving with the main line.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nread _line\n\
             printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"sub1\",\"name\":\"Agent\",\"input\":{\"subagent_type\":\"Explore\",\"description\":\"scan\"}}]}}'\n\
             printf '%s\\n' '{\"type\":\"stream_event\",\"parent_tool_use_id\":\"sub1\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"found it\"}}}'\n\
             printf '%s\\n' '{\"type\":\"user\",\"parent_tool_use_id\":\"sub1\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"src tree\"}]}}'\n\
             printf '%s\\n' '{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"sub1\",\"content\":\"report\"}]}}'\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}'\n\
             cat >/dev/null\n",
        );
        let mut s = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-subagent",
            true,
            None,
        )
        .await
        .expect("start");
        s.send_turn("explore".to_string()).await.expect("send");

        let mut got = Vec::new();
        while let Some(ev) = s.next_event().await {
            let done = matches!(ev, SessionEvent::TurnDone { .. });
            got.push(ev);
            if done {
                break;
            }
        }
        // 0: the main-line Agent spawn row (untouched).
        assert!(
            matches!(&got[0], SessionEvent::ToolCall { name, .. } if name == "Agent"),
            "main-line spawn row unchanged: {got:?}"
        );
        // 1: the ONE working row when the buffer opens.
        assert!(
            matches!(&got[1], SessionEvent::ToolCall { name, .. }
                if name.starts_with(SUBAGENT_MARKER) && name.contains(SUBAGENT_WORKING)),
            "working row: {got:?}"
        );
        // 2–3: the grouped block, flushed by the spawning tool_result.
        assert!(
            matches!(&got[2], SessionEvent::ToolCall { name, .. }
                if name == &format!("{SUBAGENT_MARKER}Explore")),
            "grouped-block header: {got:?}"
        );
        assert!(
            matches!(&got[3], SessionEvent::ToolResult { summary, .. }
                if summary.starts_with(SUBAGENT_MARKER)
                    && summary.contains("found it")
                    && summary.contains("src tree")),
            "grouped, compacted sub-agent output: {got:?}"
        );
        // 4: the main-line final report, unmarked and unchanged.
        assert!(
            matches!(&got[4], SessionEvent::ToolResult { summary, .. } if summary == "report"),
            "main-line final report unchanged: {got:?}"
        );
        assert!(matches!(got.last(), Some(SessionEvent::TurnDone { .. })));
        let _ = s.end().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_reaps_the_child_within_the_bounded_wait() {
        // A base that stays alive (a long sleep) after emitting init. `end()` must
        // start-kill it AND wait (bounded) for the reap, so no orphan lingers and
        // shutdown timing is deterministic — not left to a lazy drop.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nread _line\n\
             printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\n\
             sleep 30\n",
        );
        let mut s = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid",
            true,
            None,
        )
        .await
        .expect("start");
        // The child is alive before end().
        assert!(s.try_exit_status().is_none(), "child should be running");

        let started = tokio::time::Instant::now();
        s.end().await.expect("end");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "end() must return within its bounded reap budget, not hang: {:?}",
            started.elapsed()
        );
        // end() awaited the reap, so the exit is observable immediately after.
        assert!(
            s.try_exit_status().is_some(),
            "end() must reap the child (no orphan) within the bounded wait"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stderr_tail_captures_the_base_idle_reason_and_exit_is_observable() {
        // A base that prints a config error to STDERR then exits (the "bad model
        // / not logged in" shape). The driver must (1) capture that stderr line
        // as the idle reason and (2) eventually observe the process exit — the
        // two diagnostics the TUI needs to explain "base session idle".
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\n\
             echo 'error: model gpt-bogus is not available' 1>&2\n\
             exit 7\n",
        );
        let s = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-stderr",
            true,
            None,
        )
        .await
        .expect("start");

        // Poll briefly for the child to exit + its stderr to be drained (fail-open
        // bounded loop, never an unbounded wait).
        let mut exited = false;
        let mut tail = None;
        // Generous bound (≈5s): tokio's Unix child reaper (which `try_wait` depends
        // on) can be slow to get a worker when the whole test suite runs in parallel
        // and saturates the cores — a tighter budget made this flake under load.
        for _ in 0..250 {
            if s.try_exit_status().is_some() {
                exited = true;
            }
            if let Some(t) = s.stderr_tail() {
                tail = Some(t);
            }
            if exited && tail.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(exited, "try_exit_status must observe the exited child");
        let tail = tail.expect("stderr_tail must capture the base's stderr error");
        assert!(
            tail.contains("gpt-bogus is not available"),
            "the captured tail must carry the base's idle reason: {tail}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_spawn_sets_govern_root_env_to_workspace() {
        // The spawned base must inherit UMADEV_GOVERN_ROOT = the session
        // workspace, so the PreToolUse hook it spawns governs THIS run (and only
        // this run). The fake claude emits the env value back as a text delta;
        // the test asserts the session relays the workspace path.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nread _line\n\
             printf '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"%s\"}}}\\n' \"$UMADEV_GOVERN_ROOT\"\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}'\n\
             cat >/dev/null\n",
        );
        let mut s = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-env",
            true,
            None,
        )
        .await
        .expect("start");
        s.send_turn("go".to_string()).await.expect("send");
        let mut text = String::new();
        while let Some(ev) = s.next_event().await {
            match ev {
                SessionEvent::TextDelta(t) => text.push_str(&t),
                SessionEvent::TurnDone { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            text.trim(),
            tmp.to_string_lossy(),
            "the base must see UMADEV_GOVERN_ROOT = the session workspace"
        );
        let _ = s.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn base_crash_mid_turn_fails_open_to_turn_done_failed() {
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\"}'\n",
        );
        let mut s = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-crash",
            true,
            None,
        )
        .await
        .expect("start");
        let _ = s.send_turn("go".to_string()).await;
        let mut last = None;
        while let Some(ev) = s.next_event().await {
            last = Some(ev);
        }
        assert!(matches!(
            last,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Failed(_),
                ..
            })
        ));
    }

    // ── Item 1: `--max-turns` per-run execution shaping (arg construction) ──

    #[test]
    fn session_args_omit_max_turns_when_no_cap() {
        // Fail-open: `None` → NO `--max-turns` flag → claude's default unbounded loop
        // (today's behavior), on both a fresh start and a writable resume.
        // Hold PERM_ENV_LOCK: this test MUTATES the shared permission-mode env
        // (`EnvRestore::remove`), so it must serialize against the other env-touching
        // tests — else its remove clobbers a sibling's `set_var("plan")` mid-flight and
        // flakes the autonomy-override test.
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");
        let fresh = session_args("sid", None, true, None);
        assert!(
            !fresh.iter().any(|a| a == "--max-turns"),
            "no cap → no flag: {fresh:?}"
        );
        let resumed = resume_session_args("sid", None, true, None);
        assert!(
            !resumed.iter().any(|a| a == "--max-turns"),
            "no cap on resume → no flag: {resumed:?}"
        );
    }

    #[test]
    fn session_args_include_max_turns_when_capped() {
        // A cap appends `--max-turns <n>` (the runaway backstop) on both shapes.
        // Hold PERM_ENV_LOCK for the same reason as the sibling above: this test mutates
        // the shared permission-mode env, so it serializes with every env-touching test.
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");
        let fresh = session_args("sid", None, false, Some(150));
        let i = fresh
            .iter()
            .position(|a| a == "--max-turns")
            .expect("--max-turns present when capped");
        assert_eq!(fresh[i + 1], "150");
        let resumed = resume_session_args("sid", None, false, Some(150));
        let ri = resumed.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(resumed[ri + 1], "150");
    }

    #[test]
    fn a_critic_fork_is_turn_capped_low_below_a_deliberate_build() {
        // Per-tier caps: a read-only critic consult fork carries a VERY LOW turn ceiling
        // (a runaway backstop), and a deliberate build session's cap is much higher. Both
        // fork shapes (fresh + carried-transcript) are capped identically.
        // A Depth::Standard build tier (see `umadev_agent::router::Depth::max_turns`).
        let build_cap: u32 = 150;
        let build = session_args("sid", None, true, Some(build_cap));
        let bi = build.iter().position(|a| a == "--max-turns").unwrap();
        let build_n: u32 = build[bi + 1].parse().unwrap();

        for fork in [fork_session_args("f"), resume_fork_session_args("main")] {
            let fi = fork
                .iter()
                .position(|a| a == "--max-turns")
                .expect("a read-only critic fork is turn-capped");
            let fork_n: u32 = fork[fi + 1].parse().unwrap();
            assert_eq!(
                fork_n, CRITIC_FORK_MAX_TURNS,
                "critic fork uses the low const"
            );
            assert!(
                build_n > fork_n,
                "a deliberate build cap ({build_n}) must exceed the critic consult cap ({fork_n})"
            );
        }
    }

    // ── Item 2: inbound control_response / system:init are observed, not dropped ──

    #[test]
    fn inbound_control_response_is_observed_but_produces_no_event() {
        // claude's ACK to our `interrupt` (an inbound control_response) used to fall
        // through `_ => vec![]` and vanish. It is now described for the tracing log, but
        // STILL emits no SessionEvent — the approval loop is untouched.
        let line =
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"int-1"}}"#;
        assert!(
            parse_stdout_line(line).is_empty(),
            "a control ack emits no event"
        );
        let v: Value = serde_json::from_str(line).unwrap();
        let desc = describe_control_response(&v);
        assert!(
            desc.contains("success"),
            "ack subtype is observable: {desc}"
        );
        assert!(
            desc.contains("int-1"),
            "acked request id is observable: {desc}"
        );
    }

    #[test]
    fn inbound_system_init_is_observed_but_produces_no_event() {
        let line = r#"{"type":"system","subtype":"init","session_id":"sid-x"}"#;
        assert!(
            parse_stdout_line(line).is_empty(),
            "system init emits no event"
        );
        let v: Value = serde_json::from_str(line).unwrap();
        let desc = describe_system_event(&v);
        assert!(
            desc.contains("init"),
            "system subtype is observable: {desc}"
        );
        assert!(desc.contains("sid-x"), "session id is observable: {desc}");
    }

    #[test]
    fn malformed_control_and_system_frames_fail_open_no_panic_no_event() {
        // Fail-open: a control_response / system frame missing (or mistyping) its inner
        // fields is described WITHOUT panicking and STILL emits no event — never a
        // disturbance to the can_use_tool → NeedApproval → respond loop.
        for line in [
            r#"{"type":"control_response"}"#,
            r#"{"type":"control_response","response":{}}"#,
            r#"{"type":"control_response","response":42}"#,
            r#"{"type":"system"}"#,
            r#"{"type":"system","subtype":null}"#,
        ] {
            assert!(parse_stdout_line(line).is_empty(), "no event for: {line}");
            let v: Value = serde_json::from_str(line).unwrap();
            // Neither describer panics on the malformed shape (both fall back to "?").
            let _ = describe_control_response(&v);
            let _ = describe_system_event(&v);
        }
        // A control ack must NEVER be misread as a can_use_tool approval prompt.
        let ack =
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"x"}}"#;
        assert!(
            !parse_stdout_line(ack)
                .iter()
                .any(|e| matches!(e, SessionEvent::NeedApproval { .. })),
            "a control ack must never become a NeedApproval"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inbound_control_response_and_system_do_not_disturb_the_turn_stream() {
        // Item 2 over the fake peer: a `system` init line AND an inbound `control_response`
        // (claude's interrupt ack) interleaved in the stream are observed (logged) but
        // surface NO events — the turn still relays its tool call and completes cleanly,
        // and no spurious NeedApproval is raised by the control ack.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nread _line\n\
             printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"x\"}'\n\
             printf '%s\\n' '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"int-1\"}}'\n\
             printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Write\",\"input\":{\"file_path\":\"a.txt\"}}]}}'\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}'\n\
             cat >/dev/null\n",
        );
        let mut s = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-ctrl",
            true,
            None,
        )
        .await
        .expect("start");
        s.send_turn("go".to_string()).await.expect("send");
        let mut got = Vec::new();
        while let Some(ev) = s.next_event().await {
            let done = matches!(ev, SessionEvent::TurnDone { .. });
            got.push(ev);
            if done {
                break;
            }
        }
        assert!(
            got.iter()
                .any(|e| matches!(e, SessionEvent::ToolCall { name, .. } if name == "Write")),
            "the tool call still surfaces past the control/system frames: {got:?}"
        );
        assert!(
            !got.iter()
                .any(|e| matches!(e, SessionEvent::NeedApproval { .. })),
            "the control ack is not an approval prompt: {got:?}"
        );
        assert!(
            matches!(
                got.last(),
                Some(SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    ..
                })
            ),
            "the turn completes cleanly: {got:?}"
        );
        let _ = s.end().await;
    }
}
