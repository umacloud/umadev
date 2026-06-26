//! `CodexSession` — the **continuous-session** driver for the `codex` base.
//!
//! This is the long-lived counterpart to [`crate::codex::CodexDriver`]. Where
//! `CodexDriver` is "one prompt → one text blob" (a fresh `codex exec`
//! subprocess per call), `CodexSession` keeps a SINGLE `codex app-server`
//! process alive for an entire 9-phase run: context flows research → docs →
//! code without re-priming, the base runs its own agentic tool loop (it WRITES
//! files), and the orchestrator observes a stream of tool-call / text / done
//! events. It implements [`umadev_runtime::BaseSession`].
//!
//! It does **not** replace `CodexDriver`; the two co-exist. The single-shot
//! `codex exec --json` path in `codex.rs` is untouched.
//!
//! # Wire protocol — `codex app-server` (JSON-RPC 2.0 over stdio)
//!
//! Verified against OpenAI's official Codex App Server documentation:
//!
//! - <https://developers.openai.com/codex/app-server>
//! - <https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md>
//!
//! Transport is **newline-delimited JSON** over the child's stdin/stdout. Per
//! the spec, messages are JSON-RPC 2.0 *with the `"jsonrpc":"2.0"` member
//! omitted on the wire* — so we neither send nor require it. The earlier
//! `codex proto` mode is deprecated; `codex app-server` is the current entry
//! point.
//!
//! Handshake (per the README's "Every connection must start with `initialize`
//! followed by `initialized`"):
//!
//! 1. `initialize` request `{clientInfo, capabilities}` → wait for its result.
//! 2. `initialized` notification (client → server, no id).
//! 3. `thread/start {model, cwd, approvalPolicy, sandbox}` → result carries
//!    `thread.id` + `thread.sessionId`.
//!
//! Per-phase injection (same thread = context flows):
//! `turn/start {threadId, input:[{type:"text", text:"<directive>"}]}`.
//!
//! Observed notifications (server → client, no id) → [`SessionEvent`]:
//! - `item/agentMessage/delta {delta}` → [`SessionEvent::TextDelta`].
//! - `item/completed` with item `type:"commandExecution"` (the `command`) /
//!   `type:"fileChange"` (the `changes[]` paths) → [`SessionEvent::ToolCall`] /
//!   [`SessionEvent::ToolResult`]. **This is the source of truth** for what the
//!   base actually did.
//! - `turn/completed {turn:{status}}` (`completed` / `interrupted` / `failed`)
//!   → [`SessionEvent::TurnDone`].
//!
//! Governance / gates: when `approvalPolicy` is left at `never` the base never
//! asks; at a gate the policy is non-`never` and the server sends a
//! server-initiated REQUEST `item/commandExecution/requestApproval` /
//! `item/fileChange/requestApproval` (has both `method` and `id`) which becomes
//! [`SessionEvent::NeedApproval`]; the reply is `{id, result:{approved: bool}}`.
//!
//! Control: `turn/interrupt {threadId, turnId}` (interrupt),
//! `turn/steer {threadId, turnId, input}` (queue input mid-turn),
//! `thread/fork {threadId, ephemeral:true}` (read-only critic fork),
//! `thread/resume {threadId}` (crash recovery).
//!
//! **Fail-open by contract:** any parse failure, a JSON-RPC `error` (e.g. the
//! `-32001` "overloaded" surface), or the child process dying mid-turn
//! surfaces a [`TurnStatus::Failed`] / `next_event` → `None`. The driver never
//! panics — a bug here must never crash the host.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use umadev_runtime::{
    ApprovalDecision, BaseSession, SessionError, SessionEvent, TurnStatus, Usage,
};

use crate::spawn_parts;

