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
//! --allowedTools "Read,Edit,Write,Bash"` (+ optional `--append-system-prompt`).
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
    ApprovalDecision, BaseSession, SessionError, SessionEvent, TurnStatus, Usage,
};

use crate::spawn_parts;
use crate::stderr_tail::{drain_stderr_into, StderrTail};

/// How many events the stdout-reader task may buffer ahead of the consumer.
const EVENT_CHANNEL_CAP: usize = 256;

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
    /// The pinned conversation id (also usable for `--resume` on recovery, and
    /// for `--resume <id> --fork-session` to open a read-only critic fork).
    session_id: String,
    /// The resolved `claude` program string this session was spawned with, kept
    /// so [`fork`](BaseSession::fork) re-spawns the SAME binary (honoring a test
    /// fake / `UMADEV_CLAUDE_BIN` override).
    program: String,
    /// The workspace this session runs in, so a fork operates in the same dir.
    workspace: std::path::PathBuf,
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
    pub async fn start(
        workspace: &Path,
        append_system: Option<&str>,
        autonomous: bool,
    ) -> Result<Self, SessionError> {
        let program = std::env::var("UMADEV_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        Self::start_with_program(
            &program,
            workspace,
            append_system,
            &new_session_id(),
            autonomous,
        )
        .await
    }

    /// Start a session against an explicit `program` + pinned `session_id`
    /// (mainly for tests, where `program` is a fake stream-json emitter).
    /// `autonomous` chooses the permission mode (see [`session_args`]).
    pub async fn start_with_program(
        program: &str,
        workspace: &Path,
        append_system: Option<&str>,
        session_id: &str,
        autonomous: bool,
    ) -> Result<Self, SessionError> {
        Self::spawn_with_args(
            program,
            workspace,
            &session_args(session_id, append_system, autonomous),
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
        let mut cmd = Command::new(prog);
        cmd.args(&lead);
        cmd.args(args);
        cmd.current_dir(workspace);
        // Mark "UmaDev is driving" + the governed root for the PreToolUse hook
        // (see `crate::GOVERN_ROOT_ENV`). The base inherits this var and passes
        // it to the hook subprocess it spawns, so the hook governs THIS session's
        // writes while leaving the user's own claude sessions completely
        // untouched. Set on every spawned `claude` (main + read-only fork) so the
        // governance scope is consistent across the session's process tree.
        cmd.env(crate::GOVERN_ROOT_ENV, workspace);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
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
        })
    }

    /// The pinned conversation id (e.g. for `--resume` on crash recovery).
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Write one NDJSON line + flush to the live session's stdin.
    async fn write_line(&mut self, line: &str) -> Result<(), SessionError> {
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
        // A read-only critic fork: resume the MAIN session id and branch it with
        // `--fork-session` (a new id, the main line untouched), in
        // `--permission-mode plan` so the fork can READ the workspace + the prior
        // context but can NEVER write (single-writer invariant — only the main
        // session writes the blackboard). A fresh fork id is generated so the new
        // branch gets its own pinned conversation. Fail-open: a spawn failure
        // surfaces as `Start`, which the caller treats like `ForkUnsupported`.
        let fork_id = new_session_id();
        let args = fork_session_args(&self.session_id, &fork_id);
        let s = Self::spawn_with_args(&self.program, &self.workspace, &args, &fork_id).await?;
        Ok(Box::new(s))
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

    // `start_kill` is sync; the trait method is async for the other impls.
    #[allow(clippy::unused_async)]
    async fn end(&mut self) -> Result<(), SessionError> {
        // Best-effort: killing the child drops stdin (EOF) and tears down the
        // reader/stderr tasks. kill_on_drop is also set as a backstop.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
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
}

/// Reader task: parse stdout NDJSON → events forever. On EOF (the base process
/// died / the session ended) emit a terminal `Failed` so a crash mid-turn
/// surfaces as `TurnDone{Failed}` rather than a silent hang.
async fn pump_stdout(stdout: ChildStdout, tx: mpsc::Sender<SessionEvent>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        for ev in parse_stdout_line(&line) {
            if tx.send(ev).await.is_err() {
                return; // consumer dropped → stop
            }
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

/// The argument vector preceding any input — the stream-json continuous-session
/// flags. Exposed for tests. `--append-system-prompt` (NOT `--system-prompt`).
///
/// `autonomous` picks the permission mode so claude tracks the trust tier like
/// the codex / opencode drivers: `true` → `acceptEdits` (write unattended),
/// `false` → `default` (claude raises a `can_use_tool` approval per tool, which
/// the orchestrator answers — keeping the human-in-the-loop / irreversible-action
/// floor live). `UMADEV_CLAUDE_PERMISSION_MODE`, when set, overrides the derived
/// default for both tiers.
#[must_use]
pub fn session_args(
    session_id: &str,
    append_system: Option<&str>,
    autonomous: bool,
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
        "Read,Edit,Write,Bash".to_string(),
    ];
    if let Some(sys) = append_system.filter(|s| !s.is_empty()) {
        args.push("--append-system-prompt".to_string());
        args.push(sys.to_string());
    }
    args
}

/// Resolve claude's `--permission-mode` for an autonomy tier. `autonomous` →
/// `acceptEdits` (write unattended); otherwise `default` (claude asks before
/// each tool → a `NeedApproval` the orchestrator answers, the guarded
/// human-in-the-loop tier). `UMADEV_CLAUDE_PERMISSION_MODE` overrides both.
fn claude_permission_mode(autonomous: bool) -> String {
    std::env::var("UMADEV_CLAUDE_PERMISSION_MODE").unwrap_or_else(|_| {
        if autonomous {
            "acceptEdits".to_string()
        } else {
            "default".to_string()
        }
    })
}

/// The argument vector for a READ-ONLY critic fork: resume `main_session_id` and
/// branch it with `--fork-session` (so the main line is never disturbed), pin the
/// new branch to `fork_session_id`, and force `--permission-mode plan` so the
/// fork can read context + the workspace but can NEVER write a file (the
/// single-writer invariant). `--allowedTools "Read,Grep,Glob"` further fences it
/// to read tools. Exposed for tests.
#[must_use]
pub fn fork_session_args(main_session_id: &str, fork_session_id: &str) -> Vec<String> {
    vec![
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
        // Branch off the main conversation; the new branch gets its own id.
        "--resume".to_string(),
        main_session_id.to_string(),
        "--fork-session".to_string(),
        "--session-id".to_string(),
        fork_session_id.to_string(),
        // Read-only: plan mode never applies an edit; the tool allowlist is
        // read-only too. Two independent fences on the single-writer invariant.
        "--permission-mode".to_string(),
        "plan".to_string(),
        "--allowedTools".to_string(),
        "Read,Grep,Glob".to_string(),
    ]
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
#[must_use]
pub fn parse_stdout_line(line: &str) -> Vec<SessionEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return vec![]; // not JSON (a stray log line) → skip
    };
    match v.get("type").and_then(Value::as_str) {
        // Incremental text deltas (we launch with `--include-partial-messages`), so
        // a reply streams token-by-token instead of arriving all at once.
        Some("stream_event") => parse_stream_event(&v),
        Some("assistant") => parse_assistant(&v),
        Some("user") => parse_user_tool_results(&v),
        Some("result") => vec![parse_result(&v)],
        Some("control_request") => parse_control_request(&v),
        // system/init, keep_alive, status, tool_progress, … → not events.
        _ => vec![],
    }
}

/// A `stream_event` frame (present with `--include-partial-messages`) → a
/// `TextDelta` for each `content_block_delta` carrying a `text_delta`. Thinking
/// deltas, tool-arg (`input_json_delta`) deltas, and the start/stop frames are
/// ignored — tool calls are surfaced from the final aggregate `assistant` block.
fn parse_stream_event(v: &Value) -> Vec<SessionEvent> {
    let Some(event) = v.get("event") else {
        return vec![];
    };
    if event.get("type").and_then(Value::as_str) != Some("content_block_delta") {
        return vec![];
    }
    let delta = event.get("delta");
    if delta.and_then(|d| d.get("type")).and_then(Value::as_str) != Some("text_delta") {
        return vec![]; // thinking_delta / input_json_delta → not displayed text
    }
    delta
        .and_then(|d| d.get("text"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(|t| vec![SessionEvent::TextDelta(t.to_string())])
        .unwrap_or_default()
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
fn parse_result(v: &Value) -> SessionEvent {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    let status = match subtype {
        "success" => TurnStatus::Completed,
        "error_max_turns" | "error_max_budget_usd" | "error_max_structured_output_retries" => {
            TurnStatus::Truncated
        }
        other => {
            let reason = v
                .get("result")
                .and_then(Value::as_str)
                .map_or_else(|| format!("base error ({other})"), str::to_string);
            TurnStatus::Failed(reason)
        }
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

/// Truncated preview of a tool_result `content` (string or block array).
fn summarize_tool_content(content: Option<&Value>) -> String {
    let raw = match content {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    truncate(&raw, 200)
}

/// A short, human-readable target for an approval prompt (file path / command).
fn summarize_input(input: &Value) -> String {
    for key in ["file_path", "path", "command", "pattern", "url"] {
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
        let args = session_args("sid-1", Some("be terse"), true);
        assert!(args.contains(&"--input-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"sid-1".to_string()));
        assert!(args.contains(&"--append-system-prompt".to_string()));
        assert!(!args.contains(&"--system-prompt".to_string()));
        assert!(args.contains(&"be terse".to_string()));
    }

    /// The permission mode tracks the autonomy tier (claude consistent with
    /// codex / opencode): autonomous → `acceptEdits` (write unattended), guarded
    /// → `default` (claude asks per tool → a NeedApproval the orchestrator
    /// answers, so the human-in-the-loop / irreversible-action floor is live).
    #[test]
    fn session_args_permission_mode_tracks_autonomy() {
        // Guard against the env override leaking in from a sibling process.
        let prior = std::env::var_os("UMADEV_CLAUDE_PERMISSION_MODE");
        std::env::remove_var("UMADEV_CLAUDE_PERMISSION_MODE");

        let auto = session_args("sid-a", None, true);
        let auto_idx = auto.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(auto[auto_idx + 1], "acceptEdits", "auto → acceptEdits");

        let guarded = session_args("sid-g", None, false);
        let g_idx = guarded
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(
            guarded[g_idx + 1],
            "default",
            "guarded → default (claude asks → NeedApproval, human in the loop)"
        );

        // The explicit override beats the derived default for either tier.
        std::env::set_var("UMADEV_CLAUDE_PERMISSION_MODE", "plan");
        let overridden = session_args("sid-o", None, true);
        let o_idx = overridden
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(overridden[o_idx + 1], "plan", "env override wins");

        match prior {
            Some(v) => std::env::set_var("UMADEV_CLAUDE_PERMISSION_MODE", v),
            None => std::env::remove_var("UMADEV_CLAUDE_PERMISSION_MODE"),
        }
    }

    #[test]
    fn fork_session_args_resume_branch_and_read_only() {
        let args = fork_session_args("main-sid", "fork-sid");
        // Branches off the main conversation without disturbing it.
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"main-sid".to_string()));
        assert!(args.contains(&"--fork-session".to_string()));
        // The new branch gets its own pinned id.
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

    #[cfg(unix)]
    #[tokio::test]
    async fn fork_spawns_a_read_only_branch_session() {
        // The fork is a real, independent BaseSession the critic can drive: the
        // fake `claude` replies to a judge directive with a JSON verdict and ends
        // the turn. Proves fork() spawns a usable session and the verdict streams.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nread _line\n\
             printf '%s\\n' '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"accepts\\\":true}\"}}}'\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}'\n\
             cat >/dev/null\n",
        );
        let mut main =
            ClaudeSession::start_with_program(fake.to_str().unwrap(), &tmp, None, "sid-main", true)
                .await
                .expect("start main");
        let mut fork = main
            .fork()
            .await
            .expect("fork must spawn a read-only branch");
        fork.send_turn("review from the architect seat, return JSON".to_string())
            .await
            .expect("fork send");
        let mut text = String::new();
        while let Some(ev) = fork.next_event().await {
            match ev {
                SessionEvent::TextDelta(t) => text.push_str(&t),
                SessionEvent::TurnDone { .. } => break,
                _ => {}
            }
        }
        assert!(
            text.contains("accepts"),
            "fork relayed the verdict text: {text}"
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
    fn parse_stream_event_yields_text_delta() {
        // `--include-partial-messages` makes claude stream text as content_block_delta
        // frames — the fix for the 60s-stall freeze on a plain chat reply.
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#;
        assert_eq!(
            parse_stdout_line(line),
            vec![SessionEvent::TextDelta("Hello".to_string())]
        );
        // A thinking / tool-arg delta is NOT surfaced as displayed text.
        let think = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#;
        assert!(parse_stdout_line(think).is_empty());
        // The session is actually launched with the flag (both main + fork).
        assert!(session_args("sid", None, false)
            .iter()
            .any(|a| a == "--include-partial-messages"));
        assert!(fork_session_args("m", "f")
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
        let mut s =
            ClaudeSession::start_with_program(fake.to_str().unwrap(), &tmp, None, "sid-test", true)
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
        let mut s =
            ClaudeSession::start_with_program(fake.to_str().unwrap(), &tmp, None, "sid-env", true)
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
}