/// Program name for the codex base (overridable for tests / forward compat),
/// mirroring [`crate::codex::CodexDriver`]'s `UMADEV_CODEX_BIN`.
fn codex_program() -> String {
    std::env::var("UMADEV_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

/// The `app-server` subcommand (overridable). Per OpenAI's docs the long-lived
/// JSON-RPC host is launched as `codex app-server`.
fn codex_app_server_subcmd() -> String {
    std::env::var("UMADEV_CODEX_APP_SERVER_SUBCMD").unwrap_or_else(|_| "app-server".to_string())
}

/// How long [`interrupt`](CodexSession::interrupt) waits for the turn id to be
/// assigned (by the `turn/started` notification) before giving up, so an ESC that
/// races the turn-start handshake is honored rather than silently dropped (F5).
/// Bounded so an interrupt never blocks the host for long; `kill_on_drop` is the
/// final cancellation if the id never lands.
const INTERRUPT_TURN_ID_WAIT: Duration = Duration::from_millis(500);

/// Poll interval while waiting for the turn id in [`await_turn_id`].
const TURN_ID_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Shared map of outstanding client request ids → their result oneshot.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;
/// Shared map of approval `req_id` (string form) → the raw JSON-RPC id to echo.
type ApprovalMap = Arc<Mutex<HashMap<String, Value>>>;
/// Shared in-flight turn id (set by `turn/started`, cleared by `turn/completed`).
type TurnId = Arc<Mutex<Option<String>>>;
/// Shared latest REAL token usage seen on the live stream (F3).
///
/// codex streams per-turn usage in a SEPARATE `thread/tokenUsage/updated`
/// notification (and some versions inline it on `turn/completed`); the reader
/// stashes the most-recent parse here, and `emit_turn_done` drains it onto the
/// `TurnDone` so `/usage` is truthful on the DEFAULT loop. `None` until the base
/// reports usage → the consumer estimates instead (fail-open).
type LatestUsage = Arc<Mutex<Option<Usage>>>;
/// Sender half for translated session events.
type EventTx = mpsc::UnboundedSender<SessionEvent>;

/// A long-lived `codex app-server` session.
///
/// One per 9-phase run. The constructor performs the full
/// `initialize → initialized → thread/start` handshake; thereafter
/// [`send_turn`](BaseSession::send_turn) injects a directive per phase and
/// [`next_event`](BaseSession::next_event) drains the notification stream until
/// `turn/completed`.
pub struct CodexSession {
    /// Child stdin, shared with control methods (writes are line-framed JSON).
    stdin: Arc<Mutex<ChildStdin>>,
    /// Receiver for translated [`SessionEvent`]s produced by the reader task.
    events: mpsc::UnboundedReceiver<SessionEvent>,
    /// Map: outstanding client request id → oneshot for its JSON-RPC result.
    /// Shared with the reader task, which completes the oneshot on the matching
    /// response line.
    pending: PendingMap,
    /// Map: a `NeedApproval` `req_id` (the string form of the server request id)
    /// → the raw JSON id we must echo back in the reply. Populated by the reader
    /// when it sees a server-initiated `requestApproval`.
    approvals: ApprovalMap,
    /// Monotonic client-request id counter.
    next_id: AtomicI64,
    /// The codex thread id from `thread/start` (`thread.id`).
    thread_id: String,
    /// The id of the in-flight turn, captured from `turn/started` /
    /// `turn/start`'s result; needed for `turn/interrupt` / `turn/steer`.
    /// `Mutex` because the reader updates it while control methods read it.
    turn_id: TurnId,
    /// The resolved `codex` program, kept so a read-only
    /// [`fork`](BaseSession::fork) spawns the SAME binary (honoring a test fake /
    /// `UMADEV_CODEX_BIN`).
    program: String,
    /// The workspace, so a fork resumes the thread in the same project dir.
    workspace: std::path::PathBuf,
    /// The model id this session was started with, forwarded to a fork's
    /// `thread/resume` so the critic uses the same brain.
    model: String,
    /// Keep the child handle so it is killed on drop (`kill_on_drop`).
    _child: tokio::process::Child,
}

impl CodexSession {
    /// Start a continuous `codex app-server` session in `workspace` and run the
    /// full handshake. `model` is forwarded to `thread/start` (an empty / non-
    /// codex model id is dropped so codex falls back to the account default,
    /// mirroring [`crate::codex::CodexDriver`]'s `codex_model_args`). `autonomous`
    /// chooses `approvalPolicy`: `true` → `"never"` (the base writes code
    /// unattended, governed by UmaDev's own rules), `false` → `"on-request"`
    /// (the server raises `requestApproval` at gates).
    ///
    /// Fail-open: a spawn failure / a missing handshake result surfaces a
    /// [`SessionError::Start`], never a panic.
    pub async fn start(
        workspace: &Path,
        model: &str,
        autonomous: bool,
    ) -> Result<Self, SessionError> {
        Self::start_with_program(&codex_program(), workspace, model, autonomous).await
    }

    /// Like [`start`](Self::start) but with the codex binary passed explicitly,
    /// so tests can point at a fake `app-server` script without touching the
    /// process-global `UMADEV_CODEX_BIN` env var (which races under parallel
    /// test execution — a sibling test's `remove_var` could be observed first,
    /// falling back to a real installed `codex` and a different error).
    async fn start_with_program(
        program: &str,
        workspace: &Path,
        model: &str,
        autonomous: bool,
    ) -> Result<Self, SessionError> {
        let mut child = spawn_app_server(program, workspace)?;
        let stdin = take_pipe(child.stdin.take(), "stdin")?;
        let stdout = take_pipe(child.stdout.take(), "stdout")?;
        // Drain stderr on its own task so a chatty base can never fill (and then
        // block on) its stderr pipe. We only log it at debug.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(stderr));
        }

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let approvals: ApprovalMap = Arc::new(Mutex::new(HashMap::new()));
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage: LatestUsage = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Reader task: the single owner of stdout. Splits every line into
        // response / server-request / notification (see `reader_loop`).
        tokio::spawn(reader_loop(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&approvals),
            Arc::clone(&turn_id),
            latest_usage,
            event_tx,
        ));

        let mut session = Self {
            stdin,
            events: event_rx,
            pending,
            approvals,
            next_id: AtomicI64::new(1),
            thread_id: String::new(),
            turn_id,
            program: program.to_string(),
            workspace: workspace.to_path_buf(),
            model: model.to_string(),
            _child: child,
        };
        session.handshake(workspace, model, autonomous).await?;
        Ok(session)
    }

    /// Start a READ-ONLY critic fork: a fresh, independent `codex app-server`
    /// that RESUMES the main thread (`fork_thread_id`) in a read-only sandbox.
    ///
    /// Forking onto its OWN process means the critic can never collide with the
    /// main writer session's in-flight turn (single-writer invariant), and
    /// `sandbox:"read-only"` + `approvalPolicy:"never"` fence it so it can read the
    /// blackboard + the prior context but can NEVER write a file. Resuming the
    /// main thread id gives the seat the main line's accumulated context.
    ///
    /// Fail-open: a spawn / handshake failure surfaces as [`SessionError::Start`],
    /// which the caller treats exactly like `ForkUnsupported` (degrade, never
    /// block).
    async fn start_fork(
        program: &str,
        workspace: &Path,
        model: &str,
        fork_thread_id: &str,
    ) -> Result<Self, SessionError> {
        let mut child = spawn_app_server(program, workspace)?;
        let stdin = take_pipe(child.stdin.take(), "stdin")?;
        let stdout = take_pipe(child.stdout.take(), "stdout")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(stderr));
        }
        let stdin = Arc::new(Mutex::new(stdin));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let approvals: ApprovalMap = Arc::new(Mutex::new(HashMap::new()));
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage: LatestUsage = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(reader_loop(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&approvals),
            Arc::clone(&turn_id),
            latest_usage,
            event_tx,
        ));
        let session = Self {
            stdin,
            events: event_rx,
            pending,
            approvals,
            next_id: AtomicI64::new(1),
            thread_id: fork_thread_id.to_string(),
            turn_id,
            program: program.to_string(),
            workspace: workspace.to_path_buf(),
            model: model.to_string(),
            _child: child,
        };
        session.fork_handshake(fork_thread_id).await?;
        Ok(session)
    }

    /// Run `initialize → initialized → thread/resume` for a read-only fork.
    async fn fork_handshake(&self, fork_thread_id: &str) -> Result<(), SessionError> {
        self.request("initialize", &initialize_params())
            .await
            .map_err(|e| SessionError::Start(format!("codex fork initialize: {e}")))?;
        self.notify("initialized", json!({}))
            .await
            .map_err(|e| SessionError::Start(format!("codex fork initialized: {e}")))?;
        // Resume the main thread on this independent server, read-only.
        self.request(
            "thread/resume",
            &thread_resume_params(fork_thread_id, &self.workspace, &self.model),
        )
        .await
        .map_err(|e| SessionError::Start(format!("codex thread/resume: {e}")))?;
        Ok(())
    }

    /// Run `initialize → initialized → thread/start` and capture `thread.id`.
    async fn handshake(
        &mut self,
        workspace: &Path,
        model: &str,
        autonomous: bool,
    ) -> Result<(), SessionError> {
        // 1. initialize. `clientInfo` identifies us; we request no experimental
        //    capabilities (the base default behaviour is what we drive).
        self.request("initialize", &initialize_params())
            .await
            .map_err(|e| SessionError::Start(format!("codex initialize: {e}")))?;

        // 2. initialized notification (client → server, no id, no result).
        self.notify("initialized", json!({}))
            .await
            .map_err(|e| SessionError::Start(format!("codex initialized: {e}")))?;

        // 3. thread/start. `sandbox:"workspace-write"` + `approvalPolicy:"never"`
        //    is the autonomous "write code without asking" tier; the gate tier
        //    uses `on-request` so the server raises `requestApproval`.
        let started = self
            .request(
                "thread/start",
                &thread_start_params(workspace, model, autonomous),
            )
            .await
            .map_err(|e| SessionError::Start(format!("codex thread/start: {e}")))?;
        self.thread_id = extract_thread_id(&started)?;
        Ok(())
    }

    /// Allocate the next monotonic client-request id.
    fn alloc_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Return the in-flight turn id, waiting up to `budget` for the `turn/started`
    /// notification to assign it (F5). Returns immediately if the id is already
    /// known; returns `None` if the window elapses with no id. Bounded + fail-open:
    /// this never blocks longer than `budget`, so an interrupt can't wedge.
    async fn await_turn_id(&self, budget: Duration) -> Option<String> {
        await_turn_id_in(&self.turn_id, budget).await
    }

    /// Write a single JSON value as one newline-delimited line to the child's
    /// stdin. The `"jsonrpc"` member is intentionally omitted (the app-server
    /// expects it absent on the wire).
    async fn write_line(&self, value: &Value) -> Result<(), SessionError> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| SessionError::Send(format!("serialize: {e}")))?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| SessionError::Send(format!("write stdin: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| SessionError::Send(format!("flush stdin: {e}")))?;
        Ok(())
    }

    /// Register a oneshot for request `id` and return its receiver. The reader
    /// task completes it when the matching response line arrives.
    async fn register(&self, id: i64) -> oneshot::Receiver<Result<Value, String>> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        rx
    }

    /// Send a JSON-RPC request and await its result (or a JSON-RPC error mapped
    /// to `Err`).
    async fn request(&self, method: &str, params: &Value) -> Result<Value, String> {
        let id = self.alloc_id();
        let rx = self.register(id).await;
        let msg = rpc_request(id, method, params);
        if let Err(e) = self.write_line(&msg).await {
            self.pending.lock().await.remove(&id);
            return Err(e.to_string());
        }
        match rx.await {
            Ok(result) => result,
            // The sender was dropped without sending → the session died.
            Err(_) => Err("codex app-server closed before responding".to_string()),
        }
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<(), SessionError> {
        self.write_line(&json!({ "method": method, "params": params }))
            .await
    }
}

/// Adopt the turn id from a `turn/start` result into the shared slot, unless one
/// is already set (the `turn/started` notification may have raced ahead). A free
/// function (not a method) so the F4 background adopt-task can run it without
/// borrowing the session. Fail-open: a result with no `turn.id` is a no-op.
async fn adopt_turn_id_into(turn_id: &TurnId, result: &Value) {
    let Some(id) = turn_id_of(result) else {
        return;
    };
    let mut guard = turn_id.lock().await;
    if guard.is_none() {
        *guard = Some(id);
    }
}

/// Poll the shared turn-id slot for up to `budget`, returning the id the moment it
/// appears (set by the `turn/started` notification) or `None` if the window
/// elapses (F5). A free function so it is unit-testable without a live session.
/// Bounded + fail-open: never blocks longer than `budget`.
async fn await_turn_id_in(turn_id: &TurnId, budget: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(id) = turn_id.lock().await.clone() {
            return Some(id);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        // The reader task sets `turn_id` on `turn/started`; poll for it rather than
        // block on a channel we don't own here. Cap each sleep at the remaining time
        // so we never overshoot the budget.
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(TURN_ID_POLL_INTERVAL.min(remaining)).await;
    }
}

/// Build the `initialize` params. `clientInfo` identifies UmaDev; we request no
/// experimental capabilities.
fn initialize_params() -> Value {
    let client_info = json!({
        "name": "umadev",
        "title": "UmaDev",
        "version": env!("CARGO_PKG_VERSION"),
    });
    json!({ "clientInfo": client_info, "capabilities": {} })
}

/// Build the `thread/start` params for `workspace` / `model` / autonomy tier.
fn thread_start_params(workspace: &Path, model: &str, autonomous: bool) -> Value {
    let approval_policy = if autonomous { "never" } else { "on-request" };
    let mut params = json!({
        "cwd": workspace.to_string_lossy(),
        "approvalPolicy": approval_policy,
        // codex's sandbox enum is KEBAB-case (`read-only` / `workspace-write` /
        // `danger-full-access`), matching its `--sandbox` CLI flag. We sent camelCase
        // (`workspaceWrite`), which newer codex rejects with `unknown variant
        // 'workspaceWrite'`, killing the continuous session (user-reported on Windows).
        "sandbox": "workspace-write",
    });
    if let Some(m) = codex_model(model) {
        params["model"] = json!(m);
    }
    params
}

/// Build the `thread/resume` params for a READ-ONLY critic fork: resume
/// `thread_id` in `workspace` with `sandbox:"read-only"` + `approvalPolicy:"never"`
/// so the seat reads context + the blackboard but can NEVER write a file (the
/// single-writer invariant). The model is forwarded only when codex-native.
fn thread_resume_params(thread_id: &str, workspace: &Path, model: &str) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "cwd": workspace.to_string_lossy(),
        "approvalPolicy": "never",
        // Kebab-case (see `thread_start_params`): `readOnly` → `read-only`.
        "sandbox": "read-only",
    });
    if let Some(m) = codex_model(model) {
        params["model"] = json!(m);
    }
    params
}

/// Build the `thread/fork` params: branch `thread_id` into an EPHEMERAL thread
/// (`ephemeral:true`) for a read-only critic review — the new branch is
/// throwaway and never mutates the main line.
fn thread_fork_params(thread_id: &str) -> Value {
    json!({ "threadId": thread_id, "ephemeral": true })
}

/// A JSON-RPC request envelope (the `"jsonrpc"` member is omitted on the wire).
fn rpc_request(id: i64, method: &str, params: &Value) -> Value {
    json!({ "id": id, "method": method, "params": params })
}

/// Pull `thread.id` out of a `thread/start` result, or a `Start` error.
fn extract_thread_id(result: &Value) -> Result<String, SessionError> {
    result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            SessionError::Start("codex thread/start: result missing thread.id".to_string())
        })
}

/// Pull `turn.id` out of a `turn/start` result or `turn/*` notification params.
fn turn_id_of(value: &Value) -> Option<String> {
    value
        .get("turn")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Unwrap an optional child pipe, mapping `None` to a `Start` error.
fn take_pipe<T>(pipe: Option<T>, which: &str) -> Result<T, SessionError> {
    pipe.ok_or_else(|| SessionError::Start(format!("codex app-server: no {which} pipe")))
}

/// Spawn `codex app-server` in `workspace` with piped stdio + kill-on-drop.
/// Windows `.cmd`/`.bat` shims are routed through `cmd /c` by [`spawn_parts`].
fn spawn_app_server(
    program: &str,
    workspace: &Path,
) -> Result<tokio::process::Child, SessionError> {
    let (prog, lead) = spawn_parts(program);
    let mut cmd = Command::new(prog);
    cmd.args(&lead);
    cmd.arg(codex_app_server_subcmd());
    cmd.current_dir(workspace);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.spawn().map_err(|e| spawn_error(program, &e))
}

/// Render a spawn failure into a `Start` error (NotFound vs other).
fn spawn_error(program: &str, e: &std::io::Error) -> SessionError {
    if e.kind() == std::io::ErrorKind::NotFound {
        SessionError::Start(format!("`{program}` not found on PATH"))
    } else {
        SessionError::Start(format!("failed to spawn `{program} app-server`: {e}"))
    }
}

/// Drain the child's stderr at debug level so it never backpressures the base.
async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        tracing::debug!(target: "codex_app_server_stderr", "{line}");
    }
}

/// The reader task body: own stdout, dispatch every line, and on EOF / read
/// error fail-open (emit a `Failed` `TurnDone` and wake every pending waiter).
async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    approvals: ApprovalMap,
    turn_id: TurnId,
    latest_usage: LatestUsage,
    event_tx: EventTx,
) {
    let mut reader = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        dispatch_line(
            &line,
            &pending,
            &approvals,
            &turn_id,
            &latest_usage,
            &event_tx,
        )
        .await;
    }
    // EOF or a read error → the app-server is gone. Tell any in-flight turn it
    // failed (fail-open) and wake every pending request so no caller hangs.
    let _ = event_tx.send(SessionEvent::TurnDone {
        status: TurnStatus::Failed("codex app-server stdout closed".to_string()),
        usage: None,
    });
    let mut guard = pending.lock().await;
    for (_, tx) in guard.drain() {
        let _ = tx.send(Err("codex app-server closed".to_string()));
    }
}

/// Map UmaDev's pipeline model id onto a codex-acceptable one, or `None`.
///
/// Mirrors [`crate::codex::CodexDriver`]'s `codex_model_args`: codex on a
/// ChatGPT account rejects non-codex model ids (the pipeline default is
/// claude-centric, e.g. `claude-sonnet-4-6`), so a non-codex id is dropped and
/// codex falls back to the account default. Codex-native ids (`gpt-*`,
/// `codex-*`, `o1`/`o3`/`o4`) are forwarded verbatim.
fn codex_model(model: &str) -> Option<String> {
    let m = model.trim().to_ascii_lowercase();
    let native = m.starts_with("gpt")
        || m.starts_with("codex")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4");
    if native && !model.trim().is_empty() {
        Some(model.trim().to_string())
    } else {
        None
    }
}

/// Classify and route one stdout line from the app-server.
///
/// JSON-RPC framing rule (per the spec, `"jsonrpc"` omitted):
/// - has `id` + (`result` | `error`), no `method` → a **response** to one of our
///   requests → complete the matching `pending` oneshot.
/// - has `method` + `id` → a **server-initiated request** (an approval ask) →
///   translate to [`SessionEvent::NeedApproval`] and stash the id for the reply.
/// - has `method`, no `id` → a **notification** → translate to a [`SessionEvent`].
///
/// Fail-open: a non-JSON / unrecognised line is logged at debug and dropped.
async fn dispatch_line(
    line: &str,
    pending: &PendingMap,
    approvals: &ApprovalMap,
    turn_id: &TurnId,
    latest_usage: &LatestUsage,
    event_tx: &EventTx,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        tracing::debug!(target: "codex_app_server", "non-JSON line dropped: {trimmed}");
        return;
    };
    let has_method = v.get("method").and_then(Value::as_str).is_some();
    let has_id = v.get("id").is_some();
    if !has_method && has_id {
        complete_response(&v, pending).await;
    } else if has_method && has_id {
        handle_server_request(&v, approvals, event_tx).await;
    } else if has_method {
        handle_notification(&v, turn_id, latest_usage, event_tx).await;
    }
}

/// Route a response line (`{id, result|error}`) to its waiting oneshot.
async fn complete_response(v: &Value, pending: &PendingMap) {
    let Some(id) = v.get("id").and_then(Value::as_i64) else {
        return;
    };
    let Some(tx) = pending.lock().await.remove(&id) else {
        return;
    };
    let _ = tx.send(response_payload(v));
}

/// Map a response value to `Ok(result)` or `Err(jsonrpc error)`.
fn response_payload(v: &Value) -> Result<Value, String> {
    if let Some(err) = v.get("error") {
        // JSON-RPC error object, e.g. {"code":-32001,"message":"overloaded"}.
        Err(format!("jsonrpc error: {err}"))
    } else {
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Translate a server-initiated `requestApproval` request into a
/// [`SessionEvent::NeedApproval`], stashing its raw id so the reply correlates.
async fn handle_server_request(v: &Value, approvals: &ApprovalMap, event_tx: &EventTx) {
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    let raw_id = v.get("id").cloned().unwrap_or(Value::Null);
    // The `req_id` we hand the orchestrator is the string form of the raw id;
    // `respond` reverses it back to the raw id for the reply.
    let req_id = json_id_key(&raw_id);
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    let (action, target) = approval_action_target(method, &params);
    approvals.lock().await.insert(req_id.clone(), raw_id);
    let _ = event_tx.send(SessionEvent::NeedApproval {
        req_id,
        action,
        target,
    });
}

/// Derive the `(action, target)` pair for a `requestApproval` method.
fn approval_action_target(method: &str, params: &Value) -> (String, String) {
    match method {
        // codex asks before running a command ...
        "item/commandExecution/requestApproval" => ("Bash".to_string(), command_of(params)),
        // ... or before editing a file (`filePath` / `changes[].path`).
        "item/fileChange/requestApproval" => ("Write".to_string(), file_change_path(params)),
        // An unknown approval shape: still surface it (default-deny upstream is
        // safe) rather than silently swallow a pending server request.
        _ => (method.to_string(), String::new()),
    }
}

/// The `command` string of a command-execution payload.
fn command_of(value: &Value) -> String {
    value
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The target file path of a fileChange approval / item payload
/// (`filePath`, else the first `changes[].path`).
fn file_change_path(params: &Value) -> String {
    if let Some(p) = params.get("filePath").and_then(Value::as_str) {
        return p.to_string();
    }
    first_change_field(params, "path")
}

/// Pull a field off the first entry of a `changes[]` array.
fn first_change_field(value: &Value, field: &str) -> String {
    value
        .get("changes")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get(field))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Translate a notification (no id) into zero or more [`SessionEvent`]s.
async fn handle_notification(
    v: &Value,
    turn_id: &TurnId,
    latest_usage: &LatestUsage,
    event_tx: &EventTx,
) {
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    match method {
        // Capture the in-flight turn id so interrupt / steer can target it.
        "turn/started" => set_turn_id(turn_id, turn_id_of(&params)).await,
        // Streamed assistant text.
        "item/agentMessage/delta" => emit_text_delta(&params, event_tx),
        // A completed item — the SOURCE OF TRUTH for produced work.
        "item/completed" => emit_completed_item(&params, event_tx),
        // F3: codex streams per-turn token usage in this dedicated notification
        // (kept separate from `turn/completed` so the protocol shape stays stable).
        // Stash the latest parse so `emit_turn_done` can attach the REAL usage.
        "thread/tokenUsage/updated" => capture_usage(&params, latest_usage).await,
        // The turn ended — the authoritative phase-done boundary.
        "turn/completed" => emit_turn_done(&params, turn_id, latest_usage, event_tx).await,
        // turn/diff/updated, thread/started, item/started, fs/changed, … carry
        // no event we surface — ignored (fail-open).
        _ => {}
    }
}

/// Stash the REAL token usage from a `thread/tokenUsage/updated` notification so
/// the next `turn/completed` can carry it. Fail-open: an unparseable payload is a
/// no-op (the prior value, if any, stands; absent → the consumer estimates).
async fn capture_usage(params: &Value, latest_usage: &LatestUsage) {
    if let Some(u) = parse_codex_usage(params) {
        *latest_usage.lock().await = Some(u);
    }
}

/// Parse a codex token-usage payload into [`Usage`], defensively.
///
/// codex's app-server protocol is not pinned here and its versions have moved the
/// usage object around / between snake_case and camelCase, so we probe the likely
/// nestings (`usage`, `info.usage`, `turn.usage`, `tokenUsage`, the payload root)
/// and fold cached input into input + reasoning output into output (mirroring the
/// legacy [`crate::codex`] `extract_codex_usage`). `None` when nothing usable is
/// found → the consumer falls back to a `chars/4` estimate (fail-open).
fn parse_codex_usage(payload: &Value) -> Option<Usage> {
    let obj = codex_usage_object(payload)?;
    // Accept both snake_case and camelCase field spellings.
    let field = |snake: &str, camel: &str| -> u64 {
        obj.get(snake)
            .or_else(|| obj.get(camel))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let input =
        field("input_tokens", "inputTokens") + field("cached_input_tokens", "cachedInputTokens");
    let output = field("output_tokens", "outputTokens")
        + field("reasoning_output_tokens", "reasoningOutputTokens");
    // A payload that matched a candidate object but carried no recognizable token
    // field is not real usage → estimate instead of recording a spurious zero.
    if input == 0 && output == 0 {
        return None;
    }
    Some(Usage {
        input_tokens: u32::try_from(input).unwrap_or(u32::MAX),
        output_tokens: u32::try_from(output).unwrap_or(u32::MAX),
    })
}

/// Find the object that actually holds the token-count fields. **Prefers the
/// PER-TURN delta (`last_token_usage`) over the cumulative `total_token_usage`**:
/// the consumer appends every turn's usage to `usage.jsonl` and `/usage` SUMS the
/// rows, so recording the cumulative each turn would overcount (~O(N²) across N
/// turns). Verified against a real `~/.codex` rollout — codex nests the counts
/// under `info.{last,total}_token_usage`, each `{input_tokens, cached_input_tokens,
/// output_tokens, reasoning_output_tokens, total_tokens}`; the old probe read
/// `tokenUsage`/`usage` DIRECTLY and so saw zero token fields → always estimated.
/// Fail-open: `None` if nothing usable matches → the consumer estimates.
fn codex_usage_object(payload: &Value) -> Option<&Value> {
    // The per-turn delta object, across the spellings codex has used.
    fn per_turn(obj: &Value) -> Option<&Value> {
        obj.get("last_token_usage")
            .or_else(|| obj.get("lastTokenUsage"))
            .or_else(|| obj.get("last"))
    }
    // A flat object that carries the token fields directly (legacy / `codex exec`).
    fn is_flat(v: &Value) -> bool {
        v.get("input_tokens").is_some() || v.get("inputTokens").is_some()
    }
    // 1) Per-turn delta: at the params root, then under a wrapper.
    if let Some(u) = per_turn(payload) {
        return Some(u);
    }
    for parent in ["usage", "info", "turn", "tokenUsage", "token_usage"] {
        if let Some(u) = payload.get(parent).and_then(per_turn) {
            return Some(u);
        }
    }
    // 2) Legacy flat fallback — only an object that truly has token fields, so a
    //    `{last,total}` wrapper is never mistaken for a flat usage object.
    for key in ["usage", "tokenUsage", "token_usage"] {
        if let Some(u) = payload.get(key) {
            if is_flat(u) {
                return Some(u);
            }
        }
    }
    for parent in ["info", "turn"] {
        if let Some(u) = payload.get(parent).and_then(|p| p.get("usage")) {
            if is_flat(u) {
                return Some(u);
            }
        }
    }
    if is_flat(payload) {
        return Some(payload);
    }
    None
}

/// Overwrite the shared turn id (used on `turn/started`).
async fn set_turn_id(turn_id: &TurnId, id: Option<String>) {
    if let Some(id) = id {
        *turn_id.lock().await = Some(id);
    }
}

/// Emit a [`SessionEvent::TextDelta`] from an `item/agentMessage/delta` payload.
fn emit_text_delta(params: &Value, event_tx: &EventTx) {
    let Some(delta) = params.get("delta").and_then(Value::as_str) else {
        return;
    };
    if !delta.is_empty() {
        let _ = event_tx.send(SessionEvent::TextDelta(delta.to_string()));
    }
}

/// Dispatch an `item/completed` payload to the per-item translators.
fn emit_completed_item(params: &Value, event_tx: &EventTx) {
    let Some(item) = params.get("item") else {
        return;
    };
    emit_item(item, event_tx);
}

/// Map a completed `item` to a [`SessionEvent::ToolCall`] (+ `ToolResult`).
///
/// codex item `type`s of interest (per the App Server docs):
/// - `commandExecution` → `Bash`, input `{command}`; result from `status` +
///   `exitCode`.
/// - `fileChange` → `Write`/`Edit` (new file = `add`, else `update`), input the
///   first changed path; result from `status`.
///
/// `agentMessage` / `reasoning` / `plan` / `webSearch` / `mcpToolCall` etc. are
/// not surfaced here (text already streams via `item/agentMessage/delta`).
fn emit_item(item: &Value, event_tx: &EventTx) {
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "commandExecution" => emit_command_execution(item, event_tx),
        "fileChange" => emit_file_change(item, event_tx),
        _ => {}
    }
}

/// Translate a completed `commandExecution` item → Bash `ToolCall` + result.
fn emit_command_execution(item: &Value, event_tx: &EventTx) {
    let command = command_of(item);
    let _ = event_tx.send(SessionEvent::ToolCall {
        name: "Bash".to_string(),
        input: json!({ "command": command }),
    });
    // status: completed | failed | declined.
    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
    let exit_ok = item
        .get("exitCode")
        .and_then(Value::as_i64)
        .map_or(status == "completed", |c| c == 0);
    let summary = item
        .get("aggregatedOutput")
        .and_then(Value::as_str)
        .unwrap_or(status);
    let _ = event_tx.send(SessionEvent::ToolResult {
        ok: status != "failed" && status != "declined" && exit_ok,
        summary: truncate(summary, 200),
    });
}

/// Translate a completed `fileChange` item → Write/Edit `ToolCall` + result.
fn emit_file_change(item: &Value, event_tx: &EventTx) {
    // changes: [{path, kind, diff}]. kind `add` = new file → Write, else Edit.
    // (codex `PatchChangeKind` serializes add/update/delete.)
    let path = first_change_field(item, "path");
    let kind = first_change_kind(item);
    let name = if kind == "add" || kind == "create" {
        "Write"
    } else {
        "Edit"
    };
    // CONTENT for content-governance (emoji / hardcoded color / secret / AI-slop).
    // codex's `fileChange` item does NOT carry the new file text in a `content`
    // field — it carries a unified `diff`. The orchestrator's `evaluate_tool_call`
    // scans `input.content` / `input.new_string`, which would be EMPTY for codex,
    // so codex writes would dodge the content scan entirely (and codex has no
    // PreToolUse hook to backstop it). We extract the ADDED lines from the diff
    // and surface them as `content` so the same scanner sees the real written text.
    // Best-effort: when no diff is present the field is simply absent (the scanner
    // then degrades to path-only, exactly as before — fail-open, never a panic).
    let added = first_change_added_content(item);
    let input = if added.is_empty() {
        json!({ "file_path": path })
    } else {
        json!({ "file_path": path, "content": added })
    };
    let _ = event_tx.send(SessionEvent::ToolCall {
        name: name.to_string(),
        input,
    });
    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
    let _ = event_tx.send(SessionEvent::ToolResult {
        ok: status != "failed" && status != "declined",
        summary: truncate(&path, 200),
    });
}

/// The `kind` of the first `changes[]` entry (defaults to `update`).
fn first_change_kind(item: &Value) -> String {
    let k = first_change_field(item, "kind");
    if k.is_empty() {
        "update".to_string()
    } else {
        k
    }
}

/// The new file CONTENT a `fileChange` item wrote, recovered for content
/// governance. codex's item does not expose a plain `content` field — it exposes
/// either a `content` (some shapes do), a unified `diff` string, or `changes[]`
/// entries each carrying their own `content`/`diff`. We prefer an explicit
/// `content`, else reconstruct the ADDED text from the diff(s): the lines a
/// unified diff prefixes with `+` (excluding the `+++` file header) ARE the new
/// content, which is exactly what the emoji / color / secret / AI-slop scanner
/// needs to see. Pure + fail-open: an absent/odd shape yields `String::new()`
/// (the scanner then degrades to path-only), never a panic.
fn first_change_added_content(item: &Value) -> String {
    // An explicit top-level `content` (rare but cheapest) wins.
    if let Some(c) = item.get("content").and_then(Value::as_str) {
        if !c.is_empty() {
            return c.to_string();
        }
    }
    // Otherwise reconstruct from every `changes[]` entry's content/diff. We fold
    // ALL entries (a single item can touch multiple hunks) so nothing escapes.
    let mut out = String::new();
    if let Some(changes) = item.get("changes").and_then(Value::as_array) {
        for ch in changes {
            if let Some(c) = ch.get("content").and_then(Value::as_str) {
                push_block(&mut out, c);
            } else if let Some(diff) = ch.get("diff").and_then(Value::as_str) {
                push_block(&mut out, &added_lines_of_diff(diff));
            }
        }
    }
    // A top-level `diff` (some item shapes put it there) as a final fallback.
    if out.is_empty() {
        if let Some(diff) = item.get("diff").and_then(Value::as_str) {
            out = added_lines_of_diff(diff);
        }
    }
    out
}

/// Append `block` to `acc` separated by a newline (skips empties).
fn push_block(acc: &mut String, block: &str) {
    if block.is_empty() {
        return;
    }
    if !acc.is_empty() {
        acc.push('\n');
    }
    acc.push_str(block);
}

/// Extract the ADDED lines from a unified diff: every line starting with a single
/// `+` (but NOT the `+++` new-file header), with the leading `+` stripped. A
/// string that is not a diff at all (no `@@`/`+++`/`---` markers, no `+`-prefixed
/// lines) is returned verbatim so a base that already hands us plain content
/// still gets scanned. Pure.
fn added_lines_of_diff(diff: &str) -> String {
    // If there is no diff structure AND no added-line markers, treat it as plain
    // content (some bases put the raw new text in the `diff` field).
    let looks_like_diff = diff.lines().any(|l| {
        l.starts_with("@@")
            || l.starts_with("+++")
            || l.starts_with("---")
            || l.starts_with('+')
            || l.starts_with('-')
    });
    if !looks_like_diff {
        return diff.to_string();
    }
    let mut out = String::new();
    for line in diff.lines() {
        // `+++ b/path` is a header, not content. A bare `+` line is added content.
        if let Some(rest) = line.strip_prefix('+') {
            if rest.starts_with("++") {
                continue; // the `+++` file header
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(rest);
        }
    }
    out
}

/// Emit a [`SessionEvent::TurnDone`] from a `turn/completed` payload and clear
/// the in-flight turn id.
///
/// F3: attach the REAL per-turn token usage so `/usage` is truthful on the
/// DEFAULT loop. Prefer usage inlined on the `turn/completed` params (some codex
/// versions carry it there); otherwise drain the latest `thread/tokenUsage/updated`
/// value. The accumulator is reset to `None` either way so a stale count can't
/// leak into the NEXT turn. Fail-open: no usage anywhere → `None` (estimate).
async fn emit_turn_done(
    params: &Value,
    turn_id: &TurnId,
    latest_usage: &LatestUsage,
    event_tx: &EventTx,
) {
    let status = params
        .get("turn")
        .and_then(|t| t.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("completed");
    *turn_id.lock().await = None;
    // Inline usage on the completion wins; else take (and clear) the streamed one.
    let inline = parse_codex_usage(params);
    let usage = {
        let mut guard = latest_usage.lock().await;
        let streamed = guard.take();
        inline.or(streamed)
    };
    let _ = event_tx.send(SessionEvent::TurnDone {
        status: map_turn_status(status, params),
        usage,
    });
}

/// Map a codex turn `status` string to a [`TurnStatus`].
fn map_turn_status(status: &str, params: &Value) -> TurnStatus {
    match status {
        "interrupted" => TurnStatus::Interrupted,
        "failed" => TurnStatus::Failed(turn_error_message(params)),
        // `"completed"` AND any unknown status are treated as a clean finish
        // boundary rather than a hang (fail-open: a phase must still terminate).
        _ => TurnStatus::Completed,
    }
}

/// Extract a human-readable failure reason from a failed `turn/completed`.
/// failures carry `{turn:{error:{message}}}` (or a top-level `error`).
fn turn_error_message(params: &Value) -> String {
    error_message_at(params.get("turn"))
        .or_else(|| error_message_at(Some(params)))
        .unwrap_or_else(|| "codex turn failed".to_string())
}

/// `error.message` of an optional object value.
fn error_message_at(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Stable string key for a JSON-RPC id (number or string), used to correlate a
/// `NeedApproval` `req_id` back to the raw id for the reply.
fn json_id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Truncate `s` to at most `max` chars on a UTF-8 boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[async_trait]
impl BaseSession for CodexSession {
    async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
        // Ask the live app-server to fork the main thread into an EPHEMERAL
        // branch (`thread/fork {threadId, ephemeral:true}`), so the critic
        // reviews a snapshot that never affects the main line. If the running
        // base doesn't support `thread/fork`, fall back to resuming the main
        // thread id directly — still read-only + on its own server, so still
        // isolated. Either way the critic runs on a SEPARATE read-only process.
        let fork_thread_id = match self
            .request("thread/fork", &thread_fork_params(&self.thread_id))
            .await
        {
            Ok(result) => extract_thread_id(&result).unwrap_or_else(|_| self.thread_id.clone()),
            // `thread/fork` unsupported / errored → resume the main thread
            // read-only instead (fail-open, still isolated + non-writing).
            Err(_) => self.thread_id.clone(),
        };
        let s =
            Self::start_fork(&self.program, &self.workspace, &self.model, &fork_thread_id).await?;
        Ok(Box::new(s))
    }

    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        // turn/start {threadId, input:[{type:"text", text}]}. Same thread =
        // context flows from the previous phase. We send it as a request so a
        // transport failure on the WRITE surfaces immediately.
        let id = self.alloc_id();
        let rx = self.register(id).await;
        let msg = rpc_request(
            id,
            "turn/start",
            &turn_start_params(&self.thread_id, &directive),
        );
        self.write_line(&msg).await?;
        // F4: do NOT inline-await the `turn/start` RESULT here — that coupled the
        // send latency to the server's response timing (claude / opencode return
        // from `send_turn` immediately). The turn id is captured the moment the
        // `turn/started` notification lands (see `set_turn_id`); we still adopt it
        // from the start RESULT too (whichever arrives first wins) on a background
        // task so the registered `pending` oneshot is consumed (never leaked) and
        // `send_turn` returns at once. Fail-open: a dropped sender / missing id is
        // a silent no-op — the notification path already set the id.
        let turn_id = Arc::clone(&self.turn_id);
        tokio::spawn(async move {
            if let Ok(Ok(result)) = rx.await {
                adopt_turn_id_into(&turn_id, &result).await;
            }
        });
        Ok(())
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        self.events.recv().await
    }

    async fn respond(
        &mut self,
        req_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError> {
        // Reverse the req_id back to the raw JSON-RPC id and reply
        // {id, result:{approved}}. If we have no record of it (already answered
        // / unknown), fail-open: nothing to do.
        let raw_id = self.approvals.lock().await.remove(req_id);
        let Some(raw_id) = raw_id else {
            return Ok(());
        };
        let approved = matches!(decision, ApprovalDecision::Allow);
        self.write_line(&approval_reply(&raw_id, approved)).await
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        // turn/interrupt {threadId, turnId}.
        //
        // F5: an early ESC (after `send_turn`, but BEFORE the `turn/started`
        // notification has assigned a turn id) used to be SILENTLY swallowed —
        // `turn_id == None` → `Ok(())` no-op — so the user's interrupt was lost
        // and the turn ran on (claude / opencode interrupt unconditionally). Now
        // we make a best-effort attempt: briefly wait for the turn id to appear
        // (the reader sets it the instant `turn/started` lands), then interrupt.
        // Bounded + fail-open: if no id arrives within the window we give up and
        // return Ok (the session's `kill_on_drop` is the final cancellation), but
        // we no longer drop an interrupt that races the turn-start handshake.
        let Some(turn) = self.await_turn_id(INTERRUPT_TURN_ID_WAIT).await else {
            return Ok(());
        };
        self.notify("turn/interrupt", interrupt_params(&self.thread_id, &turn))
            .await
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        // Best-effort graceful close: interrupt any in-flight turn, then let
        // `kill_on_drop` reap the child. We never block the host on shutdown.
        let _ = self.interrupt().await;
        Ok(())
    }
}

/// Build the `turn/start` params (one text input on the live thread).
fn turn_start_params(thread_id: &str, directive: &str) -> Value {
    let input = json!([{ "type": "text", "text": directive }]);
    json!({ "threadId": thread_id, "input": input })
}

/// Build the `turn/interrupt` params for the in-flight turn.
fn interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({ "threadId": thread_id, "turnId": turn_id })
}

/// Build the `{id, result:{approved[, reason]}}` reply to a `requestApproval`.
fn approval_reply(raw_id: &Value, approved: bool) -> Value {
    let result = if approved {
        json!({ "approved": true })
    } else {
        json!({ "approved": false, "reason": "declined by umadev governance" })
    };
    json!({ "id": raw_id, "result": result })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a JSON test fixture from a string literal. Building deeply nested
    /// fixtures this way (instead of the `json!` macro) keeps the test source's
    /// literal brace depth shallow.
    fn v(s: &str) -> Value {
        serde_json::from_str(s).expect("valid json fixture")
    }

    /// A throwaway event channel pair for the pure translators.
    fn chan() -> (EventTx, mpsc::UnboundedReceiver<SessionEvent>) {
        mpsc::unbounded_channel()
    }

    // ---------- pure-unit coverage (cross-platform, no subprocess) ----------

    #[test]
    fn codex_model_drops_non_native_and_keeps_native() {
        // The claude-centric pipeline default must NOT reach codex.
        assert_eq!(codex_model("claude-sonnet-4-6"), None);
        assert_eq!(codex_model(""), None);
        assert_eq!(codex_model("gemini-2.0-flash"), None);
        // codex-native ids are forwarded verbatim.
        assert_eq!(codex_model("gpt-5.5"), Some("gpt-5.5".to_string()));
        assert_eq!(codex_model("o3-mini"), Some("o3-mini".to_string()));
        assert_eq!(
            codex_model("codex-mini-latest"),
            Some("codex-mini-latest".to_string())
        );
    }

    #[test]
    fn thread_start_params_sets_policy_and_drops_non_native_model() {
        let autonomous = thread_start_params(Path::new("/tmp/p"), "gpt-5-codex", true);
        assert_eq!(autonomous["approvalPolicy"], "never");
        assert_eq!(autonomous["sandbox"], "workspace-write");
        assert_eq!(autonomous["model"], "gpt-5-codex");
        // Gate tier → on-request; claude model id dropped (absent key).
        let gated = thread_start_params(Path::new("/tmp/p"), "claude-sonnet-4-6", false);
        assert_eq!(gated["approvalPolicy"], "on-request");
        assert!(
            gated.get("model").is_none(),
            "non-codex model must be dropped"
        );
    }

    #[test]
    fn thread_fork_params_marks_ephemeral() {
        let p = thread_fork_params("thr_main");
        assert_eq!(p["threadId"], "thr_main");
        assert_eq!(p["ephemeral"], true);
    }

    #[test]
    fn thread_resume_params_is_read_only_sandbox() {
        // A critic fork resumes the thread read-only: never-approve + readOnly
        // sandbox so it can never write the workspace (single-writer invariant).
        let p = thread_resume_params("thr_main", Path::new("/tmp/p"), "gpt-5-codex");
        assert_eq!(p["threadId"], "thr_main");
        assert_eq!(p["approvalPolicy"], "never");
        assert_eq!(p["sandbox"], "read-only");
        assert_eq!(p["model"], "gpt-5-codex");
        // A non-codex model is dropped (account default), same as thread/start.
        let p2 = thread_resume_params("thr_main", Path::new("/tmp/p"), "claude-sonnet-4-6");
        assert!(p2.get("model").is_none());
    }

    #[test]
    fn extract_thread_id_ok_and_err() {
        let ok = extract_thread_id(&v(r#"{"thread":{"id":"thr_9"}}"#)).unwrap();
        assert_eq!(ok, "thr_9");
        assert!(extract_thread_id(&v(r#"{"thread":{}}"#)).is_err());
    }

    #[test]
    fn map_turn_status_covers_all_states() {
        assert_eq!(
            map_turn_status("completed", &Value::Null),
            TurnStatus::Completed
        );
        assert_eq!(
            map_turn_status("interrupted", &Value::Null),
            TurnStatus::Interrupted
        );
        // unknown → treated as a clean boundary (fail-open, phase still ends).
        assert_eq!(
            map_turn_status("weird", &Value::Null),
            TurnStatus::Completed
        );
        // failed carries the error message.
        let p = v(r#"{"turn":{"error":{"message":"overloaded"}}}"#);
        let TurnStatus::Failed(reason) = map_turn_status("failed", &p) else {
            panic!("expected Failed");
        };
        assert!(reason.contains("overloaded"));
    }

    #[test]
    fn json_id_key_handles_number_and_string() {
        assert_eq!(json_id_key(&json!(42)), "42");
        assert_eq!(json_id_key(&json!("abc")), "abc");
    }

    #[test]
    fn approval_action_target_maps_command_and_file() {
        let cmd = v(r#"{"command":"rm -rf x"}"#);
        let (action, target) =
            approval_action_target("item/commandExecution/requestApproval", &cmd);
        assert_eq!(action, "Bash");
        assert_eq!(target, "rm -rf x");

        let file = v(r#"{"filePath":"/etc/hosts"}"#);
        let (action, target) = approval_action_target("item/fileChange/requestApproval", &file);
        assert_eq!(action, "Write");
        assert_eq!(target, "/etc/hosts");

        // changes[].path fallback when no top-level filePath.
        let changes = v(r#"{"changes":[{"path":"src/a.ts"}]}"#);
        let (_, target) = approval_action_target("item/fileChange/requestApproval", &changes);
        assert_eq!(target, "src/a.ts");
    }

    #[test]
    fn approval_reply_shapes_accept_and_decline() {
        let accept = approval_reply(&json!(5), true);
        assert_eq!(accept["id"], 5);
        assert_eq!(accept["result"]["approved"], true);
        let decline = approval_reply(&json!("abc"), false);
        assert_eq!(decline["result"]["approved"], false);
        assert!(decline["result"]["reason"].is_string());
    }

    #[test]
    fn turn_start_params_wraps_directive_as_text_input() {
        let p = turn_start_params("thr_1", "do the thing");
        assert_eq!(p["threadId"], "thr_1");
        assert_eq!(p["input"][0]["type"], "text");
        assert_eq!(p["input"][0]["text"], "do the thing");
    }

    #[tokio::test]
    async fn emit_item_translates_command_execution() {
        let (tx, mut rx) = chan();
        emit_item(
            &v(
                r#"{"type":"commandExecution","command":"cargo build","status":"completed","exitCode":0}"#,
            ),
            &tx,
        );
        let SessionEvent::ToolCall { name, input } = rx.recv().await.unwrap() else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "Bash");
        assert_eq!(input["command"], "cargo build");
        let SessionEvent::ToolResult { ok, .. } = rx.recv().await.unwrap() else {
            panic!("expected ToolResult");
        };
        assert!(ok);
    }

    #[tokio::test]
    async fn emit_item_translates_file_change_add_and_update() {
        let (tx, mut rx) = chan();
        // add → Write.
        emit_item(
            &v(
                r#"{"type":"fileChange","changes":[{"path":"src/app.tsx","kind":"add"}],"status":"completed"}"#,
            ),
            &tx,
        );
        let SessionEvent::ToolCall { name, input } = rx.recv().await.unwrap() else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "Write", "kind=add → Write");
        assert_eq!(input["file_path"], "src/app.tsx");
        let _ = rx.recv().await; // its ToolResult

        // update → Edit.
        emit_item(
            &v(
                r#"{"type":"fileChange","changes":[{"path":"src/x.ts","kind":"update"}],"status":"completed"}"#,
            ),
            &tx,
        );
        let SessionEvent::ToolCall { name, .. } = rx.recv().await.unwrap() else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "Edit", "kind=update → Edit");
    }

    #[test]
    fn added_lines_of_diff_extracts_added_text_only() {
        // A real unified diff: only the `+` lines (minus the `+++` header) are the
        // new content; context + removed lines are dropped.
        assert_eq!(
            added_lines_of_diff("--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n keep\n-gone\n+next\n+more\n"),
            "next\nmore"
        );
        // A non-diff string (no markers) is returned verbatim so plain content is
        // still scanned.
        assert_eq!(
            added_lines_of_diff("plain new file body"),
            "plain new file body"
        );
    }

    #[tokio::test]
    async fn file_change_surfaces_added_content_for_governance() {
        // The P2-1 fix: a codex `fileChange` item carries a DIFF, not a `content`
        // field. The translator must reconstruct the added text into `input.content`
        // so the orchestrator's content scanner (emoji / color / secret / AI-slop)
        // actually sees what codex wrote — otherwise codex writes dodge governance
        // (codex has no PreToolUse hook to backstop it).
        let (tx, mut rx) = chan();
        emit_item(
            &json!({
                "type": "fileChange",
                "status": "completed",
                "changes": [{
                    "path": "src/x.tsx",
                    "kind": "add",
                    "diff": "+++ b/src/x.tsx\n@@ -0,0 +1,2 @@\n+const color = \"#ff0000\";\n+const ok = 1;\n",
                }],
            }),
            &tx,
        );
        let SessionEvent::ToolCall { name, input } = rx.recv().await.unwrap() else {
            panic!("expected ToolCall");
        };
        assert_eq!(name, "Write");
        assert_eq!(input["file_path"], "src/x.tsx");
        // The reconstructed content carries the ADDED lines, including the hardcoded
        // color the governance scanner must catch — the whole point of the fix.
        let content = input["content"].as_str().unwrap_or_default();
        assert!(
            content.contains("#ff0000"),
            "added content must reach the scanner: {content}"
        );
        assert!(!content.contains("+++"), "the +++ header is not content");
    }

    #[tokio::test]
    async fn file_change_with_explicit_content_is_surfaced_directly() {
        // Some shapes DO carry a `content` per change — prefer it verbatim, so an
        // emoji in the written text still reaches the scanner. The emoji is built
        // via a `\u{...}` escape so this source file stays emoji-free (UD-CODE-001).
        let (tx, mut rx) = chan();
        let rocket = '\u{1F680}'.to_string();
        emit_item(
            &json!({
                "type": "fileChange",
                "status": "completed",
                "changes": [{ "path": "a.md", "kind": "add", "content": format!("# Title {rocket} launch") }],
            }),
            &tx,
        );
        let SessionEvent::ToolCall { input, .. } = rx.recv().await.unwrap() else {
            panic!("expected ToolCall");
        };
        let content = input["content"].as_str().unwrap_or_default();
        assert!(
            content.contains('\u{1F680}'),
            "explicit content (with its emoji) reaches the scanner: {content}"
        );
    }

    #[tokio::test]
    async fn file_change_without_content_degrades_to_path_only() {
        // No diff / content at all → no `content` key (the scanner degrades to
        // path-only, exactly as before — fail-open, never a spurious empty scan
        // that masquerades as "clean").
        let (tx, mut rx) = chan();
        emit_item(
            &v(
                r#"{"type":"fileChange","status":"completed","changes":[{"path":"b.rs","kind":"update"}]}"#,
            ),
            &tx,
        );
        let SessionEvent::ToolCall { input, .. } = rx.recv().await.unwrap() else {
            panic!("expected ToolCall");
        };
        assert!(
            input.get("content").is_none(),
            "no recoverable content → no content key: {input}"
        );
    }

    #[tokio::test]
    async fn dispatch_line_routes_text_and_turn_done() {
        let pending = empty_pending();
        let approvals = empty_approvals();
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage = empty_usage();
        let (tx, mut rx) = chan();

        let delta = r#"{"method":"item/agentMessage/delta","params":{"delta":"hello"}}"#;
        dispatch_line(delta, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::TextDelta(t) = rx.recv().await.unwrap() else {
            panic!("expected TextDelta");
        };
        assert_eq!(t, "hello");

        let done =
            r#"{"method":"turn/completed","params":{"turn":{"id":"turn_1","status":"completed"}}}"#;
        dispatch_line(done, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::TurnDone { status, usage } = rx.recv().await.unwrap() else {
            panic!("expected TurnDone");
        };
        assert_eq!(status, TurnStatus::Completed);
        // No usage notification arrived → None (the consumer estimates). F3.
        assert!(usage.is_none());
    }

    #[tokio::test]
    async fn dispatch_line_attaches_streamed_usage_to_turn_done() {
        // F3: codex streams real usage in a SEPARATE `thread/tokenUsage/updated`
        // notification; the next `turn/completed` must carry it onto TurnDone so
        // `/usage` is truthful on the DEFAULT loop.
        let pending = empty_pending();
        let approvals = empty_approvals();
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage = empty_usage();
        let (tx, mut rx) = chan();

        // Real codex wire shape (verified against a ~/.codex rollout): the counts
        // are nested under `info.{last,total}_token_usage`, NOT flat on `usage`.
        let usage_note = r#"{"method":"thread/tokenUsage/updated","params":{"info":{"last_token_usage":{"input_tokens":17162,"cached_input_tokens":5504,"output_tokens":6,"reasoning_output_tokens":4,"total_tokens":22678},"total_token_usage":{"input_tokens":17162,"cached_input_tokens":5504,"output_tokens":6,"reasoning_output_tokens":4,"total_tokens":22678}}}}"#;
        dispatch_line(
            usage_note,
            &pending,
            &approvals,
            &turn_id,
            &latest_usage,
            &tx,
        )
        .await;
        // The usage notification surfaces no SessionEvent on its own.

        let done =
            r#"{"method":"turn/completed","params":{"turn":{"id":"t","status":"completed"}}}"#;
        dispatch_line(done, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::TurnDone { status, usage } = rx.recv().await.unwrap() else {
            panic!("expected TurnDone");
        };
        assert_eq!(status, TurnStatus::Completed);
        let u = usage.expect("real usage attached to TurnDone");
        // cached input folds into input; reasoning output folds into output.
        assert_eq!(u.input_tokens, 17162 + 5504);
        assert_eq!(u.output_tokens, 6 + 4);

        // The accumulator was drained → a NEXT turn with no usage carries None.
        dispatch_line(done, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::TurnDone { usage: next, .. } = rx.recv().await.unwrap() else {
            panic!("expected TurnDone");
        };
        assert!(
            next.is_none(),
            "stale usage must not leak into the next turn"
        );
    }

    #[tokio::test]
    async fn dispatch_line_records_per_turn_last_usage_not_cumulative_total() {
        // The consumer APPENDS each turn's usage to usage.jsonl and `/usage` SUMS
        // the rows, so each TurnDone must carry the PER-TURN delta (`last_*`), never
        // the running cumulative (`total_*`) — else N turns overcount ~O(N²). Drive
        // two turns where `total` accumulates but `last` differs, and assert each
        // TurnDone reports its own turn's `last`, summing to the final `total`.
        let pending = empty_pending();
        let approvals = empty_approvals();
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage = empty_usage();
        let (tx, mut rx) = chan();
        let done =
            r#"{"method":"turn/completed","params":{"turn":{"id":"t","status":"completed"}}}"#;

        // Turn 1: last == total (first turn).
        let u1 = r#"{"method":"thread/tokenUsage/updated","params":{"info":{"last_token_usage":{"input_tokens":100,"output_tokens":10},"total_token_usage":{"input_tokens":100,"output_tokens":10}}}}"#;
        dispatch_line(u1, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        dispatch_line(done, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::TurnDone { usage: Some(a), .. } = rx.recv().await.unwrap() else {
            panic!("turn 1 usage");
        };

        // Turn 2: total accumulated to 150/15, but THIS turn's delta is 50/5.
        let u2 = r#"{"method":"thread/tokenUsage/updated","params":{"info":{"last_token_usage":{"input_tokens":50,"output_tokens":5},"total_token_usage":{"input_tokens":150,"output_tokens":15}}}}"#;
        dispatch_line(u2, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        dispatch_line(done, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::TurnDone { usage: Some(b), .. } = rx.recv().await.unwrap() else {
            panic!("turn 2 usage");
        };

        assert_eq!(
            (a.input_tokens, a.output_tokens),
            (100, 10),
            "turn 1 = its own last"
        );
        assert_eq!(
            (b.input_tokens, b.output_tokens),
            (50, 5),
            "turn 2 must be the PER-TURN delta (50/5), not the cumulative total (150/15)"
        );
        // The per-turn rows sum to the final cumulative total — no overcount.
        assert_eq!(a.input_tokens + b.input_tokens, 150);
        assert_eq!(a.output_tokens + b.output_tokens, 15);
    }

    #[tokio::test]
    async fn dispatch_line_prefers_inline_usage_on_turn_completed() {
        // Some codex versions inline usage on `turn/completed` itself — that wins.
        let pending = empty_pending();
        let approvals = empty_approvals();
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage = empty_usage();
        let (tx, mut rx) = chan();

        let done = r#"{"method":"turn/completed","params":{"turn":{"id":"t","status":"completed"},"usage":{"inputTokens":100,"outputTokens":20}}}"#;
        dispatch_line(done, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::TurnDone { usage, .. } = rx.recv().await.unwrap() else {
            panic!("expected TurnDone");
        };
        let u = usage.expect("inline usage attached");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 20);
    }

    #[tokio::test]
    async fn dispatch_line_routes_response_and_jsonrpc_error() {
        let pending = empty_pending();
        let approvals = empty_approvals();
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage = empty_usage();
        let (tx, _rx) = chan();

        // A result response completes the oneshot with Ok.
        let (otx, orx) = oneshot::channel();
        pending.lock().await.insert(7, otx);
        dispatch_line(
            r#"{"id":7,"result":{"thread":{"id":"thr_9"}}}"#,
            &pending,
            &approvals,
            &turn_id,
            &latest_usage,
            &tx,
        )
        .await;
        let got = orx.await.unwrap().unwrap();
        assert_eq!(got["thread"]["id"], "thr_9");

        // A -32001 "overloaded" error response maps to Err, not a panic.
        let (etx, erx) = oneshot::channel();
        pending.lock().await.insert(3, etx);
        dispatch_line(
            r#"{"id":3,"error":{"code":-32001,"message":"overloaded"}}"#,
            &pending,
            &approvals,
            &turn_id,
            &latest_usage,
            &tx,
        )
        .await;
        assert!(
            erx.await.unwrap().is_err(),
            "jsonrpc error must surface as Err"
        );
    }

    #[tokio::test]
    async fn dispatch_line_routes_server_request_to_need_approval() {
        let pending = empty_pending();
        let approvals = empty_approvals();
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage = empty_usage();
        let (tx, mut rx) = chan();

        let req = r#"{"id":100,"method":"item/commandExecution/requestApproval","params":{"command":"rm -rf x"}}"#;
        dispatch_line(req, &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        let SessionEvent::NeedApproval {
            req_id,
            action,
            target,
        } = rx.recv().await.unwrap()
        else {
            panic!("expected NeedApproval");
        };
        assert_eq!(req_id, "100");
        assert_eq!(action, "Bash");
        assert_eq!(target, "rm -rf x");
        // The raw id must be stashed for the reply.
        assert!(approvals.lock().await.contains_key("100"));
    }

    #[tokio::test]
    async fn dispatch_line_drops_non_json_failopen() {
        let pending = empty_pending();
        let approvals = empty_approvals();
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let latest_usage = empty_usage();
        let (tx, mut rx) = chan();
        // Garbage must not panic and must not produce an event.
        dispatch_line(
            "not json",
            &pending,
            &approvals,
            &turn_id,
            &latest_usage,
            &tx,
        )
        .await;
        dispatch_line(
            "{broken",
            &pending,
            &approvals,
            &turn_id,
            &latest_usage,
            &tx,
        )
        .await;
        dispatch_line("", &pending, &approvals, &turn_id, &latest_usage, &tx).await;
        assert!(rx.try_recv().is_err());
    }

    /// An empty `PendingMap` for dispatch tests.
    fn empty_pending() -> PendingMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// An empty `ApprovalMap` for dispatch tests.
    fn empty_approvals() -> ApprovalMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// An empty `LatestUsage` accumulator for dispatch tests (F3).
    fn empty_usage() -> LatestUsage {
        Arc::new(Mutex::new(None))
    }

    #[tokio::test]
    async fn await_turn_id_returns_immediately_when_already_set() {
        // The common case: the id is already known → no waiting at all.
        let turn_id: TurnId = Arc::new(Mutex::new(Some("turn_x".to_string())));
        let got = await_turn_id_in(&turn_id, Duration::from_secs(5)).await;
        assert_eq!(got.as_deref(), Some("turn_x"));
    }

    #[tokio::test]
    async fn await_turn_id_picks_up_a_late_turn_started() {
        // F5: an early interrupt that arrives BEFORE `turn/started` must not be
        // dropped — `await_turn_id_in` waits and picks up the id the moment the
        // `turn/started` notification (here simulated by `set_turn_id`) lands.
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let writer = Arc::clone(&turn_id);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            set_turn_id(&writer, Some("turn_late".to_string())).await;
        });
        let got = await_turn_id_in(&turn_id, Duration::from_secs(2)).await;
        assert_eq!(
            got.as_deref(),
            Some("turn_late"),
            "an early interrupt must adopt the turn id once turn/started arrives"
        );
    }

    #[tokio::test]
    async fn await_turn_id_times_out_failopen_when_no_turn_starts() {
        // Fail-open: if the id never lands within the budget, return None (the
        // caller then no-ops; kill_on_drop is the final cancellation) — bounded,
        // never a wedge.
        let turn_id: TurnId = Arc::new(Mutex::new(None));
        let start = tokio::time::Instant::now();
        let got = await_turn_id_in(&turn_id, Duration::from_millis(50)).await;
        assert!(got.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the wait must be bounded by the budget, not hang"
        );
    }

    #[tokio::test]
    async fn adopt_turn_id_into_is_a_noop_once_set() {
        // F4 background adopt: the `turn/start` RESULT must not overwrite an id the
        // `turn/started` notification already set (whichever lands first wins).
        let turn_id: TurnId = Arc::new(Mutex::new(Some("turn_from_notify".to_string())));
        adopt_turn_id_into(&turn_id, &v(r#"{"turn":{"id":"turn_from_result"}}"#)).await;
        assert_eq!(
            turn_id.lock().await.clone().as_deref(),
            Some("turn_from_notify"),
            "the notification-set id must not be clobbered by the start result"
        );
        // But it DOES adopt when nothing is set yet.
        let empty: TurnId = Arc::new(Mutex::new(None));
        adopt_turn_id_into(&empty, &v(r#"{"turn":{"id":"turn_from_result"}}"#)).await;
        assert_eq!(
            empty.lock().await.clone().as_deref(),
            Some("turn_from_result")
        );
    }

    // ---------- end-to-end against a fake `codex app-server` (unix only) ----------
    //
    // The fake is a `#!/bin/sh` script Windows cannot exec; it models the
    // app-server JSON-RPC handshake + a turn so we assert the full
    // handshake → send_turn → event-translation → TurnDone round-trip. The
    // pure JSON translation paths above already give cross-platform coverage.

    /// One classified outcome from the e2e event stream (keeps the collector
    /// loop flat — no deep match-in-loop nesting).
    #[cfg(unix)]
    #[derive(Default)]
    struct Seen {
        text: bool,
        bash: bool,
        write: bool,
        done: Option<TurnStatus>,
    }

    #[cfg(unix)]
    fn classify(ev: SessionEvent, seen: &mut Seen) {
        match ev {
            SessionEvent::TextDelta(t) if t.contains("working") => seen.text = true,
            SessionEvent::ToolCall { name, input } if name == "Bash" => {
                seen.bash = true;
                assert_eq!(input["command"], "cargo build");
            }
            SessionEvent::ToolCall { name, input } if name == "Write" => {
                seen.write = true;
                assert_eq!(input["file_path"], "src/main.rs");
            }
            SessionEvent::TurnDone { status, .. } => seen.done = Some(status),
            _ => {}
        }
    }

    /// Write an executable fake `codex` shell shim modelling `app-server`.
    #[cfg(unix)]
    fn write_fake_codex(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The fake app-server script: replies to `initialize` + `thread/start`
    /// (echoing the request id), ignores `initialized`, and on `turn/start`
    /// echoes a turn result then drives turn/started → agentMessage delta →
    /// commandExecution → fileChange → turn/completed. Exercises the real
    /// id-correlation and notification-translation paths.
    #[cfg(unix)]
    const FAKE_APP_SERVER: &str = r#"#!/bin/sh
extract_id() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"id":%s,"result":{"userAgent":"fake"}}\n' "$(extract_id "$line")" ;;
    *'"method":"initialized"'*) : ;;
    *'"method":"thread/start"'*)
      printf '{"id":%s,"result":{"thread":{"id":"thr_test","sessionId":"thr_test"}}}\n' "$(extract_id "$line")" ;;
    *'"method":"turn/start"'*)
      printf '{"id":%s,"result":{"turn":{"id":"turn_test","status":"queued"}}}\n' "$(extract_id "$line")"
      printf '{"method":"turn/started","params":{"turn":{"id":"turn_test","status":"running"}}}\n'
      printf '{"method":"item/agentMessage/delta","params":{"delta":"working"}}\n'
      printf '{"method":"item/completed","params":{"item":{"type":"commandExecution","command":"cargo build","status":"completed","exitCode":0}}}\n'
      printf '{"method":"item/completed","params":{"item":{"type":"fileChange","changes":[{"path":"src/main.rs","kind":"add"}],"status":"completed"}}}\n'
      printf '{"method":"turn/completed","params":{"turn":{"id":"turn_test","status":"completed"}}}\n' ;;
  esac
done
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn start_handshake_send_turn_and_events_against_fake_app_server() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("codex");
        write_fake_codex(&script, FAKE_APP_SERVER);

        let mut session = CodexSession::start_with_program(
            script.to_str().unwrap(),
            dir.path(),
            "gpt-5-codex",
            true,
        )
        .await
        .expect("handshake should succeed against the fake app-server");
        assert_eq!(session.thread_id, "thr_test", "thread.id captured");

        session
            .send_turn("Produce the three core documents now.".to_string())
            .await
            .expect("send_turn should write turn/start");

        // Collect events until TurnDone (flat loop, classification extracted).
        let mut seen = Seen::default();
        while let Some(ev) = session.next_event().await {
            let is_done = matches!(ev, SessionEvent::TurnDone { .. });
            classify(ev, &mut seen);
            if is_done {
                break;
            }
        }

        assert!(seen.text, "should translate the agentMessage delta");
        assert!(
            seen.bash,
            "should translate commandExecution → Bash ToolCall"
        );
        assert!(
            seen.write,
            "should translate fileChange add → Write ToolCall"
        );
        assert_eq!(
            seen.done,
            Some(TurnStatus::Completed),
            "turn/completed → TurnDone"
        );
        let _ = session.end().await;
    }

    /// A fake whose `turn/start` NEVER echoes a response (no `{"id":..,"result"}`
    /// for the turn) — it only emits the `turn/started` notification then the
    /// stream. Models a server that's slow / never replies to the start RPC.
    /// The OLD `send_turn` inline-awaited that response and would BLOCK here; the
    /// F4 fix must return promptly (the turn id comes from the notification).
    #[cfg(unix)]
    const FAKE_APP_SERVER_NO_TURN_RESPONSE: &str = r#"#!/bin/sh
extract_id() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"id":%s,"result":{"userAgent":"fake"}}\n' "$(extract_id "$line")" ;;
    *'"method":"initialized"'*) : ;;
    *'"method":"thread/start"'*)
      printf '{"id":%s,"result":{"thread":{"id":"thr_test"}}}\n' "$(extract_id "$line")" ;;
    *'"method":"turn/start"'*)
      printf '{"method":"turn/started","params":{"turn":{"id":"turn_test","status":"running"}}}\n'
      printf '{"method":"item/agentMessage/delta","params":{"delta":"working"}}\n'
      printf '{"method":"turn/completed","params":{"turn":{"id":"turn_test","status":"completed"}}}\n' ;;
  esac
done
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn send_turn_returns_promptly_even_if_turn_start_response_never_comes() {
        // F4: send_turn must not couple to the server's `turn/start` RESPONSE timing.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("codex");
        write_fake_codex(&script, FAKE_APP_SERVER_NO_TURN_RESPONSE);

        let mut session = CodexSession::start_with_program(
            script.to_str().unwrap(),
            dir.path(),
            "gpt-5-codex",
            true,
        )
        .await
        .expect("handshake should succeed");

        // The send must complete FAST — bound it so a regression (inline-await of a
        // response that never arrives) trips the timeout instead of hanging the run.
        let sent =
            tokio::time::timeout(Duration::from_secs(2), session.send_turn("go".to_string())).await;
        assert!(
            matches!(sent, Ok(Ok(()))),
            "send_turn must return promptly without awaiting the turn/start response: {sent:?}"
        );

        // The turn drives to completion via the notification stream. (The turn id
        // being adopted from `turn/started` mid-turn is verified deterministically
        // by `await_turn_id_*` — polling it during the drain here was a race: the
        // background notification task sets AND clears it between `next_event`s, so
        // on a fast/loaded runner the test missed the window.)
        let mut done = false;
        while let Some(ev) = session.next_event().await {
            if matches!(ev, SessionEvent::TurnDone { .. }) {
                done = true;
                break;
            }
        }
        assert!(done, "the turn completes from the notification stream");
        let _ = session.end().await;
    }

    // Fail-open: a base that exits immediately (no handshake) must surface a
    // Start error, never hang or panic.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_failopen_when_app_server_exits_immediately() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("codex");
        // Exits at once: no `initialize` reply ever comes → the reader hits EOF
        // and the pending oneshot is completed with an error.
        write_fake_codex(&script, "#!/bin/sh\nexit 0\n");

        let res = CodexSession::start_with_program(
            script.to_str().unwrap(),
            dir.path(),
            "gpt-5-codex",
            true,
        )
        .await;
        assert!(res.is_err(), "a base that never handshakes must fail-open");
    }

    #[tokio::test]
    async fn start_reports_not_installed() {
        // A bare program name that is definitely not on PATH → spawn NotFound →
        // a "not found on PATH" Start error, regardless of whether a real codex
        // is installed (we pass the program explicitly, no PATH fallthrough race).
        let dir = tempfile::TempDir::new().unwrap();
        let res = CodexSession::start_with_program(
            "umadev-fake-codex-missing-xyz",
            dir.path(),
            "gpt-5-codex",
            true,
        )
        .await;
        let Err(SessionError::Start(msg)) = res else {
            panic!("expected Start(not found)");
        };
        assert!(msg.contains("not found on PATH"));
    }
}
