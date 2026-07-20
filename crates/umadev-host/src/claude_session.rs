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
//! - exposes the [`umadev_runtime::BaseSession`] contract the 9-phase runner
//!   drives.
//!
//! Launch flags (from the headless stream-json contract):
//! `claude --print --input-format stream-json --output-format stream-json
//! --verbose --session-id <uuid> --permission-mode <plan|default|bypassPermissions>
//! --allowedTools <read-only + research + sub-agent set; auto adds the mutating
//! Edit/Write/Bash/NotebookEdit>` (+ optional `--append-system-prompt`). The base's
//! native read/research/delegate tools (incl. `Agent`/`Task` sub-agents) are
//! pre-approved so they run natively instead of eating a per-tool approval — see
//! the internal `PLAN_ALLOWED_TOOLS` / `GUARDED_ALLOWED_TOOLS` /
//! `AUTO_ALLOWED_TOOLS` allowlists.
//! We deliberately use `--append-system-prompt` (NOT `--system-prompt`, which
//! would replace the tool guidance and degrade the base into a chat box).
//!
//! The permission mode tracks the autonomy tier so claude is consistent with the
//! codex / opencode drivers: `autonomous` (auto tier) → `bypassPermissions` (the
//! base runs with FULL ACCESS and never interrupts — matching codex
//! `approvalPolicy: never` + full-access sandbox and opencode's wildcard-allow
//! ruleset; UmaDev's PreToolUse/PostToolUse governance hooks still see every
//! tool call, since claude runs hooks regardless of the permission mode),
//! Guarded → `default` (claude raises a
//! `can_use_tool` approval for each tool, which becomes a `NeedApproval` the
//! orchestrator answers — the human-in-the-loop floor, so the
//! irreversible-action gate is not bypassed), and Plan → `plan` with a strict
//! read-only allowlist. `UMADEV_CLAUDE_PERMISSION_MODE` can only tighten Auto;
//! it can never widen Plan or Guarded.
//!
//! Fail-open by contract: a garbled line is skipped, a dead session surfaces a
//! [`umadev_runtime::TurnStatus::Failed`], never a panic.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use umadev_runtime::{
    ApprovalDecision, AskQuestion, AskUserQuestion, BackgroundTaskSignal, BasePermissionProfile,
    BaseSession, DeliveryReceiptStage, DeliveryReport, ExitPlanMode, FileInputMode, HostAnswer,
    HostQuestion, HostQuestionKind, HostQuestionOption, HostRequest, HostResponse, InputDelivery,
    ResumeCapability, SessionCapabilities, SessionError, SessionEvent, SteerSemantics,
    SubagentVisibility, TurnInput, TurnInputBlockKind, TurnStatus, Usage,
};

use crate::spawn_parts;
use crate::stderr_tail::{StderrDrain, StderrTail};
use crate::{
    isolate_process_tree, kill_isolated_process_tree_blocking, reap_isolated_process_tree,
    END_REAP_BUDGET,
};

/// How many events the stdout-reader task may buffer ahead of the consumer.
const EVENT_CHANNEL_CAP: usize = 256;

/// Bound on unresolved `can_use_tool` requests retained for exact replies. A
/// normal Claude turn has only a handful; the cap is a defensive backstop for a
/// broken or hostile peer that streams requests without ever resolving them.
const PENDING_CONTROL_CAP: usize = 128;
/// Defensive ceiling for exact outbound-message acknowledgments. A normal
/// session has at most one report-bearing send in flight; this guards future
/// callers and a hostile/buggy peer without retaining user message bodies.
const PENDING_REPLAY_ACK_CAP: usize = 128;
/// Defensive ceiling for UUID-only command lifecycle tracking. The state stores
/// no prompt or attachment material; it exists solely to distinguish messages
/// UmaDev sent from internally queued Claude messages named in an interrupt
/// receipt.
const KNOWN_COMMAND_CAP: usize = 256;
/// Bound outstanding client control requests (currently interrupt receipts).
/// Dropping an evicted sender wakes its waiter, so a hostile peer cannot retain
/// futures indefinitely.
const PENDING_CLIENT_CONTROL_CAP: usize = 128;
/// A protocol ACK is useful but must never hold the interactive surface
/// hostage. Older Claude versions may accept the input without replaying a UUID;
/// after this deadline the honest receipt remains `transport_written`.
const REPLAY_ACK_BUDGET: std::time::Duration = std::time::Duration::from_millis(1_500);
/// Claude advertises `interrupt_receipt_v1` before returning a typed receipt.
/// Waiting is bounded so a broken/newer peer can never hold Esc hostage.
const INTERRUPT_RECEIPT_BUDGET: std::time::Duration = std::time::Duration::from_millis(1_500);
const MAX_INPUT_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// A single stdout NDJSON record may be large (for example a tool result), but
/// it must not be able to grow the reader buffer without bound.
const MAX_OUTPUT_FRAME_BYTES: usize = 32 * 1024 * 1024;

enum ClaudeFrameRead {
    Line(Vec<u8>),
    Oversized,
}

/// The exact request material Claude requires back in an allow response. In
/// particular, `AskUserQuestion` needs its original `questions` plus injected
/// `answers`; returning only `{behavior:"allow"}` silently loses the answer.
#[derive(Debug, Clone, PartialEq)]
struct PendingClaudeControl {
    tool_name: String,
    input: Value,
}

/// Small insertion-ordered store shared by the stdout pump and response path.
/// The pump records a request before publishing its event, so a consumer can
/// answer immediately without racing the original payload into this map.
#[derive(Debug, Default)]
struct PendingClaudeControls {
    order: VecDeque<String>,
    by_id: HashMap<String, PendingClaudeControl>,
}

impl PendingClaudeControls {
    fn insert(&mut self, req_id: String, request: PendingClaudeControl) {
        if self.by_id.contains_key(&req_id) {
            self.order.retain(|id| id != &req_id);
        }
        self.order.push_back(req_id.clone());
        self.by_id.insert(req_id, request);
        while self.by_id.len() > PENDING_CONTROL_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.by_id.remove(&oldest);
            }
        }
    }

    fn get(&self, req_id: &str) -> Option<PendingClaudeControl> {
        self.by_id.get(req_id).cloned()
    }

    fn remove(&mut self, req_id: &str) {
        self.by_id.remove(req_id);
        self.order.retain(|id| id != req_id);
    }
}

type SharedPendingClaudeControls = Arc<Mutex<PendingClaudeControls>>;

/// Insertion-ordered exact-UUID ACK waiters. The store retains neither prompt
/// text nor attachment metadata. Removing a sender wakes its waiter, so stream
/// EOF, eviction, timeout, and shutdown cannot leak a pending future.
#[derive(Default)]
struct PendingClaudeReplayAcks {
    order: VecDeque<String>,
    by_uuid: HashMap<String, oneshot::Sender<()>>,
}

impl PendingClaudeReplayAcks {
    fn register(&mut self, uuid: String) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        if self.by_uuid.contains_key(&uuid) {
            self.order.retain(|id| id != &uuid);
        }
        self.order.push_back(uuid.clone());
        self.by_uuid.insert(uuid, sender);
        while self.by_uuid.len() > PENDING_REPLAY_ACK_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.by_uuid.remove(&oldest);
            }
        }
        receiver
    }

    fn acknowledge(&mut self, uuid: &str) -> bool {
        let Some(sender) = self.by_uuid.remove(uuid) else {
            return false;
        };
        self.order.retain(|id| id != uuid);
        sender.send(()).is_ok()
    }

    fn remove(&mut self, uuid: &str) {
        self.by_uuid.remove(uuid);
        self.order.retain(|id| id != uuid);
    }

    fn clear(&mut self) {
        self.by_uuid.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_uuid.len()
    }
}

type SharedPendingClaudeReplayAcks = Arc<Mutex<PendingClaudeReplayAcks>>;

/// UUID-only state for Claude's message lifecycle and client-originated control
/// responses. This intentionally retains neither user text nor tool payloads.
#[derive(Default)]
struct ClaudeProtocolState {
    command_order: VecDeque<String>,
    known_commands: HashSet<String>,
    client_control_order: VecDeque<String>,
    client_controls: HashMap<String, oneshot::Sender<Value>>,
    interrupt_receipt_v1: bool,
}

impl ClaudeProtocolState {
    fn register_command(&mut self, uuid: String) {
        if self.known_commands.contains(&uuid) {
            self.command_order.retain(|id| id != &uuid);
        }
        self.command_order.push_back(uuid.clone());
        self.known_commands.insert(uuid);
        while self.known_commands.len() > KNOWN_COMMAND_CAP {
            if let Some(oldest) = self.command_order.pop_front() {
                self.known_commands.remove(&oldest);
            }
        }
    }

    fn forget_command(&mut self, uuid: &str) {
        self.known_commands.remove(uuid);
        self.command_order.retain(|id| id != uuid);
    }

    fn register_client_control(&mut self, request_id: String) -> oneshot::Receiver<Value> {
        let (sender, receiver) = oneshot::channel();
        if self.client_controls.contains_key(&request_id) {
            self.client_control_order.retain(|id| id != &request_id);
        }
        self.client_control_order.push_back(request_id.clone());
        self.client_controls.insert(request_id, sender);
        while self.client_controls.len() > PENDING_CLIENT_CONTROL_CAP {
            if let Some(oldest) = self.client_control_order.pop_front() {
                self.client_controls.remove(&oldest);
            }
        }
        receiver
    }

    fn forget_client_control(&mut self, request_id: &str) {
        self.client_controls.remove(request_id);
        self.client_control_order.retain(|id| id != request_id);
    }

    fn observe(&mut self, frame: &Value) {
        match frame.get("type").and_then(Value::as_str) {
            Some("system") if frame.get("subtype").and_then(Value::as_str) == Some("init") => {
                self.interrupt_receipt_v1 = frame
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .is_some_and(|capabilities| {
                        capabilities
                            .iter()
                            .any(|capability| capability.as_str() == Some("interrupt_receipt_v1"))
                    });
            }
            Some("command_lifecycle") => {
                let Some(uuid) = frame
                    .get("command_uuid")
                    .and_then(Value::as_str)
                    .filter(|uuid| !uuid.is_empty())
                else {
                    return;
                };
                if matches!(
                    frame.get("state").and_then(Value::as_str),
                    Some("completed" | "cancelled" | "discarded")
                ) {
                    self.forget_command(uuid);
                }
            }
            Some("control_response") => {
                let response = frame.get("response").and_then(Value::as_object);
                let request_id = response
                    .and_then(|response| response.get("request_id"))
                    .and_then(Value::as_str)
                    .or_else(|| frame.get("request_id").and_then(Value::as_str))
                    .filter(|request_id| !request_id.is_empty());
                let Some(request_id) = request_id else {
                    return;
                };
                let Some(sender) = self.client_controls.remove(request_id) else {
                    return;
                };
                self.client_control_order.retain(|id| id != request_id);
                let payload = response
                    .and_then(|response| response.get("response"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let _ = sender.send(payload);
            }
            _ => {}
        }
    }

    fn known_still_queued(&self, receipt: &Value) -> Vec<String> {
        let mut seen = HashSet::new();
        receipt
            .get("still_queued")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            // Claude documents that receipts may include internal UUIDs. Only
            // cancel exact messages UmaDev registered before writing.
            .filter(|uuid| self.known_commands.contains(*uuid))
            .filter(|uuid| seen.insert((*uuid).to_string()))
            .map(str::to_string)
            .collect()
    }

    fn clear(&mut self) {
        self.command_order.clear();
        self.known_commands.clear();
        self.client_control_order.clear();
        self.client_controls.clear();
        self.interrupt_receipt_v1 = false;
    }
}

type SharedClaudeProtocolState = Arc<Mutex<ClaudeProtocolState>>;

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
    /// Windows kill-on-close Job Object holding the base's whole process tree so a
    /// native grandchild can't orphan once the Node trampoline exits (`taskkill`
    /// walks only the live parent chain). Terminated in `end`/`Drop`. `None` off
    /// Windows / when the OS refused nested job assignment (the process-group
    /// fallback in [`crate::kill_isolated_process_tree`] stands).
    #[cfg(windows)]
    process_job: Option<umadev_process::KillOnCloseJob>,
    stdin: ChildStdin,
    events: mpsc::Receiver<SessionEvent>,
    /// Exact unresolved control payloads, keyed by Claude request id. Needed to
    /// preserve normal tool input and to merge structured question answers.
    pending_controls: SharedPendingClaudeControls,
    /// Exact client UUID → replay ACK waiters. Prompt content is never stored.
    pending_replay_acks: SharedPendingClaudeReplayAcks,
    /// UUID-only lifecycle/capability state for typed interrupt receipts. This
    /// lets Esc cancel only UmaDev-originated queued commands and ignore Claude's
    /// internally queued UUIDs.
    protocol: SharedClaudeProtocolState,
    /// Bounded tail of the base's STDERR, captured by the drain task, surfaced
    /// via [`BaseSession::stderr_tail`] to explain *why* a base went idle.
    stderr: StderrTail,
    stderr_drain: StderrDrain,
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
    /// The permission profile maps Plan/Guarded/Auto to Claude's native
    /// `plan`/`default`/`bypassPermissions` modes.
    ///
    /// `max_turns` is an OPTIONAL per-run turn ceiling (a runaway backstop): `Some(n)`
    /// spawns claude with `--max-turns <n>`, `None` leaves it unbounded (today's
    /// behavior). The cap is derived by the caller from the route depth
    /// (`umadev_agent::router::Depth::max_turns`); see [`session_args`].
    pub async fn start(
        workspace: &Path,
        append_system: Option<&str>,
        permissions: BasePermissionProfile,
        max_turns: Option<u32>,
        // The user's selected model id/alias (empty = run claude's own default) —
        // emitted as `--model` so the continuous (default) path honors `/model`.
        model: &str,
    ) -> Result<Self, SessionError> {
        // Resolve the SAME way the single-shot driver does: honor UMADEV_CLAUDE_BIN, else on
        // Windows prefer the REAL `@anthropic-ai/claude-code/bin/claude.exe` over the bare
        // `claude` PATH entry (a `.cmd`/`.ps1` shim). Spawning the shim wraps it as
        // `cmd /c claude.cmd`, which (a) surfaces as os error 193/232 (broken pipe) and (b)
        // makes kill/exit-status target cmd.exe while the real node `claude` orphans. Using
        // the real binary directly fixes both on the continuous (default) path.
        let program = crate::claude::resolve_claude_program();
        let session_id = new_session_id();
        Self::spawn_with_args(
            &program,
            workspace,
            &session_args_for_profile(&session_id, append_system, permissions, max_turns, model),
            &session_id,
        )
        .await
        .map_err(crate::redaction::sanitize_session_error)
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
        .map_err(crate::redaction::sanitize_session_error)
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
    /// `UMADEV_CLAUDE_BIN` override honored. A spawn or resume failure surfaces as
    /// [`SessionError::Start`]; the caller must decide explicitly whether this task
    /// may start fresh or must preserve its existing conversation identity.
    pub async fn resume(
        workspace: &Path,
        append_system: Option<&str>,
        session_id: &str,
        permissions: BasePermissionProfile,
        max_turns: Option<u32>,
        // The user's selected model — emitted as `--model` on resume too, so a
        // `/continue` re-opens on the same model the user chose.
        model: &str,
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
            &resume_session_args_for_profile(
                session_id,
                append_system,
                permissions,
                max_turns,
                model,
            ),
            session_id,
        )
        .await
        .map_err(crate::redaction::sanitize_session_error)
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
        // Own process group so teardown signals the WHOLE tree at once. A
        // direct-child kill would orphan any tool/subagent subprocess the base
        // spawned (and, on an npm trampoline install, the native base grandchild)
        // by reparenting it to init. See [`crate::kill_isolated_process_tree`].
        isolate_process_tree(&mut cmd);

        let mut child = crate::spawn_retrying_etxtbsy(&mut cmd)
            .map_err(|e| SessionError::Start(spawn_err(program, &e)))?;
        // Windows: attach the base's whole tree to a kill-on-close Job Object BEFORE
        // any protocol traffic, so the native `claude` grandchild an npm trampoline
        // execs is torn down on teardown instead of orphaning once the trampoline
        // exits (taskkill walks only the LIVE parent chain). `None` off Windows / if
        // the OS refuses nested job assignment — the process group is the fallback.
        #[cfg(windows)]
        let process_job = umadev_process::KillOnCloseJob::attach(&child);

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
        let stderr_drain = child
            .stderr
            .take()
            .map_or_else(StderrDrain::empty, |stderr| {
                StderrDrain::spawn(stderr, stderr_tail.clone())
            });

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let pending_controls = Arc::new(Mutex::new(PendingClaudeControls::default()));
        let pending_replay_acks = Arc::new(Mutex::new(PendingClaudeReplayAcks::default()));
        let protocol = Arc::new(Mutex::new(ClaudeProtocolState::default()));
        tokio::spawn(pump_stdout(
            stdout,
            tx,
            Arc::clone(&pending_controls),
            Arc::clone(&pending_replay_acks),
            Arc::clone(&protocol),
        ));

        Ok(Self {
            child: std::sync::Mutex::new(child),
            #[cfg(windows)]
            process_job,
            stdin,
            events: rx,
            pending_controls,
            pending_replay_acks,
            protocol,
            stderr: stderr_tail,
            stderr_drain,
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

    /// Clone one pending request without holding the small sync mutex across an
    /// async stdin write.
    fn pending_control(&self, req_id: &str) -> Option<PendingClaudeControl> {
        self.pending_controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(req_id)
    }

    /// Forget a request only after its control response was flushed. On a pipe
    /// error the payload remains available for diagnostics/recovery.
    fn forget_control(&self, req_id: &str) {
        self.pending_controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(req_id);
    }

    /// Register before the stdin write so a fast replay cannot race ahead of
    /// the waiter. Only the opaque client UUID is retained.
    fn register_replay_ack(&self, uuid: &str) -> oneshot::Receiver<()> {
        self.pending_replay_acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register(uuid.to_string())
    }

    fn forget_replay_ack(&self, uuid: &str) {
        self.pending_replay_acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(uuid);
    }

    fn register_command(&self, uuid: &str) {
        self.protocol
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_command(uuid.to_string());
    }

    fn forget_command(&self, uuid: &str) {
        self.protocol
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forget_command(uuid);
    }

    /// Write one user frame and wait only for Claude's documented replay ACK.
    /// Timeout/old-version shapes retain the truthful transport receipt; they
    /// never become a send error and never claim that the model processed input.
    async fn write_user_line_with_receipt(
        &mut self,
        line: &str,
        uuid: &str,
    ) -> Result<DeliveryReceiptStage, SessionError> {
        self.register_command(uuid);
        let receiver = self.register_replay_ack(uuid);
        if let Err(error) = self.write_line(line).await {
            self.forget_replay_ack(uuid);
            self.forget_command(uuid);
            return Err(error);
        }
        let acknowledged = matches!(
            tokio::time::timeout(REPLAY_ACK_BUDGET, receiver).await,
            Ok(Ok(()))
        );
        self.forget_replay_ack(uuid);
        Ok(if acknowledged {
            DeliveryReceiptStage::ProtocolAcknowledged
        } else {
            DeliveryReceiptStage::TransportWritten
        })
    }

    /// Legacy phase sends have no report return value. They still carry a UUID
    /// and correlate replay ACKs, but cleanup happens asynchronously so adding
    /// acknowledgment support cannot insert a 1.5-second delay into older agent
    /// loops when an old Claude build emits no replay UUID.
    async fn write_user_line_detached_ack(
        &mut self,
        line: &str,
        uuid: &str,
    ) -> Result<(), SessionError> {
        self.register_command(uuid);
        let receiver = self.register_replay_ack(uuid);
        if let Err(error) = self.write_line(line).await {
            self.forget_replay_ack(uuid);
            self.forget_command(uuid);
            return Err(error);
        }
        let pending = Arc::clone(&self.pending_replay_acks);
        let uuid = uuid.to_string();
        tokio::spawn(async move {
            let acknowledged = matches!(
                tokio::time::timeout(REPLAY_ACK_BUDGET, receiver).await,
                Ok(Ok(()))
            );
            pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&uuid);
            tracing::debug!(
                acknowledged,
                "Claude outbound user frame receipt settled (not model progress)"
            );
        });
        Ok(())
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        // A dropped session (timed-out / cancelled turn, or teardown that skipped
        // `end()`) must kill the whole process GROUP so no tool/subagent (or, on
        // an npm trampoline install, native base) grandchild is orphaned to init.
        // Windows: terminate the Job Object first (reaches a grandchild taskkill
        // can't). The blocking group-kill recovers a poisoned lock and briefly
        // spins on contention so the group kill still fires (a bare `try_lock`
        // skipped it on poison/contention); fail-open to `kill_on_drop` on overrun.
        #[cfg(windows)]
        if let Some(job) = self.process_job.take() {
            job.terminate();
            drop(job);
        }
        kill_isolated_process_tree_blocking(&self.child);
    }
}

#[async_trait]
impl BaseSession for ClaudeSession {
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            mid_turn_steer: false,
            set_model: false,
            set_mode: false,
            set_thinking: false,
            text_input: InputDelivery::Native,
            image_input: InputDelivery::Native,
            file_input: InputDelivery::MaterializedText,
            steer: SteerSemantics::Unsupported,
            resume: ResumeCapability::Native,
            subagents: SubagentVisibility::AuthoritativeLiveSet,
            prompt_queue: umadev_runtime::PromptQueueCapability::Unsupported,
            background_process_control:
                umadev_runtime::BackgroundProcessControlCapability::Unsupported,
        }
    }

    async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
        // A fork is a FRESH read-only consult, never a branch of the writer's
        // transcript. This matters for both callers of the unified API:
        //
        // - intent routing runs before the writer's first turn, when Claude may not
        //   have persisted the parent's session yet; `--resume <main> --fork-session`
        //   can therefore spawn successfully and only fail later inside the child;
        // - independent review must not inherit the maker's prior reasoning or a
        //   stale conversation that can bias the verdict.
        //
        // A new `--session-id` provides clean model context. `--permission-mode plan`
        // is the actual read-only boundary. The `Read,Grep,Glob` allowlist only makes
        // those reads prompt-free; Claude documents `--allowedTools` as pre-approval,
        // not as a tool-denial sandbox. Intent JSON triage normally uses no tool at all.
        // The fork runs in the same workspace so reviews can use the on-disk
        // blackboard instead of inherited chat history. It takes no writer run-lock.
        let fork_id = new_session_id();
        let args = fork_session_args(&fork_id);
        let session = Self::spawn_with_args(&self.program, &self.workspace, &args, &fork_id)
            .await
            .map_err(crate::redaction::sanitize_session_error)?;
        Ok(Box::new(session))
    }

    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        let uuid = new_session_id();
        let line = user_message_line_with_uuid(&directive, &uuid);
        self.write_user_line_detached_ack(&line, &uuid)
            .await
            .map_err(crate::redaction::sanitize_session_error)
    }

    async fn send_input(&mut self, input: TurnInput) -> Result<DeliveryReport, SessionError> {
        crate::turn_input::ensure_supported(&input, self.capabilities())?;
        let prepared = crate::turn_input::prepare(input).await?;
        let (line, deliveries, uuid) = claude_user_message_line(&prepared)?;
        let encoded_bytes = line.len();
        if encoded_bytes > MAX_INPUT_FRAME_BYTES {
            return Err(SessionError::InputInvalid {
                index: 0,
                kind: TurnInputBlockKind::Text,
                reason: "encoded Claude input exceeds the 32 MiB frame limit".to_string(),
            });
        }
        let receipt = self
            .write_user_line_with_receipt(&line, &uuid)
            .await
            .map_err(crate::redaction::sanitize_session_error)?;
        let mut report = prepared.report(&deliveries, encoded_bytes);
        report.receipt = receipt;
        Ok(report)
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        // No internal timeout BY DESIGN — the runner owns phase/run budgets and
        // races this against them (then calls `interrupt`). Keeping the session
        // a pure relay avoids a synthetic TurnDone racing a real one.
        self.events
            .recv()
            .await
            .map(crate::redaction::sanitize_session_event)
    }

    async fn respond(
        &mut self,
        req_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError> {
        let pending = self.pending_control(req_id);
        let payload = legacy_approval_payload(decision, pending.as_ref());
        let line = control_response_line(req_id, &payload);
        self.write_line(&line)
            .await
            .map_err(crate::redaction::sanitize_session_error)?;
        self.forget_control(req_id);
        Ok(())
    }

    async fn respond_host(
        &mut self,
        req_id: &str,
        response: HostResponse,
    ) -> Result<(), SessionError> {
        let pending = self.pending_control(req_id);
        let payload = typed_host_response_payload(response, pending.as_ref());
        let line = control_response_line(req_id, &payload);
        self.write_line(&line)
            .await
            .map_err(crate::redaction::sanitize_session_error)?;
        self.forget_control(req_id);
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        let request_id = new_session_id();
        // Old Claude builds return an untyped empty ACK. Feature-detect the
        // receipt so Esc never acquires a new 1.5-second delay on those builds.
        let receipt = {
            let mut protocol = self
                .protocol
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            protocol
                .interrupt_receipt_v1
                .then(|| protocol.register_client_control(request_id.clone()))
        };
        let line = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        })
        .to_string();
        if let Err(error) = self.write_line(&line).await {
            self.protocol
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget_client_control(&request_id);
            return Err(crate::redaction::sanitize_session_error(error));
        }

        let Some(receipt) = receipt else {
            return Ok(());
        };
        let Ok(Ok(payload)) = tokio::time::timeout(INTERRUPT_RECEIPT_BUDGET, receipt).await else {
            self.protocol
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget_client_control(&request_id);
            tracing::warn!("Claude advertised interrupt_receipt_v1 but no typed receipt arrived");
            return Ok(());
        };
        let still_queued = self
            .protocol
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .known_still_queued(&payload);
        // The official receipt may include Claude-internal UUIDs. Never cancel
        // those; cancel every exact UmaDev UUID promptly, including all members
        // of a coalesced batch, before returning control to the interactive loop.
        for message_uuid in &still_queued {
            let cancel = serde_json::json!({
                "type": "control_request",
                "request_id": new_session_id(),
                "request": {
                    "subtype": "cancel_async_message",
                    "message_uuid": message_uuid
                }
            })
            .to_string();
            self.write_line(&cancel)
                .await
                .map_err(crate::redaction::sanitize_session_error)?;
        }
        tracing::debug!(
            cancelled_queued_commands = still_queued.len(),
            "Claude interrupt receipt settled"
        );
        Ok(())
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        // Best-effort: kill the whole process GROUP (drops stdin → EOF, tears
        // down the reader/stderr tasks) AND wait (bounded) for the direct child
        // to be reaped so shutdown is deterministic and leaves no orphan — a
        // direct-child kill would reparent tool/subagent grandchildren to init.
        // On overrun we fail open to kill_on_drop. Consistent with codex /
        // opencode `end()`.
        //
        // Windows: terminate the Job Object FIRST so the native `claude` grandchild
        // is killed even after the trampoline has exited (taskkill can no longer
        // reach it); the group reap below is the cross-platform backstop.
        #[cfg(windows)]
        if let Some(job) = self.process_job.take() {
            job.terminate();
            drop(job);
        }
        reap_isolated_process_tree(&self.child, END_REAP_BUDGET).await;
        self.stderr_drain.shutdown().await;
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

/// Exact stream-json envelope for resolving a `can_use_tool` request.
fn control_response_line(req_id: &str, payload: &Value) -> String {
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": req_id,
            "response": payload
        }
    })
    .to_string()
}

/// Legacy binary approval remains supported for ordinary tool prompts. Claude's
/// current callback contract expects the original input back on allow; denying
/// always carries a useful message. Interactive tools require a typed reply so
/// an accidental binary allow cannot submit an empty question/plan response.
fn legacy_approval_payload(
    decision: ApprovalDecision,
    pending: Option<&PendingClaudeControl>,
) -> Value {
    match decision {
        ApprovalDecision::Allow => match pending {
            Some(request)
                if AskUserQuestion::is_tool_name(&request.tool_name)
                    || ExitPlanMode::is_tool_name(&request.tool_name) =>
            {
                deny_payload("This Claude interaction requires a structured host response")
            }
            Some(request) => allow_payload(&request.input),
            None => deny_payload("The Claude permission request is no longer pending"),
        },
        ApprovalDecision::Deny => deny_payload("Denied by UmaDev"),
    }
}

/// Encode one typed UmaDev interaction response into Claude's `canUseTool`
/// callback shape. Variant mismatches fail closed to an explicit denial; they
/// are never coerced into authority.
fn typed_host_response_payload(
    response: HostResponse,
    pending: Option<&PendingClaudeControl>,
) -> Value {
    let Some(request) = pending else {
        return deny_payload("The Claude interaction is no longer pending");
    };

    match response {
        HostResponse::Approval {
            decision, message, ..
        } => {
            if AskUserQuestion::is_tool_name(&request.tool_name)
                || ExitPlanMode::is_tool_name(&request.tool_name)
            {
                return deny_payload("The Claude interaction requires its typed response");
            }
            approval_payload(decision, &request.input, message.as_deref())
        }
        HostResponse::UserInput { answers } => {
            if !AskUserQuestion::is_tool_name(&request.tool_name) {
                return deny_payload("User-input response does not match the pending Claude tool");
            }
            match merge_claude_answers(&request.input, &answers) {
                Ok(updated_input) => allow_payload(&updated_input),
                Err(reason) => deny_payload(&reason),
            }
        }
        HostResponse::PlanConfirmation { decision, feedback } => {
            if !ExitPlanMode::is_tool_name(&request.tool_name) {
                return deny_payload("Plan response does not match the pending Claude tool");
            }
            approval_payload(decision, &request.input, feedback.as_deref())
        }
        HostResponse::Cancelled { reason } => {
            deny_payload(reason.as_deref().unwrap_or("Cancelled by the user"))
        }
        HostResponse::Rejected { reason } => deny_payload(&reason),
        HostResponse::PermissionExpansion { message, .. } => deny_payload(
            message
                .as_deref()
                .unwrap_or("Claude did not request a permission-expansion response"),
        ),
        HostResponse::McpElicitation { .. } => {
            deny_payload("Claude did not request an MCP elicitation response")
        }
        HostResponse::UserInputOutcome { .. }
        | HostResponse::PlanOutcome { .. }
        | HostResponse::FolderTrust { .. } => {
            deny_payload("This response contract belongs to a different base interaction")
        }
    }
}

fn approval_payload(
    decision: ApprovalDecision,
    original_input: &Value,
    denial_message: Option<&str>,
) -> Value {
    match decision {
        ApprovalDecision::Allow => allow_payload(original_input),
        ApprovalDecision::Deny => deny_payload(denial_message.unwrap_or("Denied by the user")),
    }
}

fn allow_payload(updated_input: &Value) -> Value {
    serde_json::json!({
        "behavior": "allow",
        "updatedInput": updated_input
    })
}

fn deny_payload(message: &str) -> Value {
    let message = message.trim();
    serde_json::json!({
        "behavior": "deny",
        "message": if message.is_empty() { "Denied by UmaDev" } else { message }
    })
}

/// Claude's `AskUserQuestion` expects `answers` as
/// `{question text: answer string}` inside the ORIGINAL input. Multi-select
/// values are comma-separated by Claude's own schema; free text passes through
/// verbatim after trimming.
fn merge_claude_answers(input: &Value, answers: &[HostAnswer]) -> Result<Value, String> {
    let parsed = AskUserQuestion::parse_value(input)
        .ok_or_else(|| "Claude AskUserQuestion input is malformed".to_string())?;
    let mut expected: HashMap<String, (String, bool)> = HashMap::new();
    for (index, question) in parsed.questions.iter().enumerate() {
        let id = claude_question_id(question, index);
        let answer_key = if question.question.trim().is_empty() {
            id.clone()
        } else {
            question.question.clone()
        };
        if expected
            .insert(id, (answer_key, question.multi_select))
            .is_some()
        {
            return Err("Claude AskUserQuestion contains duplicate question ids".to_string());
        }
    }

    let mut answer_map = serde_json::Map::new();
    for answer in answers {
        let Some((answer_key, multi_select)) = expected.remove(&answer.question_id) else {
            return Err(format!(
                "Answer does not match a pending Claude question: {}",
                truncate(&answer.question_id, 80)
            ));
        };
        let values: Vec<&str> = answer
            .values
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect();
        if values.is_empty() {
            return Err(format!("Claude question was left unanswered: {answer_key}"));
        }
        if !multi_select && values.len() != 1 {
            return Err(format!(
                "Claude single-choice question received multiple answers: {answer_key}"
            ));
        }
        answer_map.insert(answer_key, Value::String(values.join(", ")));
    }
    if !expected.is_empty() {
        return Err("One or more required Claude questions were left unanswered".to_string());
    }

    let mut updated = input
        .as_object()
        .cloned()
        .ok_or_else(|| "Claude AskUserQuestion input is not an object".to_string())?;
    updated.insert("answers".to_string(), Value::Object(answer_map));
    Ok(Value::Object(updated))
}

fn claude_question_id(question: &AskQuestion, index: usize) -> String {
    if question.question.trim().is_empty() {
        format!("claude-question-{}", index + 1)
    } else {
        question.question.clone()
    }
}

/// Record or cancel pending controls before their public events are sent. This
/// preserves exact input without placing protocol-only mutable state in the
/// runtime event type.
fn observe_control_frame(line: &str, pending: &SharedPendingClaudeControls) {
    let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };
    match frame.get("type").and_then(Value::as_str) {
        Some("control_request") => {
            let Some((req_id, request)) = pending_control_from_frame(&frame) else {
                return;
            };
            pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(req_id, request);
        }
        Some("control_cancel_request") => {
            if let Some(req_id) = frame
                .get("request_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(req_id);
            }
        }
        _ => {}
    }
}

fn pending_control_from_frame(frame: &Value) -> Option<(String, PendingClaudeControl)> {
    let request = frame.get("request")?;
    if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    let req_id = frame
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    let tool_name = request
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    Some((req_id, PendingClaudeControl { tool_name, input }))
}

/// Extract an exact Claude replay acknowledgment UUID. The current official
/// `SDKUserMessageReplay` shape requires `type:"user"`, `isReplay:true`, and a
/// UUID. A normal user/tool-result frame is never accepted as an ACK, even when
/// it happens to carry a UUID.
fn replay_ack_uuid(line: &str) -> Option<String> {
    let frame = serde_json::from_str::<Value>(line.trim()).ok()?;
    if frame.get("type").and_then(Value::as_str) != Some("user")
        || frame.get("isReplay").and_then(Value::as_bool) != Some(true)
        || frame
            .get("session_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || frame
            .get("message")
            .and_then(Value::as_object)
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            != Some("user")
        || !matches!(
            frame.get("parent_tool_use_id"),
            Some(Value::Null | Value::String(_))
        )
    {
        return None;
    }
    frame
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
}

/// Reader task: parse stdout NDJSON → events forever. On EOF (the base process
/// died / the session ended) emit a terminal `Failed` so a crash mid-turn
/// surfaces as `TurnDone{Failed}` rather than a silent hang.
///
/// Lines flow through a per-session [`SubagentOutputGate`] (NOT the stateless
/// [`parse_stdout_line`] directly): a NESTED sub-agent's streamed frames are
/// buffered and flushed as ONE grouped block instead of interleaving
/// fragmentarily with the main agent's output. While a background sub-agent is
/// live, MAIN text/reasoning and a clean turn boundary are held until every
/// observed agent reaches a terminal state.
async fn pump_stdout(
    stdout: ChildStdout,
    tx: mpsc::Sender<SessionEvent>,
    pending_controls: SharedPendingClaudeControls,
    pending_replay_acks: SharedPendingClaudeReplayAcks,
    protocol: SharedClaudeProtocolState,
) {
    // Read raw bytes per line and decode LOSSY: `next_line` returns `Err` on a
    // single invalid UTF-8 byte, and the old `while let Ok(Some)` treated that as
    // end-of-stream — discarding the rest of the NDJSON turn AND emitting a
    // spurious "base session ended unexpectedly". The bounded byte reader +
    // `from_utf8_lossy` tolerates a bad byte (a non-JSON line is ignored by
    // `parse_stdout_line`, not the whole stream) without unbounded retention.
    let mut reader = BufReader::new(stdout);
    let mut gate = SubagentOutputGate::default();
    let terminal_reason = loop {
        match read_bounded_claude_frame(&mut reader, MAX_OUTPUT_FRAME_BYTES).await {
            Ok(Some(ClaudeFrameRead::Line(line_buf))) => {
                let line = String::from_utf8_lossy(&line_buf);
                let line = line.trim_end_matches(['\r', '\n']);
                if let Some(uuid) = replay_ack_uuid(line) {
                    let acknowledged = pending_replay_acks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .acknowledge(&uuid);
                    tracing::debug!(
                        acknowledged,
                        "inbound Claude replay user ACK (not model progress)"
                    );
                    // `--replay-user-messages` is an acknowledgment surface, not
                    // model output. Never render the user's own input a second time.
                    continue;
                }
                observe_control_frame(line, &pending_controls);
                if let Ok(frame) = serde_json::from_str::<Value>(line) {
                    protocol
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .observe(&frame);
                }
                for ev in gate.on_line(line) {
                    if tx
                        .send(crate::redaction::sanitize_session_event(ev))
                        .await
                        .is_err()
                    {
                        return; // consumer dropped → stop
                    }
                }
            }
            Ok(Some(ClaudeFrameRead::Oversized)) => {
                break "Claude stream-json frame exceeded the 32 MiB safety limit";
            }
            Ok(None) => break "base session ended unexpectedly",
            Err(_) => break "base session stdout could not be read",
        }
    };
    // Wake every receipt waiter before flushing buffered events. Stream EOF is
    // not an ACK; waiters degrade immediately to `transport_written` and no UUID
    // remains retained until its wall-clock timeout.
    pending_replay_acks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    protocol
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    // The base died / the stream ended: flush any still-held sub-agent buffers
    // FIRST so nothing a sub-agent produced is ever silently dropped, then the
    // synthetic terminal Failed.
    for ev in gate.finish_stream() {
        if tx
            .send(crate::redaction::sanitize_session_event(ev))
            .await
            .is_err()
        {
            return; // consumer dropped → stop
        }
    }
    let _ = tx
        .send(crate::redaction::sanitize_session_event(
            SessionEvent::TurnDone {
                status: TurnStatus::Failed(terminal_reason.to_string()),
                usage: None,
            },
        ))
        .await;
}

/// Read one LF-delimited stream-json record while bounding retained memory.
/// Pipe fragmentation is transparent: bytes are accumulated across reads until
/// LF or EOF. Once the limit is crossed, the rest of that record is discarded
/// before returning `Oversized`, so even a peer that never sends LF cannot grow
/// the process without bound.
async fn read_bounded_claude_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<ClaudeFrameRead>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() && !oversized {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if !oversized {
            let remaining = limit.saturating_sub(bytes.len());
            if take > remaining {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..take]);
            }
        }
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    if oversized {
        Ok(Some(ClaudeFrameRead::Oversized))
    } else {
        Ok(Some(ClaudeFrameRead::Line(bytes)))
    }
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
/// `WebSearch` / `WebFetch`, Claude's task-list tools, and every sub-agent spawn.
/// `TodoWrite` remains as a compatibility alias for older Claude builds;
/// `TaskCreate` / `TaskGet` / `TaskUpdate` / `TaskList` are the current official
/// task tools. `Agent` / `Task`
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
const PLAN_ALLOWED_TOOLS: &str = "Read,Grep,Glob,WebSearch,WebFetch";

const GUARDED_ALLOWED_TOOLS: &str = "Read,Grep,Glob,WebSearch,WebFetch,TodoWrite,TaskCreate,TaskGet,TaskUpdate,TaskList,Agent,Task,TaskOutput,BashOutput,AgentOutput";

/// AUTO additionally pre-approves the MUTATING working set (`Edit` / `Write` / `Bash`
/// / `NotebookEdit`) so an unattended autonomous run is never interrupted by a
/// per-tool prompt — the autonomy tier the user opted into.
const AUTO_ALLOWED_TOOLS: &str = "Read,Edit,Write,Bash,Grep,Glob,WebSearch,WebFetch,TodoWrite,\
     TaskCreate,TaskGet,TaskUpdate,TaskList,NotebookEdit,Agent,Task,TaskOutput,BashOutput,AgentOutput";

/// Resolve Claude's permission mode and allowlist as one policy pair. Keeping
/// them coupled matters: changing Auto's mode to `default` while retaining its
/// mutating `--allowedTools` list would still pre-authorize those mutations.
fn claude_permission_args_for_profile(
    permissions: BasePermissionProfile,
) -> (&'static str, &'static str) {
    let override_mode = std::env::var("UMADEV_CLAUDE_PERMISSION_MODE").ok();
    let no_skip = std::env::var("UMADEV_NO_SKIP_PERMS").as_deref() == Ok("1");
    resolve_claude_permission_args(permissions, override_mode.as_deref(), no_skip)
}

/// Pure permission-policy core. Plan and Guarded are fixed postures, so no
/// environment/config override can widen them. Auto accepts only a small
/// whitelist of known Claude modes, all at or below its native bypass posture.
/// Claude's classifier-backed `auto` is deliberately distinct from raw
/// `bypassPermissions`: it gets the non-mutating allowlist so Edit/Bash still
/// pass through Claude's classifier. Unknown future values fail safely to
/// Guarded. `UMADEV_NO_SKIP_PERMS=1` forbids bypass while still permitting the
/// official classifier-backed Auto and tighter Plan/dontAsk postures.
fn resolve_claude_permission_args(
    permissions: BasePermissionProfile,
    override_mode: Option<&str>,
    no_skip: bool,
) -> (&'static str, &'static str) {
    match permissions {
        BasePermissionProfile::Plan => ("plan", PLAN_ALLOWED_TOOLS),
        BasePermissionProfile::Guarded => ("default", GUARDED_ALLOWED_TOOLS),
        BasePermissionProfile::Auto => {
            let requested = override_mode
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_ascii_lowercase);
            if no_skip {
                return match requested.as_deref() {
                    Some("plan") => ("plan", PLAN_ALLOWED_TOOLS),
                    Some("dontask") => ("dontAsk", GUARDED_ALLOWED_TOOLS),
                    Some("auto") => ("auto", GUARDED_ALLOWED_TOOLS),
                    Some("manual") => ("manual", GUARDED_ALLOWED_TOOLS),
                    _ => ("default", GUARDED_ALLOWED_TOOLS),
                };
            }
            match requested.as_deref() {
                None | Some("bypasspermissions") => ("bypassPermissions", AUTO_ALLOWED_TOOLS),
                Some("auto") => ("auto", GUARDED_ALLOWED_TOOLS),
                Some("acceptedits") => ("acceptEdits", GUARDED_ALLOWED_TOOLS),
                Some("dontask") => ("dontAsk", GUARDED_ALLOWED_TOOLS),
                Some("plan") => ("plan", PLAN_ALLOWED_TOOLS),
                Some("manual") => ("manual", GUARDED_ALLOWED_TOOLS),
                // Never pass through an unknown mode: a future Claude release
                // could assign it broader semantics than UmaDev understands.
                // The known `default` mode lands on the same guarded pair.
                Some(_) => ("default", GUARDED_ALLOWED_TOOLS),
            }
        }
    }
}

/// The argument vector preceding any input — the stream-json continuous-session
/// flags. Exposed for tests. `--append-system-prompt` (NOT `--system-prompt`).
///
/// `autonomous` picks the permission mode so claude tracks the trust tier like
/// the codex / opencode drivers: `true` → `bypassPermissions` (full access,
/// never interrupts; governance hooks still audit every call), `false` →
/// `default` (claude raises a `can_use_tool` approval per tool, which
/// the orchestrator answers — keeping the human-in-the-loop / irreversible-action
/// floor live). Environment overrides are confined to Auto and may only select
/// a known equal-or-tighter posture; Plan/Guarded remain fixed.
///
/// `max_turns` is the OPTIONAL per-run turn ceiling (a runaway backstop): `Some(n)`
/// appends `--max-turns <n>`, `None` omits the flag entirely — leaving claude's
/// default unbounded agentic loop (today's behavior). The caller derives the cap from
/// the route depth (`umadev_agent::router::Depth::max_turns` — Fast 40 / Standard 150
/// / Deep 400); hitting it is reported as `error_max_turns` → [`TurnStatus::Truncated`]
/// (already handled by the internal `parse_result` parser), so no new parsing is needed. Fail-open: no
/// cap → no flag → unchanged behavior.
#[must_use]
pub fn session_args(
    session_id: &str,
    append_system: Option<&str>,
    autonomous: bool,
    max_turns: Option<u32>,
) -> Vec<String> {
    let permissions = if autonomous {
        BasePermissionProfile::Auto
    } else {
        BasePermissionProfile::Guarded
    };
    // The autonomous-bool wrapper (start_with_program + tests) pins no model — the
    // continuous model selection flows through ClaudeSession::start/resume instead.
    session_args_for_profile(session_id, append_system, permissions, max_turns, "")
}

fn session_args_for_profile(
    session_id: &str,
    append_system: Option<&str>,
    permissions: BasePermissionProfile,
    max_turns: Option<u32>,
    // The model id/alias the user selected. Emitted as `--model <id>` (skipping an
    // empty id and the offline/test placeholders) so the CONTINUOUS session honors
    // the user's `/model` choice exactly like the one-shot driver and like codex /
    // opencode — without this the default (persistent-goal) claude path silently ran
    // on claude's OWN config default, ignoring the selection.
    model: &str,
) -> Vec<String> {
    let (permission_mode, allowed_tools) = claude_permission_args_for_profile(permissions);
    let mut args = vec![
        "--print".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // Official stream-json acknowledgment surface. Every outbound user
        // frame carries a client UUID; Claude replays it with `isReplay:true`.
        // This proves protocol acceptance only, never model processing.
        "--replay-user-messages".to_string(),
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
        permission_mode.to_string(),
        "--allowedTools".to_string(),
        allowed_tools.to_string(),
    ];
    args.extend(crate::model_args(model));
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
    let permissions = if autonomous {
        BasePermissionProfile::Auto
    } else {
        BasePermissionProfile::Guarded
    };
    resume_session_args_for_profile(session_id, append_system, permissions, max_turns, "")
}

fn resume_session_args_for_profile(
    session_id: &str,
    append_system: Option<&str>,
    permissions: BasePermissionProfile,
    max_turns: Option<u32>,
    // The user's selected model — emitted as `--model <id>` on resume too, so a
    // `/continue` re-opens the conversation on the SAME model the user chose (see
    // [`session_args_for_profile`]). Empty / placeholder ids are skipped.
    model: &str,
) -> Vec<String> {
    let (permission_mode, allowed_tools) = claude_permission_args_for_profile(permissions);
    let mut args = vec![
        "--print".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--replay-user-messages".to_string(),
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
        // Re-open the existing conversation on its WRITABLE main line. No
        // `--fork-session` (that branches read-only), no new `--session-id` (that
        // mints a fresh one) — `--resume <id>` alone resumes + continues writing it.
        "--resume".to_string(),
        session_id.to_string(),
        "--permission-mode".to_string(),
        permission_mode.to_string(),
        "--allowedTools".to_string(),
        allowed_tools.to_string(),
    ];
    args.extend(crate::model_args(model));
    push_max_turns(&mut args, max_turns);
    if let Some(sys) = append_system.filter(|s| !s.is_empty()) {
        args.push("--append-system-prompt".to_string());
        args.push(sys.to_string());
    }
    args
}

/// The argument vector for every read-only consult fork: a FRESH, INDEPENDENT
/// session pinned to `fork_session_id` with **NO** `--resume <main>` and **NO**
/// `--fork-session`. It starts on clean model context and sees only its judge
/// directive plus any on-disk artifact it explicitly reads through
/// `Read,Grep,Glob`. It is spawned with `current_dir(workspace)` (see
/// the session's internal spawn path), so an artifact reviewer still sees the same
/// blackboard the main line wrote without inheriting the writer's transcript.
/// `--permission-mode plan` is the hard single-writer fence (read the workspace,
/// never write a file); `--allowedTools "Read,Grep,Glob"` merely pre-approves the
/// reads so they do not prompt. The unified API also serves pre-action intent JSON triage; that
/// prompt needs no tool, but retaining this minimal read-only set lets the same API
/// serve evidence-based reviewers. Mirrors opencode's fresh-independent-session
/// fork. Exposed for tests.
#[must_use]
pub fn fork_session_args(fork_session_id: &str) -> Vec<String> {
    let mut args = vec![
        "--print".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--replay-user-messages".to_string(),
        // Stream incremental text here too (a critic fork's verdict text must arrive
        // as deltas, not buffered — see `session_args`). Keeps `block_to_event`'s
        // text-suppression invariant: text always comes via `stream_event` deltas.
        "--include-partial-messages".to_string(),
        "--verbose".to_string(),
        // A FRESH pinned conversation — NO `--resume <main>` (that re-loads the
        // writer's transcript) and NO `--fork-session` (that branches the live main
        // line). The consult's model context is genuinely clean at the host level.
        "--session-id".to_string(),
        fork_session_id.to_string(),
        // Read-only: plan mode never applies an edit. The tool list makes only
        // Read/Grep/Glob prompt-free; it does not independently restrict tools.
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

/// Build the stream-json `user` message line for a phase directive. This is the
/// REAL wire shape (`{type:"user",message:{role,content},...}`) — the simplified
/// `{type:"user_message",message:"..."}` from some docs is wrong and claude
/// would reject it (and `exit(1)`). Exposed for tests.
#[must_use]
pub fn user_message_line(directive: &str) -> String {
    user_message_line_with_uuid(directive, &new_session_id())
}

fn user_message_line_with_uuid(directive: &str, uuid: &str) -> String {
    serde_json::json!({
        "type": "user",
        "uuid": uuid,
        "message": { "role": "user", "content": directive },
        "parent_tool_use_id": Value::Null,
        "session_id": ""
    })
    .to_string()
}

fn claude_user_message_line(
    input: &crate::turn_input::PreparedTurnInput,
) -> Result<(String, Vec<InputDelivery>, String), SessionError> {
    let mut content = Vec::with_capacity(input.blocks.len());
    let mut deliveries = Vec::with_capacity(input.blocks.len());
    for (index, block) in input.blocks.iter().enumerate() {
        match block {
            crate::turn_input::PreparedBlock::Text(text) => {
                content.push(serde_json::json!({"type":"text", "text":text}));
                deliveries.push(InputDelivery::Native);
            }
            crate::turn_input::PreparedBlock::Image(attachment) => {
                content.push(serde_json::json!({
                    "type":"image",
                    "source":{
                        "type":"base64",
                        "media_type":attachment.media_type,
                        "data":base64::engine::general_purpose::STANDARD.encode(&attachment.bytes)
                    }
                }));
                deliveries.push(InputDelivery::Native);
            }
            crate::turn_input::PreparedBlock::File { attachment, mode } => {
                if !matches!(mode, FileInputMode::MaterializeText) {
                    return Err(crate::turn_input::unsupported(
                        index,
                        TurnInputBlockKind::File,
                        "Claude stream-json has no generic file part; request explicit text materialization",
                    ));
                }
                let text = attachment.bounded_text(index)?;
                content.push(serde_json::json!({
                    "type":"text",
                    "text":format!(
                        "<umadev-attached-text index=\"{index}\" media-type=\"{}\">\n{text}\n</umadev-attached-text>",
                        attachment.media_type
                    )
                }));
                deliveries.push(InputDelivery::MaterializedText);
            }
        }
    }
    let uuid = new_session_id();
    Ok((
        serde_json::json!({
            "type":"user",
            "uuid":uuid,
            "message":{"role":"user", "content":content},
            "parent_tool_use_id":Value::Null,
            "session_id":""
        })
        .to_string(),
        deliveries,
        uuid,
    ))
}

/// Parse one stdout NDJSON line into zero or more [`SessionEvent`]s.
/// Fail-open: an unparseable / unknown line yields `vec![]` (skipped noise),
/// never an error or panic. Exposed for tests.
///
/// This is the STATELESS parse. The live pump wraps it in an internal
/// `SubagentGrouper`,
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
        .into_iter()
        .map(crate::redaction::sanitize_session_event)
        .collect()
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
        // control FLOW (`can_use_tool` → legacy approval or typed interaction →
        // response) is untouched; these still produce zero events. Fail-open: the
        // describers never panic on a malformed frame.
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
            // Claude 2.1.x reports retryable API failures as `system/api_retry`.
            // Surface a bounded progress warning so the TUI stays alive and the
            // user sees the attempt/backoff instead of an unexplained freeze.
            if let Some(ev) = api_retry_event(v) {
                return vec![ev];
            }
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
            SessionEvent::ToolCallCorrelated {
                call_id,
                name,
                input,
            } => SessionEvent::ToolCallCorrelated {
                call_id,
                name: format!("{SUBAGENT_MARKER}{name}"),
                input,
            },
            SessionEvent::ToolResult { ok, summary } => SessionEvent::ToolResult {
                ok,
                summary: format!("{SUBAGENT_MARKER}{summary}"),
            },
            SessionEvent::ToolResultCorrelated {
                call_id,
                ok,
                summary,
            } => SessionEvent::ToolResultCorrelated {
                call_id,
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
        /// Stable Claude `tool_use.id`, when present.
        call_id: Option<String>,
        /// Tool id (`Read`, `Grep`, …).
        name: String,
        /// Short human target ([`summarize_input`]: file path / command / …).
        target: String,
    },
    /// A nested tool result → completes the pending call row.
    Result {
        /// Matching Claude `tool_result.tool_use_id`, when present.
        call_id: Option<String>,
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
            Self::Call {
                call_id,
                name,
                target,
            } => call_id.as_ref().map_or(0, String::len) + name.len() + target.len(),
            Self::Result {
                call_id, summary, ..
            } => call_id.as_ref().map_or(0, String::len) + summary.len(),
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
    #[cfg(test)]
    fn on_line(&mut self, line: &str) -> Vec<SessionEvent> {
        self.on_line_with_deferred_boundary(line, false)
    }

    fn on_line_with_deferred_boundary(
        &mut self,
        line: &str,
        defer_completed_boundary: bool,
    ) -> Vec<SessionEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return vec![];
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            return vec![]; // not JSON (a stray log line) → skip, same as the pure parse
        };
        match parent_tool_use_id(&v).map(str::to_string) {
            Some(pid) => self.capture_frame(&pid, &v),
            None => self.main_line_frame(&v, defer_completed_boundary),
        }
    }

    /// A MAIN-line frame: yield exactly what [`parse_frame`] yields, PLUS any
    /// grouped-block flushes its content triggers (a sub-agent's terminating
    /// signal / the turn boundary), emitted BEFORE the main-line events so the
    /// transcript reads "grouped sub-agent block → its final report / turn end".
    fn main_line_frame(&mut self, v: &Value, defer_completed_boundary: bool) -> Vec<SessionEvent> {
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
        let boundary = events.iter().find_map(|event| match event {
            SessionEvent::TurnDone { status, .. } => Some(status),
            _ => None,
        });
        if boundary.is_some_and(|status| {
            !defer_completed_boundary || !matches!(status, TurnStatus::Completed)
        }) {
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
                    call_id: None,
                    target: summarize_input(&input),
                    name,
                },
                SessionEvent::ToolCallCorrelated {
                    call_id,
                    name,
                    input,
                } => SubagentEntry::Call {
                    call_id: Some(call_id),
                    target: summarize_input(&input),
                    name,
                },
                SessionEvent::ToolResult { ok, summary } => SubagentEntry::Result {
                    call_id: None,
                    ok,
                    summary,
                },
                SessionEvent::ToolResultCorrelated {
                    call_id,
                    ok,
                    summary,
                } => SubagentEntry::Result {
                    call_id: Some(call_id),
                    ok,
                    summary,
                },
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

/// Orders the main agent around Claude's background sub-agents. Lifecycle
/// frames are authoritative: while their live set is non-empty, main-line text,
/// reasoning, and a clean `TurnDone` stay private to the pump. Terminal task
/// output is emitted first, followed by held main output in arrival order and
/// finally the deferred boundary. Failures and interrupts pass through.
#[derive(Default)]
struct SubagentOutputGate {
    grouper: SubagentGrouper,
    live: std::collections::BTreeSet<String>,
    held_main: Vec<SessionEvent>,
    pending_done: Option<SessionEvent>,
}

impl SubagentOutputGate {
    fn on_line(&mut self, line: &str) -> Vec<SessionEvent> {
        let parsed = serde_json::from_str::<Value>(line.trim()).ok();
        let main_frame = parsed
            .as_ref()
            .is_some_and(|v| parent_tool_use_id(v).is_none());
        let main_stream_delta = main_frame
            && parsed
                .as_ref()
                .is_some_and(|v| v.get("type").and_then(Value::as_str) == Some("stream_event"));
        let events = self
            .grouper
            .on_line_with_deferred_boundary(line, !self.live.is_empty());
        self.route(events, main_stream_delta, main_frame)
    }

    fn route(
        &mut self,
        events: Vec<SessionEvent>,
        main_stream_delta: bool,
        main_frame: bool,
    ) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        for event in events {
            if main_stream_delta
                && !self.live.is_empty()
                && matches!(
                    &event,
                    SessionEvent::TextDelta(_) | SessionEvent::ThinkingDelta(_)
                )
            {
                self.held_main.push(event);
                continue;
            }

            match event {
                SessionEvent::BackgroundTask(signal) => {
                    let removed = self.observe_background(&signal);
                    for id in removed {
                        out.extend(self.grouper.flush_buffer(&id));
                    }
                    out.push(SessionEvent::BackgroundTask(signal));
                    if self.live.is_empty() {
                        if self.pending_done.is_some() {
                            out.extend(self.grouper.flush_all());
                        }
                        self.release_deferred(&mut out);
                    }
                }
                SessionEvent::TurnDone { .. } if !main_frame => {}
                event @ SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    ..
                } if !self.live.is_empty() => {
                    if self.pending_done.is_none() {
                        self.pending_done = Some(event);
                    }
                }
                event @ SessionEvent::TurnDone { .. } => {
                    out.append(&mut self.held_main);
                    self.pending_done = None;
                    out.push(event);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn observe_background(&mut self, signal: &BackgroundTaskSignal) -> Vec<String> {
        match signal {
            BackgroundTaskSignal::Started { id } => {
                self.live.insert(id.clone());
                vec![]
            }
            BackgroundTaskSignal::Finished { id } => {
                self.live.remove(id);
                vec![id.clone()]
            }
            BackgroundTaskSignal::Live { agent_ids } => {
                let next: std::collections::BTreeSet<String> = agent_ids.iter().cloned().collect();
                let removed = self.live.difference(&next).cloned().collect();
                self.live = next;
                removed
            }
        }
    }

    fn release_deferred(&mut self, out: &mut Vec<SessionEvent>) {
        out.append(&mut self.held_main);
        if let Some(done) = self.pending_done.take() {
            out.push(done);
        }
    }

    fn finish_stream(&mut self) -> Vec<SessionEvent> {
        let mut out = self.grouper.flush_all();
        out.append(&mut self.held_main);
        self.pending_done = None;
        out
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
    let mut correlated_calls: HashMap<String, usize> = HashMap::new();
    let mut legacy_calls: VecDeque<usize> = VecDeque::new();
    for e in entries {
        match e {
            SubagentEntry::Text(t) => {
                text_run.push_str(t);
            }
            SubagentEntry::Thinking(_) => {}
            SubagentEntry::Call {
                call_id,
                name,
                target,
            } => {
                push_text_run(&mut lines, &mut text_run);
                let call = if target.is_empty() {
                    name.clone()
                } else {
                    format!("{name}({})", truncate(target, 80))
                };
                let index = lines.len();
                lines.push(call);
                match call_id {
                    Some(call_id) => {
                        correlated_calls.insert(call_id.clone(), index);
                    }
                    None => legacy_calls.push_back(index),
                }
            }
            SubagentEntry::Result {
                call_id,
                ok,
                summary,
            } => {
                push_text_run(&mut lines, &mut text_run);
                let s = truncate(first_line(summary), SUBAGENT_ROW_CAP);
                let call_index = match call_id {
                    Some(call_id) => correlated_calls.remove(call_id),
                    None => legacy_calls.pop_front(),
                };
                let mut line = call_index.map_or_else(
                    || format!("→ {s}"),
                    |index| format!("{} → {s}", lines[index]),
                );
                if !ok {
                    line.push_str(SUBAGENT_ROW_FAILED);
                }
                if let Some(index) = call_index {
                    lines[index] = line;
                } else {
                    lines.push(line);
                }
            }
        }
    }
    push_text_run(&mut lines, &mut text_run);
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
    let base = if session.is_empty() {
        format!("subtype={subtype}")
    } else {
        format!("subtype={subtype} session_id={session}")
    };
    if subtype != "init" {
        return base;
    }
    let tools = v
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let has_ask = tools
        .iter()
        .any(|tool| tool.as_str().is_some_and(AskUserQuestion::is_tool_name));
    let has_exit_plan = tools
        .iter()
        .any(|tool| tool.as_str().is_some_and(ExitPlanMode::is_tool_name));
    format!(
        "{base} tools={} ask_user_question={has_ask} exit_plan_mode={has_exit_plan}",
        tools.len()
    )
}

/// Turn one official `system/api_retry` frame into visible, non-terminal
/// progress. Only documented enum/numeric fields are surfaced; arbitrary
/// response bodies are never copied into logs or the transcript.
fn api_retry_event(v: &Value) -> Option<SessionEvent> {
    if v.get("subtype").and_then(Value::as_str) != Some("api_retry") {
        return None;
    }
    let attempt = v.get("attempt").and_then(Value::as_u64);
    let max_retries = v.get("max_retries").and_then(Value::as_u64);
    let delay_ms = v.get("retry_delay_ms").and_then(Value::as_u64);
    let category = v
        .get("error")
        .and_then(Value::as_str)
        .map_or_else(|| "unknown error".to_string(), |error| truncate(error, 80));
    let status = v
        .get("error_status")
        .and_then(Value::as_u64)
        .map_or_else(String::new, |status| format!(" (HTTP {status})"));
    let attempt_text = match (attempt, max_retries) {
        (Some(current), Some(maximum)) => format!(" attempt {current}/{maximum}"),
        (Some(current), None) => format!(" attempt {current}"),
        _ => String::new(),
    };
    let delay_text = delay_ms.map_or_else(String::new, |delay| format!(" in {delay} ms"));
    Some(SessionEvent::ToolOutputDelta(format!(
        "[warning] Claude API retry{attempt_text}{delay_text}: {category}{status}"
    )))
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
            match block
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                Some(call_id) => Some(SessionEvent::ToolCallCorrelated {
                    call_id: call_id.to_string(),
                    name,
                    input,
                }),
                None => Some(SessionEvent::ToolCall { name, input }),
            }
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
    let summary = summarize_tool_content(block.get("content"));
    match block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        Some(call_id) => Some(SessionEvent::ToolResultCorrelated {
            call_id: call_id.to_string(),
            ok,
            summary,
        }),
        None => Some(SessionEvent::ToolResult { ok, summary }),
    }
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
///
/// Newer Claude builds also emit `terminal_reason`. That field distinguishes
/// formerly silent dead turns (`api_error`, `turn_setup_failed`, malformed tool
/// exhaustion, etc.) from a genuine completion, and is authoritative whenever
/// it names a reason UmaDev understands. Unknown future values deliberately fall
/// back to the legacy subtype/is_error contract instead of crashing the stream.
fn parse_result(v: &Value) -> SessionEvent {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let legacy_status = || match subtype {
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
    let status = v
        .get("terminal_reason")
        .and_then(Value::as_str)
        .and_then(|reason| terminal_reason_status(v, reason, is_error))
        .unwrap_or_else(legacy_status);
    // F3: surface the REAL per-turn token usage off the `result` line so `/usage`
    // is truthful on the DEFAULT continuous loop (claude reports it; previously
    // only the legacy single-shot `claude.rs` path read it). Fail-open: a result
    // line with no `usage` object yields `None` → the consumer estimates instead.
    SessionEvent::TurnDone {
        status,
        usage: parse_result_usage(v),
    }
}

/// Map Claude's documented terminal reasons onto UmaDev's smaller, stable turn
/// status contract. `None` means a future/unknown reason and asks the caller to
/// use the backwards-compatible subtype/is_error mapping.
fn terminal_reason_status(v: &Value, reason: &str, is_error: bool) -> Option<TurnStatus> {
    match reason {
        "completed" | "background_requested" | "tool_deferred" if !is_error => {
            Some(TurnStatus::Completed)
        }
        "max_turns" | "budget_exhausted" | "structured_output_retry_exhausted" => {
            Some(TurnStatus::Truncated)
        }
        "aborted_streaming" | "aborted_tools" => Some(TurnStatus::Interrupted),
        "blocking_limit"
        | "rapid_refill_breaker"
        | "prompt_too_long"
        | "image_error"
        | "model_error"
        | "api_error"
        | "malformed_tool_use_exhausted"
        | "stop_hook_prevented"
        | "hook_stopped"
        | "tool_deferred_unavailable"
        | "turn_setup_failed" => Some(TurnStatus::Failed(result_error_text(v, reason))),
        // A contradictory `completed + is_error:true` must never hide the base's
        // own error. Future official values stay forward-compatible.
        "completed" | "background_requested" | "tool_deferred" => {
            Some(TurnStatus::Failed(result_error_text(v, reason)))
        }
        _ => None,
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
    let required = |key: &str| u.get(key)?.as_u64();
    let optional = |key: &str| -> Option<u64> {
        match u.get(key) {
            None => Some(0),
            Some(value) => value.as_u64(),
        }
    };
    let input = required("input_tokens")?;
    let output_tokens = required("output_tokens")?;
    let cached_read_tokens = optional("cache_read_input_tokens")?;
    let cached_write_tokens = optional("cache_creation_input_tokens")?;
    let input_tokens = input
        .checked_add(cached_read_tokens)?
        .checked_add(cached_write_tokens)?;
    Some(Usage {
        cached_read_tokens,
        cached_write_tokens,
        ..Usage::exact(input_tokens, output_tokens)
    })
}

/// Translate Claude's `can_use_tool` callback without flattening interactive
/// tools into a binary approval. Questions and plan confirmation retain their
/// full original input in metadata; ordinary tools keep the stable legacy
/// `NeedApproval` surface for backward compatibility.
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
    let input = req
        .and_then(|r| r.get("input"))
        .cloned()
        .unwrap_or(Value::Null);

    if let Some(request) = typed_claude_interaction(&action, &input) {
        return vec![SessionEvent::HostRequest { req_id, request }];
    }

    let target = summarize_input(&input);
    vec![SessionEvent::NeedApproval {
        req_id,
        action,
        target,
    }]
}

fn typed_claude_interaction(tool_name: &str, input: &Value) -> Option<HostRequest> {
    if AskUserQuestion::is_tool_name(tool_name) {
        let Some(parsed) = AskUserQuestion::parse_value(input) else {
            return Some(HostRequest::Unknown {
                method: "claude/can_use_tool/AskUserQuestion".to_string(),
                payload: input.clone(),
            });
        };
        let questions = parsed
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| HostQuestion {
                id: claude_question_id(question, index),
                header: (!question.header.trim().is_empty()).then(|| question.header.clone()),
                prompt: if question.question.trim().is_empty() {
                    question.header.clone()
                } else {
                    question.question.clone()
                },
                kind: if question.options.is_empty() {
                    HostQuestionKind::Text
                } else if question.multi_select {
                    HostQuestionKind::MultiChoice
                } else {
                    HostQuestionKind::SingleChoice
                },
                required: true,
                options: question
                    .options
                    .iter()
                    .map(|option| HostQuestionOption {
                        value: option.label.clone(),
                        label: option.label.clone(),
                        description: (!option.description.trim().is_empty())
                            .then(|| option.description.clone()),
                        preview: None,
                    })
                    .collect(),
            })
            .collect();
        return Some(HostRequest::UserInput {
            questions,
            metadata: serde_json::json!({
                "protocol": "claude-stream-json",
                "tool_name": tool_name,
                "original_input": input
            }),
        });
    }

    if ExitPlanMode::is_tool_name(tool_name) {
        let Some(plan) = ExitPlanMode::parse_value(input) else {
            return Some(HostRequest::Unknown {
                method: "claude/can_use_tool/ExitPlanMode".to_string(),
                payload: input.clone(),
            });
        };
        return Some(HostRequest::PlanConfirmation {
            plan: plan.plan,
            message: Some("Claude is ready to leave plan mode and begin execution".to_string()),
            metadata: serde_json::json!({
                "protocol": "claude-stream-json",
                "tool_name": tool_name,
                "original_input": input
            }),
        });
    }

    None
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
        u[0],
        u[1],
        u[2],
        u[3],
        u[4],
        u[5],
        u[6],
        u[7],
        u[8],
        u[9],
        u[10],
        u[11],
        u[12],
        u[13],
        u[14],
        u[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the two process-wide permission latches.
    /// Pure policy tests use [`resolve_claude_permission_args`] and need no lock.
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
        let uuid = v["uuid"].as_str().expect("client UUID");
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
    }

    #[test]
    fn replay_ack_requires_the_official_replay_shape_and_exact_uuid() {
        assert_eq!(
            replay_ack_uuid(
                r#"{"type":"user","uuid":"u-1","session_id":"s","message":{"role":"user","content":"x"},"parent_tool_use_id":null,"isReplay":true}"#
            )
            .as_deref(),
            Some("u-1")
        );
        for not_an_ack in [
            r#"{"type":"user","uuid":"u-1","isReplay":true}"#,
            r#"{"type":"user","uuid":"u-1","isReplay":false}"#,
            r#"{"type":"user","uuid":"u-1"}"#,
            r#"{"type":"user","isReplay":true}"#,
            r#"{"type":"assistant","uuid":"u-1","isReplay":true}"#,
            r#"{"type":"user","uuid":"u-1","session_id":"","message":{"role":"user"},"parent_tool_use_id":null,"isReplay":true}"#,
            r#"{"type":"user","uuid":"u-1","session_id":"s","message":{"role":"assistant"},"parent_tool_use_id":null,"isReplay":true}"#,
            r#"{"type":"user","uuid":"u-1","session_id":"s","message":{"role":"user"},"isReplay":true}"#,
            r#"{"type":"user","uuid":"u-1","session_id":"s","message":{"role":"user"},"parent_tool_use_id":7,"isReplay":true}"#,
            "not-json",
        ] {
            assert_eq!(replay_ack_uuid(not_an_ack), None, "frame: {not_an_ack}");
        }
    }

    #[tokio::test]
    async fn replay_ack_store_handles_out_of_order_duplicate_and_bounded_eviction() {
        let mut pending = PendingClaudeReplayAcks::default();
        let first = pending.register("first".to_string());
        let second = pending.register("second".to_string());
        assert!(pending.acknowledge("second"));
        assert!(second.await.is_ok(), "second ACK may arrive first");
        assert!(pending.acknowledge("first"));
        assert!(first.await.is_ok());
        assert!(!pending.acknowledge("first"), "duplicate ACK is ignored");
        assert_eq!(pending.len(), 0);

        let mut receivers = Vec::new();
        for index in 0..=PENDING_REPLAY_ACK_CAP {
            receivers.push(pending.register(format!("bounded-{index}")));
        }
        assert_eq!(pending.len(), PENDING_REPLAY_ACK_CAP);
        assert!(
            receivers.remove(0).await.is_err(),
            "oldest waiter is woken when the bounded map evicts it"
        );
        pending.clear();
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn structured_user_line_preserves_text_image_text_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("截图 空格.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").unwrap();
        let prepared = crate::turn_input::prepare(TurnInput::new(vec![
            umadev_runtime::TurnInputBlock::Text {
                text: "before".into(),
            },
            umadev_runtime::TurnInputBlock::Image { path: image },
            umadev_runtime::TurnInputBlock::Text {
                text: "after".into(),
            },
        ]))
        .await
        .unwrap();
        let (line, deliveries, uuid) = claude_user_message_line(&prepared).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["uuid"], uuid);
        let content = value["message"]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "before");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[2]["text"], "after");
        assert_eq!(deliveries, vec![InputDelivery::Native; 3]);
    }

    #[tokio::test]
    async fn generic_file_requires_explicit_text_materialization() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("private-name.txt");
        std::fs::write(&file, "hello").unwrap();
        let prepared = crate::turn_input::prepare(TurnInput::new(vec![
            umadev_runtime::TurnInputBlock::File {
                path: file,
                mode: FileInputMode::NativeOnly,
            },
        ]))
        .await
        .unwrap();
        let error = claude_user_message_line(&prepared).unwrap_err();
        assert!(matches!(error, SessionError::InputUnsupported { .. }));
        assert!(!error.to_string().contains("private-name"));
    }

    #[test]
    fn session_args_use_append_not_replace_system_prompt() {
        let args = session_args("sid-1", Some("be terse"), true, None);
        assert!(args.contains(&"--input-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"sid-1".to_string()));
        assert!(args.contains(&"--replay-user-messages".to_string()));
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
    /// codex / opencode): autonomous → `bypassPermissions` (full access, never
    /// interrupts; governance hooks still audit), guarded → `default` (claude
    /// asks per tool → a NeedApproval the orchestrator answers, so the
    /// human-in-the-loop / irreversible-action floor is live).
    #[test]
    fn guarded_gates_mutating_tools_but_auto_pre_approves_all() {
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");
        let _no_skip = EnvRestore::remove("UMADEV_NO_SKIP_PERMS");
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
        let _no_skip = EnvRestore::remove("UMADEV_NO_SKIP_PERMS");

        let auto = session_args("sid-a", None, true, None);
        let auto_idx = auto.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(
            auto[auto_idx + 1],
            "bypassPermissions",
            "auto → bypassPermissions (full access — the base itself never prompts; \
             cross-base parity with codex `approvalPolicy: never` + opencode wildcard-allow)"
        );

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

        let plan = session_args_for_profile("sid-p", None, BasePermissionProfile::Plan, None, "");
        let p_idx = plan.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(plan[p_idx + 1], "plan");
        let tools_idx = plan.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(plan[tools_idx + 1], PLAN_ALLOWED_TOOLS);
        for mutating in ["Edit", "Write", "Bash", "NotebookEdit", "Agent", "Task"] {
            assert!(!plan[tools_idx + 1].split(',').any(|tool| tool == mutating));
        }

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
            "Auto accepts a known tighter override"
        );
        let overridden_tools = overridden
            .iter()
            .position(|a| a == "--allowedTools")
            .unwrap();
        assert_eq!(overridden[overridden_tools + 1], PLAN_ALLOWED_TOOLS);

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

        // A hostile widening override is ignored on Guarded too.
        std::env::set_var("UMADEV_CLAUDE_PERMISSION_MODE", "acceptEdits");
        let guarded_accept = session_args("sid-ga", None, false, None);
        let accept_pos = guarded_accept
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(
            guarded_accept[accept_pos + 1],
            "default",
            "Guarded cannot be widened by an environment override"
        );
    }

    #[test]
    fn session_args_emit_the_selected_model_and_skip_placeholders() {
        // The HIGH audit finding: the continuous (default) claude path silently dropped
        // the user's /model choice. Both the fresh and resume arg builders must now emit
        // `--model <id>` for a real id, and skip it for an empty / placeholder id so a
        // default run never injects a bogus --model.
        let fresh = session_args_for_profile(
            "sid-m",
            None,
            BasePermissionProfile::Auto,
            None,
            "claude-opus-4-8",
        );
        let m = fresh
            .iter()
            .position(|a| a == "--model")
            .expect("the fresh continuous session passes --model");
        assert_eq!(fresh[m + 1], "claude-opus-4-8");

        let resumed = resume_session_args_for_profile(
            "sid-m",
            None,
            BasePermissionProfile::Auto,
            None,
            "claude-opus-4-8",
        );
        let rm = resumed
            .iter()
            .position(|a| a == "--model")
            .expect("a /continue resume passes --model too");
        assert_eq!(resumed[rm + 1], "claude-opus-4-8");

        // An empty id and the offline/test placeholders never inject a bogus --model.
        for placeholder in ["", "offline", "stub", "m"] {
            let args = session_args_for_profile(
                "sid-e",
                None,
                BasePermissionProfile::Auto,
                None,
                placeholder,
            );
            assert!(
                !args.iter().any(|a| a == "--model"),
                "no --model is injected for the placeholder id `{placeholder}`"
            );
        }
    }

    #[test]
    fn bypass_override_is_confined_to_auto_and_no_skip_tightens_it() {
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");
        let _no_skip = EnvRestore::remove("UMADEV_NO_SKIP_PERMS");
        std::env::set_var("UMADEV_CLAUDE_PERMISSION_MODE", "bypassPermissions");
        for (profile, expected) in [
            (BasePermissionProfile::Plan, "plan"),
            (BasePermissionProfile::Guarded, "default"),
            (BasePermissionProfile::Auto, "bypassPermissions"),
        ] {
            let args = session_args_for_profile("sid-b", None, profile, None, "");
            let p = args.iter().position(|a| a == "--permission-mode").unwrap();
            assert_eq!(args[p + 1], expected, "profile {profile:?}: {args:?}");
        }

        std::env::set_var("UMADEV_NO_SKIP_PERMS", "1");
        let tightened =
            session_args_for_profile("sid-t", None, BasePermissionProfile::Auto, None, "");
        let p = tightened
            .iter()
            .position(|a| a == "--permission-mode")
            .unwrap();
        assert_eq!(tightened[p + 1], "default");
        let tools = tightened
            .iter()
            .position(|a| a == "--allowedTools")
            .unwrap();
        assert_eq!(tightened[tools + 1], GUARDED_ALLOWED_TOOLS);
    }

    #[test]
    fn pure_permission_policy_rejects_widening_and_only_tightens_auto() {
        for hostile in ["bypassPermissions", "acceptEdits", "future-root-mode"] {
            assert_eq!(
                resolve_claude_permission_args(BasePermissionProfile::Plan, Some(hostile), false,),
                ("plan", PLAN_ALLOWED_TOOLS)
            );
            assert_eq!(
                resolve_claude_permission_args(
                    BasePermissionProfile::Guarded,
                    Some(hostile),
                    false,
                ),
                ("default", GUARDED_ALLOWED_TOOLS)
            );
        }

        for (override_mode, expected) in [
            (None, ("bypassPermissions", AUTO_ALLOWED_TOOLS)),
            (
                Some("bypassPermissions"),
                ("bypassPermissions", AUTO_ALLOWED_TOOLS),
            ),
            (Some("auto"), ("auto", GUARDED_ALLOWED_TOOLS)),
            (Some("acceptEdits"), ("acceptEdits", GUARDED_ALLOWED_TOOLS)),
            (Some("default"), ("default", GUARDED_ALLOWED_TOOLS)),
            (Some("manual"), ("manual", GUARDED_ALLOWED_TOOLS)),
            (Some("dontAsk"), ("dontAsk", GUARDED_ALLOWED_TOOLS)),
            (Some("plan"), ("plan", PLAN_ALLOWED_TOOLS)),
            (Some("unknown"), ("default", GUARDED_ALLOWED_TOOLS)),
        ] {
            assert_eq!(
                resolve_claude_permission_args(BasePermissionProfile::Auto, override_mode, false,),
                expected
            );
        }
        assert_eq!(
            resolve_claude_permission_args(
                BasePermissionProfile::Auto,
                Some("bypassPermissions"),
                true,
            ),
            ("default", GUARDED_ALLOWED_TOOLS)
        );
        assert_eq!(
            resolve_claude_permission_args(BasePermissionProfile::Auto, Some("auto"), true),
            ("auto", GUARDED_ALLOWED_TOOLS),
            "official classifier-backed auto is not raw bypass and remains available under no-skip"
        );
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
        let _no_skip = EnvRestore::remove("UMADEV_NO_SKIP_PERMS");

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
        assert_eq!(
            args[perm + 1],
            "bypassPermissions",
            "autonomous → bypassPermissions"
        );
        // Streams partial messages so a resumed reply renders token-by-token.
        assert!(args.iter().any(|a| a == "--include-partial-messages"));
        assert!(args.iter().any(|a| a == "--replay-user-messages"));
        // Firmware still injects natively on resume.
        assert!(args.contains(&"--append-system-prompt".to_string()));
        assert!(args.contains(&"be terse".to_string()));
    }

    #[test]
    fn fresh_and_resume_preserve_each_permission_profile_exactly() {
        let _lock = PERM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _mode = EnvRestore::remove("UMADEV_CLAUDE_PERMISSION_MODE");
        let _no_skip = EnvRestore::remove("UMADEV_NO_SKIP_PERMS");

        for profile in [
            BasePermissionProfile::Plan,
            BasePermissionProfile::Guarded,
            BasePermissionProfile::Auto,
        ] {
            let fresh = session_args_for_profile("fresh-id", None, profile, None, "");
            let resumed = resume_session_args_for_profile("resume-id", None, profile, None, "");
            for flag in ["--permission-mode", "--allowedTools"] {
                let fresh_value = fresh
                    .iter()
                    .position(|argument| argument == flag)
                    .and_then(|index| fresh.get(index + 1));
                let resumed_value = resumed
                    .iter()
                    .position(|argument| argument == flag)
                    .and_then(|index| resumed.get(index + 1));
                assert_eq!(fresh_value, resumed_value, "{profile:?} {flag}");
            }
            assert!(fresh.iter().any(|argument| argument == "--session-id"));
            assert!(!fresh.iter().any(|argument| argument == "--resume"));
            assert!(resumed.iter().any(|argument| argument == "--resume"));
            assert!(!resumed.iter().any(|argument| argument == "--session-id"));
        }
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
        assert!(args.contains(&"--replay-user-messages".to_string()));
        // Read-only: plan mode + a read-only tool allowlist (no Write / Edit).
        let perm = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[perm + 1], "plan");
        let tools = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(args[tools + 1], "Read,Grep,Glob");
        assert!(!args[tools + 1].contains("Write"));
        assert!(!args[tools + 1].contains("Edit"));
    }

    /// A fake `claude` that reports the security-relevant fork argv before
    /// streaming a JSON verdict. This tests the actual child-process parameters,
    /// not just the pure argument builder.
    #[cfg(unix)]
    const FORK_ARGV_REPORTING_FAKE: &str = "#!/bin/sh\n\
         case \" $* \" in *\" --resume \"*) RESUME=resumed ;; *) RESUME=fresh ;; esac\n\
         case \" $* \" in *\" --fork-session \"*) BRANCH=branched ;; *) BRANCH=independent ;; esac\n\
         case \" $* \" in *\" --session-id sid-main \"*) ID=reused ;; *\" --session-id \"*) ID=pinned ;; *) ID=unpinned ;; esac\n\
         case \" $* \" in *\" --permission-mode plan \"*) PERM=plan ;; *) PERM=unsafe ;; esac\n\
         case \" $* \" in *\" --allowedTools Read,Grep,Glob \"*) TOOLS=readonly ;; *) TOOLS=unexpected ;; esac\n\
         read _line\n\
         printf '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"%s %s %s %s %s \"}}}\\n' \"$RESUME\" \"$BRANCH\" \"$ID\" \"$PERM\" \"$TOOLS\"\n\
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
    async fn fork_before_first_writer_turn_is_fresh_independent_and_read_only() {
        // The main process has a real non-empty session id but has NOT received its
        // first writer turn. fork() must still start successfully without trying to
        // resume that not-yet-persisted session, and its argv must carry the hard Plan
        // boundary plus the narrow prompt-free read list.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(&tmp, FORK_ARGV_REPORTING_FAKE);
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
            text.contains("fresh independent pinned plan readonly"),
            "fork child argv must be clean, independent, pinned, and read-only: {text}"
        );
        assert!(
            text.contains("accepts"),
            "fork relayed the verdict text: {text}"
        );
        let _ = fork.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fork_is_equally_fresh_when_parent_session_id_is_empty() {
        // An empty parent id uses exactly the same clean/read-only process shape;
        // production behavior does not branch on parent transcript availability.
        let tmp = tempfile_dir();
        let fake = write_fake_claude(&tmp, FORK_ARGV_REPORTING_FAKE);
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
            text.contains("fresh independent pinned plan readonly"),
            "empty parent id must use the same clean/read-only child argv: {text}"
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
    fn parallel_tool_results_keep_claude_ids_when_they_finish_out_of_order() {
        let calls = parse_stdout_line(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"tool-A","name":"Read","input":{"file_path":"a.rs"}},
                {"type":"tool_use","id":"tool-B","name":"Read","input":{"file_path":"b.rs"}}
            ]}}"#,
        );
        let results = parse_stdout_line(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"tool-B","content":"result B"},
                {"type":"tool_result","tool_use_id":"tool-A","content":"result A"}
            ]}}"#,
        );
        assert!(matches!(
            calls.as_slice(),
            [
                SessionEvent::ToolCallCorrelated { call_id: a, .. },
                SessionEvent::ToolCallCorrelated { call_id: b, .. }
            ] if a == "tool-A" && b == "tool-B"
        ));
        assert!(matches!(
            results.as_slice(),
            [
                SessionEvent::ToolResultCorrelated { call_id: b, summary: sb, .. },
                SessionEvent::ToolResultCorrelated { call_id: a, summary: sa, .. }
            ] if b == "tool-B" && sb == "result B" && a == "tool-A" && sa == "result A"
        ));

        let mut activity = umadev_runtime::ToolActivity::default();
        assert!(activity.observe(&calls[0]));
        assert!(activity.observe(&calls[1]));
        assert!(
            activity.observe(&results[0]),
            "finishing B must leave A active"
        );
        assert!(
            !activity.observe(&results[1]),
            "A then settles independently"
        );
    }

    #[test]
    fn subagent_compaction_maps_out_of_order_results_by_claude_id() {
        let entries = vec![
            SubagentEntry::Call {
                call_id: Some("tool-A".to_string()),
                name: "Read".to_string(),
                target: "a.rs".to_string(),
            },
            SubagentEntry::Call {
                call_id: Some("tool-B".to_string()),
                name: "Read".to_string(),
                target: "b.rs".to_string(),
            },
            SubagentEntry::Result {
                call_id: Some("tool-B".to_string()),
                ok: true,
                summary: "result B".to_string(),
            },
            SubagentEntry::Result {
                call_id: Some("tool-A".to_string()),
                ok: true,
                summary: "result A".to_string(),
            },
        ];
        let (ok, body) = render_subagent_body(&entries);
        assert!(ok);
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines[0], "Read(a.rs) → result A");
        assert_eq!(lines[1], "Read(b.rs) → result B");
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
    fn terminal_reason_is_authoritative_for_new_claude_dead_and_interrupted_turns() {
        let api_error = parse_stdout_line(
            r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"api_error","result":"upstream exhausted retries"}"#,
        );
        assert!(matches!(
            api_error.as_slice(),
            [SessionEvent::TurnDone {
                status: TurnStatus::Failed(reason),
                ..
            }] if reason == "upstream exhausted retries"
        ));

        let interrupted = parse_stdout_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools"}"#,
        );
        assert_eq!(
            interrupted,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Interrupted,
                usage: None,
            }]
        );

        let exhausted = parse_stdout_line(
            r#"{"type":"result","subtype":"success","terminal_reason":"budget_exhausted"}"#,
        );
        assert_eq!(
            exhausted,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Truncated,
                usage: None,
            }]
        );

        // The protocol is explicitly open-ended. A future value retains the
        // older, known-safe subtype/is_error behavior instead of becoming a
        // false failure or panic.
        let future = parse_stdout_line(
            r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"future_clean_reason"}"#,
        );
        assert_eq!(
            future,
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }]
        );
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
        let line = r#"{"type":"result","subtype":"success","usage":{"input_tokens":1200,"cache_read_input_tokens":300,"cache_creation_input_tokens":50,"output_tokens":450},"total_cost_usd":0.02}"#;
        let evs = parse_stdout_line(line);
        match evs.as_slice() {
            [SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: Some(u),
            }] => {
                assert_eq!(u.input_tokens, 1550);
                assert_eq!(u.output_tokens, 450);
                assert_eq!(u.cached_read_tokens, 300);
                assert_eq!(u.cached_write_tokens, 50);
                assert!(!u.usage_incomplete);
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

        for invalid in [
            r#"{"type":"result","subtype":"success","usage":{}}"#,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":null}}"#,
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":18446744073709551615,"output_tokens":2,"cache_read_input_tokens":1}}"#,
        ] {
            assert_eq!(
                parse_stdout_line(invalid),
                vec![SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                }],
                "invalid fixture: {invalid}"
            );
        }
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
    fn ask_user_question_is_typed_and_crlf_safe() {
        let line = concat!(
            r#"{"type":"control_request","request_id":"ask-1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":["#,
            r#"{"header":"Scope","question":"Which areas?","multiSelect":true,"options":[{"label":"API","description":"Backend"},{"label":"UI","description":"Frontend"}]},"#,
            r#"{"header":"Notes","question":"Any extra constraints?","multiSelect":false,"options":[{"label":"None","description":"No extras"},{"label":"Other","description":"Free text is accepted"}]}]}}}"#,
            "\r\n"
        );
        let events = parse_stdout_line(line);
        let [SessionEvent::HostRequest { req_id, request }] = events.as_slice() else {
            panic!("expected one typed host request, got {events:?}");
        };
        assert_eq!(req_id, "ask-1");
        let HostRequest::UserInput {
            questions,
            metadata,
        } = request
        else {
            panic!("expected user input, got {request:?}");
        };
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].id, "Which areas?");
        assert_eq!(questions[0].kind, HostQuestionKind::MultiChoice);
        assert_eq!(questions[0].options[1].value, "UI");
        assert_eq!(questions[1].kind, HostQuestionKind::SingleChoice);
        assert_eq!(
            metadata["original_input"]["questions"][0]["header"],
            "Scope"
        );
    }

    #[test]
    fn ask_user_question_response_preserves_input_and_merges_multi_and_free_text() {
        let input = serde_json::json!({
            "questions": [
                {
                    "header": "Scope",
                    "question": "Which areas?",
                    "multiSelect": true,
                    "options": [
                        {"label": "API", "description": "Backend"},
                        {"label": "UI", "description": "Frontend"}
                    ]
                },
                {
                    "header": "Notes",
                    "question": "Any extra constraints?",
                    "multiSelect": false,
                    "options": [
                        {"label": "None", "description": "No extras"},
                        {"label": "Other", "description": "Custom answer"}
                    ]
                }
            ],
            "metadata": {"source": "test"}
        });
        let pending = PendingClaudeControl {
            tool_name: "AskUserQuestion".to_string(),
            input: input.clone(),
        };
        let payload = typed_host_response_payload(
            HostResponse::UserInput {
                answers: vec![
                    HostAnswer {
                        question_id: "Which areas?".to_string(),
                        values: vec!["API".to_string(), "UI".to_string()],
                    },
                    HostAnswer {
                        question_id: "Any extra constraints?".to_string(),
                        values: vec!["Keep the public API stable".to_string()],
                    },
                ],
            },
            Some(&pending),
        );
        assert_eq!(payload["behavior"], "allow");
        assert_eq!(payload["updatedInput"]["questions"], input["questions"]);
        assert_eq!(payload["updatedInput"]["metadata"], input["metadata"]);
        assert_eq!(
            payload["updatedInput"]["answers"]["Which areas?"],
            "API, UI"
        );
        assert_eq!(
            payload["updatedInput"]["answers"]["Any extra constraints?"],
            "Keep the public API stable"
        );
    }

    #[test]
    fn ask_user_question_reject_and_cancel_carry_messages() {
        let pending = PendingClaudeControl {
            tool_name: "AskUserQuestion".to_string(),
            input: serde_json::json!({
                "questions": [{
                    "header": "Choice",
                    "question": "Proceed?",
                    "multiSelect": false,
                    "options": [
                        {"label": "Yes", "description": "Continue"},
                        {"label": "No", "description": "Stop"}
                    ]
                }]
            }),
        };
        let rejected = typed_host_response_payload(
            HostResponse::Rejected {
                reason: "I do not want to answer".to_string(),
            },
            Some(&pending),
        );
        assert_eq!(rejected["behavior"], "deny");
        assert_eq!(rejected["message"], "I do not want to answer");

        let cancelled = typed_host_response_payload(
            HostResponse::Cancelled {
                reason: Some("User pressed Esc".to_string()),
            },
            Some(&pending),
        );
        assert_eq!(cancelled["behavior"], "deny");
        assert_eq!(cancelled["message"], "User pressed Esc");
    }

    #[test]
    fn exit_plan_mode_is_typed_and_reply_preserves_plan() {
        let input = serde_json::json!({
            "plan": "# Plan\n\n1. Inspect\n2. Implement",
            "allowedPrompts": [{"tool": "Bash", "prompt": "cargo test"}]
        });
        let line = serde_json::json!({
            "type": "control_request",
            "request_id": "plan-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "ExitPlanMode",
                "input": input
            }
        })
        .to_string();
        let events = parse_stdout_line(&line);
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::HostRequest {
                req_id,
                request: HostRequest::PlanConfirmation { plan, .. }
            }] if req_id == "plan-1" && plan.starts_with("# Plan")
        ));

        let pending = PendingClaudeControl {
            tool_name: "ExitPlanMode".to_string(),
            input: input.clone(),
        };
        let allowed = typed_host_response_payload(
            HostResponse::PlanConfirmation {
                decision: ApprovalDecision::Allow,
                feedback: None,
            },
            Some(&pending),
        );
        assert_eq!(allowed["behavior"], "allow");
        assert_eq!(allowed["updatedInput"], input);

        let denied = typed_host_response_payload(
            HostResponse::PlanConfirmation {
                decision: ApprovalDecision::Deny,
                feedback: Some("Please add rollback steps".to_string()),
            },
            Some(&pending),
        );
        assert_eq!(denied["behavior"], "deny");
        assert_eq!(denied["message"], "Please add rollback steps");
    }

    #[test]
    fn ordinary_allow_returns_original_updated_input_and_deny_has_message() {
        let pending = PendingClaudeControl {
            tool_name: "Bash".to_string(),
            input: serde_json::json!({"command": "cargo test", "timeout": 120_000}),
        };
        let allowed = legacy_approval_payload(ApprovalDecision::Allow, Some(&pending));
        assert_eq!(allowed["behavior"], "allow");
        assert_eq!(allowed["updatedInput"], pending.input);

        let denied = legacy_approval_payload(ApprovalDecision::Deny, Some(&pending));
        assert_eq!(denied["behavior"], "deny");
        assert!(denied["message"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn system_api_retry_is_visible_progress() {
        let events = parse_stdout_line(
            r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":5,"retry_delay_ms":1500,"error_status":429,"error":"rate_limit"}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::ToolOutputDelta(message)]
                if message.contains("2/5")
                    && message.contains("1500 ms")
                    && message.contains("rate_limit")
                    && message.contains("HTTP 429")
        ));
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

    #[tokio::test]
    async fn bounded_stream_reader_handles_half_frames_crlf_eof_and_oversize() {
        let bytes = b"{\"a\":1}\r\n{\"b\":2}";
        let mut reader = BufReader::with_capacity(3, &bytes[..]);
        let Some(ClaudeFrameRead::Line(first)) =
            read_bounded_claude_frame(&mut reader, 32).await.unwrap()
        else {
            panic!("first bounded frame");
        };
        assert_eq!(first, b"{\"a\":1}\r\n");
        let Some(ClaudeFrameRead::Line(second)) =
            read_bounded_claude_frame(&mut reader, 32).await.unwrap()
        else {
            panic!("EOF-terminated frame");
        };
        assert_eq!(second, b"{\"b\":2}");
        assert!(read_bounded_claude_frame(&mut reader, 32)
            .await
            .unwrap()
            .is_none());

        let oversized = b"0123456789\n{\"ok\":true}\n";
        let mut reader = BufReader::with_capacity(2, &oversized[..]);
        assert!(matches!(
            read_bounded_claude_frame(&mut reader, 8).await.unwrap(),
            Some(ClaudeFrameRead::Oversized)
        ));
        let Some(ClaudeFrameRead::Line(recovered)) =
            read_bounded_claude_frame(&mut reader, 32).await.unwrap()
        else {
            panic!("reader must stop exactly at the oversized record boundary");
        };
        assert_eq!(recovered, b"{\"ok\":true}\n");
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
    fn background_gate_releases_subagent_then_main_output_then_turn_done() {
        let mut gate = SubagentOutputGate::default();
        let spawn = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"task_bg1","name":"Task",
             "input":{"description":"audit rendering","run_in_background":true}}]}}"#;
        assert_eq!(gate.on_line(spawn), parse_stdout_line(spawn));

        let started = r#"{"type":"system","subtype":"task_started","task_id":"task_bg1","task_type":"local_agent","subagent_type":"Explore"}"#;
        assert_eq!(
            gate.on_line(started),
            vec![SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started {
                    id: "task_bg1".to_string()
                }
            )]
        );

        let nested = r#"{"type":"stream_event","parent_tool_use_id":"task_bg1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"subagent evidence"}}}"#;
        assert!(matches!(
            gate.on_line(nested).as_slice(),
            [SessionEvent::ToolCall { name, .. }] if name.contains(SUBAGENT_WORKING)
        ));

        let main_text = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"interim main report"}}}"#;
        let main_thinking = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"interim reasoning"}}}"#;
        assert!(gate.on_line(main_text).is_empty());
        assert!(gate.on_line(main_thinking).is_empty());

        let nested_turn_done =
            r#"{"type":"result","parent_tool_use_id":"task_bg1","subtype":"success"}"#;
        assert!(
            gate.on_line(nested_turn_done).is_empty(),
            "a nested result is not the main agent's settle boundary"
        );

        let turn_done = r#"{"type":"result","subtype":"success","stop_reason":"end_turn"}"#;
        assert!(gate.on_line(turn_done).is_empty());

        let nested_after_boundary = r#"{"type":"stream_event","parent_tool_use_id":"task_bg1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":" after boundary"}}}"#;
        assert!(
            gate.on_line(nested_after_boundary).is_empty(),
            "the deferred boundary must not reopen or flush the live subagent row"
        );

        let finished = r#"{"type":"system","subtype":"task_notification","task_id":"task_bg1","status":"completed"}"#;
        let events = gate.on_line(finished);
        assert_eq!(events.len(), 6, "ordered terminal release: {events:?}");
        assert!(matches!(
            &events[0],
            SessionEvent::ToolCall { name, .. }
                if name.contains("audit rendering") && !name.contains(SUBAGENT_WORKING)
        ));
        assert!(matches!(
            &events[1],
            SessionEvent::ToolResult { summary, .. }
                if summary.contains("subagent evidence after boundary")
        ));
        assert_eq!(
            events[2],
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished {
                id: "task_bg1".to_string()
            })
        );
        assert_eq!(
            events[3],
            SessionEvent::TextDelta("interim main report".to_string())
        );
        assert_eq!(
            events[4],
            SessionEvent::ThinkingDelta("interim reasoning".to_string())
        );
        assert!(matches!(
            &events[5],
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn background_gate_uses_live_level_to_release_a_missed_terminal_edge() {
        let mut gate = SubagentOutputGate::default();
        let started = r#"{"type":"system","subtype":"task_started","task_id":"task_bg1","task_type":"agent"}"#;
        assert_eq!(gate.on_line(started).len(), 1);
        let nested = r#"{"type":"stream_event","parent_tool_use_id":"task_bg1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"work"}}}"#;
        assert_eq!(gate.on_line(nested).len(), 1);
        let main = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"held"}}}"#;
        assert!(gate.on_line(main).is_empty());
        let done = r#"{"type":"result","subtype":"success"}"#;
        assert!(gate.on_line(done).is_empty());

        let live_empty = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        let events = gate.on_line(live_empty);
        assert!(matches!(&events[0], SessionEvent::ToolCall { .. }));
        assert!(
            matches!(&events[1], SessionEvent::ToolResult { summary, .. } if summary.contains("work"))
        );
        assert_eq!(
            events[2],
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Live { agent_ids: vec![] })
        );
        assert_eq!(events[3], SessionEvent::TextDelta("held".to_string()));
        assert!(matches!(
            &events[4],
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn background_gate_waits_for_every_live_agent() {
        let mut gate = SubagentOutputGate::default();
        for id in ["a1", "a2"] {
            let started = format!(
                r#"{{"type":"system","subtype":"task_started","task_id":"{id}","task_type":"agent"}}"#
            );
            assert_eq!(gate.on_line(&started).len(), 1);
        }
        let main = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"held until both finish"}}}"#;
        assert!(gate.on_line(main).is_empty());
        let done = r#"{"type":"result","subtype":"success"}"#;
        assert!(gate.on_line(done).is_empty());

        let first = r#"{"type":"system","subtype":"task_notification","task_id":"a1","status":"completed"}"#;
        assert_eq!(
            gate.on_line(first),
            vec![SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Finished {
                    id: "a1".to_string()
                }
            )],
            "one terminal edge cannot release output while another agent is live"
        );

        let second =
            r#"{"type":"system","subtype":"task_notification","task_id":"a2","status":"failed"}"#;
        let events = gate.on_line(second);
        assert_eq!(
            events[0],
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished {
                id: "a2".to_string()
            })
        );
        assert_eq!(
            events[1],
            SessionEvent::TextDelta("held until both finish".to_string())
        );
        assert!(matches!(
            &events[2],
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn background_gate_does_not_hide_failure_or_interrupt_boundaries() {
        for status in [
            TurnStatus::Failed("base failed".to_string()),
            TurnStatus::Interrupted,
        ] {
            let mut gate = SubagentOutputGate::default();
            let started =
                r#"{"type":"system","subtype":"task_started","task_id":"a1","task_type":"agent"}"#;
            assert_eq!(gate.on_line(started).len(), 1);
            let main = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"partial"}}}"#;
            assert!(gate.on_line(main).is_empty());

            let events = gate.route(
                vec![SessionEvent::TurnDone {
                    status: status.clone(),
                    usage: None,
                }],
                false,
                true,
            );
            assert_eq!(
                events,
                vec![
                    SessionEvent::TextDelta("partial".to_string()),
                    SessionEvent::TurnDone {
                        status,
                        usage: None
                    }
                ]
            );
        }
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
    async fn structured_send_reports_only_an_exact_replay_ack() {
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            r#"#!/bin/sh
printf '%s\n' '{"type":"system","subtype":"init","model":"fixture"}'
IFS= read -r line
uuid=$(printf '%s\n' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
printf '{"type":"user","uuid":"wrong","session_id":"s","message":{"role":"user","content":"wrong"},"parent_tool_use_id":null,"isReplay":true}\n'
printf '{"type":"user","uuid":"%s","session_id":"s","message":{"role":"user","content":"ack"},"parent_tool_use_id":null,"isReplay":true}\n' "$uuid"
printf '{"type":"user","uuid":"%s","session_id":"s","message":{"role":"user","content":"duplicate"},"parent_tool_use_id":null,"isReplay":true}\n' "$uuid"
printf '%s\n' '{"type":"result","subtype":"success","stop_reason":"end_turn"}'
cat >/dev/null
"#,
        );
        let mut session = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-replay-ack",
            true,
            None,
        )
        .await
        .expect("start");
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::SessionModel(model)) if model == "fixture"
        ));

        let report = session
            .send_input(TurnInput::text("hello"))
            .await
            .expect("send");
        assert_eq!(report.receipt, DeliveryReceiptStage::ProtocolAcknowledged);
        tokio::task::yield_now().await;
        assert_eq!(
            session
                .pending_replay_acks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            0,
            "wrong and duplicate replay frames cannot leak waiters"
        );
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            })
        ));
        let _ = session.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interrupt_receipt_cancels_only_the_exact_known_queued_command() {
        let tmp = tempfile_dir();
        let capture = tmp.join("cancel.json");
        let body = format!(
            r#"#!/bin/sh
printf '%s\n' '{{"type":"system","subtype":"init","capabilities":["interrupt_receipt_v1"]}}'
IFS= read -r user_line
uuid=$(printf '%s\n' "$user_line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
printf '{{"type":"command_lifecycle","command_uuid":"%s","state":"queued"}}\n' "$uuid"
IFS= read -r interrupt_line
request_id=$(printf '%s\n' "$interrupt_line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"%s","response":{{"still_queued":["%s","claude-internal"]}}}}}}\n' "$request_id" "$uuid"
IFS= read -r cancel_line
printf '%s\n' "$cancel_line" > '{}'
cat >/dev/null
"#,
            capture.display()
        );
        let fake = write_fake_claude(&tmp, &body);
        let mut session = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-interrupt-receipt",
            true,
            None,
        )
        .await
        .expect("start");

        // Wait for the init capability to be consumed; this is feature
        // detection, never version sniffing.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if session
                    .protocol
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .interrupt_receipt_v1
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("init capability observed");

        session
            .send_turn("queued command".to_string())
            .await
            .expect("send");
        session.interrupt().await.expect("typed interrupt");

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !capture.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancel frame captured");
        let cancel: Value = serde_json::from_slice(&std::fs::read(&capture).unwrap()).unwrap();
        assert_eq!(cancel["type"], "control_request");
        assert_eq!(cancel["request"]["subtype"], "cancel_async_message");
        let cancelled_uuid = cancel["request"]["message_uuid"]
            .as_str()
            .expect("message uuid");
        assert_ne!(cancelled_uuid, "claude-internal");
        assert!(
            session
                .protocol
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .known_commands
                .contains(cancelled_uuid),
            "the cancellation targets an exact UUID registered before send"
        );
        let _ = session.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_send_correlates_replay_without_blocking_or_leaking() {
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            r#"#!/bin/sh
printf '%s\n' '{"type":"system","subtype":"init","model":"fixture"}'
IFS= read -r line
uuid=$(printf '%s\n' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
printf '{"type":"user","uuid":"%s","session_id":"s","message":{"role":"user","content":"ack"},"parent_tool_use_id":null,"isReplay":true}\n' "$uuid"
cat >/dev/null
"#,
        );
        let mut session = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-legacy-replay",
            true,
            None,
        )
        .await
        .expect("start");
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::SessionModel(model)) if model == "fixture"
        ));

        session
            .send_turn("legacy phase directive".to_string())
            .await
            .expect("transport write returns without waiting for the ACK budget");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let pending = session
                .pending_replay_acks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len();
            if pending == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "exact legacy ACK must settle its bounded waiter"
            );
            tokio::task::yield_now().await;
        }
        let _ = session.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_replay_ack_falls_back_to_transport_written_without_leak() {
        let tmp = tempfile_dir();
        let fake = write_fake_claude(
            &tmp,
            "#!/bin/sh\nIFS= read -r _line\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}'\n\
             cat >/dev/null\n",
        );
        let mut session = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-old-replay-shape",
            true,
            None,
        )
        .await
        .expect("start");
        let started = tokio::time::Instant::now();
        let report = session
            .send_input(TurnInput::text("accepted by an older base"))
            .await
            .expect("transport write remains successful");
        assert_eq!(report.receipt, DeliveryReceiptStage::TransportWritten);
        assert!(
            started.elapsed() < REPLAY_ACK_BUDGET + std::time::Duration::from_secs(1),
            "ACK fallback is wall-clock bounded"
        );
        assert_eq!(
            session
                .pending_replay_acks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            0
        );
        let _ = session.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stream_eof_wakes_replay_waiter_and_keeps_receipt_honest() {
        let tmp = tempfile_dir();
        let fake = write_fake_claude(&tmp, "#!/bin/sh\nIFS= read -r _line\nexit 7\n");
        let mut session = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-replay-eof",
            true,
            None,
        )
        .await
        .expect("start");
        let report = session
            .send_input(TurnInput::text("written before crash"))
            .await
            .expect("the completed transport write still has a receipt");
        assert_eq!(report.receipt, DeliveryReceiptStage::TransportWritten);
        assert_eq!(
            session
                .pending_replay_acks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            0,
            "EOF clears all exact-ACK waiters"
        );
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Failed(_),
                ..
            })
        ));
        let _ = session.end().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn end_reclaims_stderr_drain_when_a_grandchild_holds_the_pipe() {
        let tmp = tempfile_dir();
        let grandchild_pid = tmp.join("grandchild.pid");
        let fake = write_fake_claude(
            &tmp,
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\n\
                 printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fixture\"}}'\n\
                 sleep 30\n",
                grandchild_pid.display()
            ),
        );
        let mut session = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-held-stderr",
            true,
            None,
        )
        .await
        .expect("start fake session");
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::SessionModel(model)) if model == "fixture"
        ));
        assert!(session.stderr_drain.is_active());

        let started = std::time::Instant::now();
        session.end().await.expect("bounded end");
        assert!(!session.stderr_drain.is_active());
        assert!(
            started.elapsed() < END_REAP_BUDGET + std::time::Duration::from_secs(1),
            "end must not wait for the inherited stderr writer"
        );

        if let Ok(pid) = std::fs::read_to_string(&grandchild_pid) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_round_trips_typed_question_answers_to_same_control_request() {
        let tmp = tempfile_dir();
        let response_path = tmp.join("control-response.json");
        let body = format!(
            "#!/bin/sh\n\
             read _turn\n\
             printf '%s\\r\\n' '{{\"type\":\"control_request\",\"request_id\":\"ask-live\",\"request\":{{\"subtype\":\"can_use_tool\",\"tool_name\":\"AskUserQuestion\",\"input\":{{\"questions\":[{{\"header\":\"DB\",\"question\":\"Which database?\",\"multiSelect\":false,\"options\":[{{\"label\":\"Postgres\",\"description\":\"SQL\"}},{{\"label\":\"SQLite\",\"description\":\"Local\"}}]}}]}}}}}}'\n\
             IFS= read -r response\n\
             printf '%s\\n' \"$response\" > '{}'\n\
             printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\"}}'\n\
             cat >/dev/null\n",
            response_path.display()
        );
        let fake = write_fake_claude(&tmp, &body);
        let mut session = ClaudeSession::start_with_program(
            fake.to_str().unwrap(),
            &tmp,
            None,
            "sid-typed-question",
            false,
            None,
        )
        .await
        .expect("start");
        session
            .send_turn("choose a database".to_string())
            .await
            .expect("send");

        let event = session.next_event().await.expect("question event");
        assert!(matches!(
            &event,
            SessionEvent::HostRequest {
                req_id,
                request: HostRequest::UserInput { .. }
            } if req_id == "ask-live"
        ));
        session
            .respond_host(
                "ask-live",
                HostResponse::UserInput {
                    answers: vec![HostAnswer {
                        question_id: "Which database?".to_string(),
                        values: vec!["Postgres".to_string()],
                    }],
                },
            )
            .await
            .expect("typed response");

        while let Some(event) = session.next_event().await {
            if matches!(event, SessionEvent::TurnDone { .. }) {
                break;
            }
        }
        let response: Value = serde_json::from_str(
            &std::fs::read_to_string(&response_path).expect("captured response"),
        )
        .expect("response JSON");
        assert_eq!(response["response"]["request_id"], "ask-live");
        assert_eq!(response["response"]["response"]["behavior"], "allow");
        assert_eq!(
            response["response"]["response"]["updatedInput"]["questions"][0]["header"],
            "DB"
        );
        assert_eq!(
            response["response"]["response"]["updatedInput"]["answers"]["Which database?"],
            "Postgres"
        );
        let _ = session.end().await;
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
        // 0: the main-line Agent spawn row keeps Claude's tool_use id.
        assert!(
            matches!(&got[0], SessionEvent::ToolCallCorrelated { call_id, name, .. }
                if call_id == "sub1" && name == "Agent"),
            "main-line spawn row is correlated: {got:?}"
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
        // 4: the main-line final report answers that exact spawn id.
        assert!(
            matches!(&got[4], SessionEvent::ToolResultCorrelated { call_id, summary, .. }
                if call_id == "sub1" && summary == "report"),
            "main-line final report is correlated: {got:?}"
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

        // Poll for the child to exit + its stderr to be drained, bounded by a
        // WALL-CLOCK deadline (never an unbounded wait).
        //
        // The bound is a deadline, NOT a fixed iteration count. The old loop was
        // `for _ in 0..250 { …; sleep(20ms) }`, which silently conflates "how long
        // we waited" with "how many times we looped": under a saturated box each
        // iteration costs *more* than its 20ms sleep, so the loop burned its 250
        // ticks in ~5.5s of wall time — while the thing it is waiting for (fork +
        // exec + dyld of a fresh `/bin/sh`, competing with 15 other test threads
        // that are also spawning children) took ~7.9s. Nothing was deadlocked; the
        // budget was simply smaller than process-startup latency under load, so the
        // test lost a race it never meant to run. Measured: the child, its stderr
        // line, and its exit all landed together at ~7.9s.
        //
        // The property under test is *observability* — "the driver eventually
        // captures the stderr reason and sees the exit" — not "it happens within N
        // seconds". So the deadline is set far above any plausible scheduling delay
        // rather than tuned to a machine: on an idle box this loop still finishes in
        // tens of milliseconds, and a genuine regression (a child that never exits,
        // a stderr tail that is never captured) still fails, just later.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut exited = false;
        let mut tail = None;
        while std::time::Instant::now() < deadline {
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
        let _no_skip = EnvRestore::remove("UMADEV_NO_SKIP_PERMS");
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
        let _no_skip = EnvRestore::remove("UMADEV_NO_SKIP_PERMS");
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
        // Per-tier caps: a fresh read-only consult fork carries a VERY LOW turn ceiling
        // (a runaway backstop), and a deliberate build session's cap is much higher.
        // A Depth::Standard build tier (see `umadev_agent::router::Depth::max_turns`).
        let build_cap: u32 = 150;
        let build = session_args("sid", None, true, Some(build_cap));
        let bi = build.iter().position(|a| a == "--max-turns").unwrap();
        let build_n: u32 = build[bi + 1].parse().unwrap();

        let fork = fork_session_args("f");
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

    #[tokio::test]
    async fn protocol_state_filters_interrupt_receipts_and_consumes_lifecycle() {
        let mut state = ClaudeProtocolState::default();
        state.observe(&serde_json::json!({
            "type": "system",
            "subtype": "init",
            "capabilities": ["unknown_future_cap", "interrupt_receipt_v1"]
        }));
        assert!(state.interrupt_receipt_v1);

        state.register_command("ours-1".to_string());
        state.register_command("ours-2".to_string());
        let receiver = state.register_client_control("interrupt-1".to_string());
        state.observe(&serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "interrupt-1",
                "response": {
                    "still_queued": ["ours-1", "internal-cron", "ours-1", "ours-2"]
                }
            }
        }));
        let receipt = receiver.await.expect("typed receipt delivered");
        assert_eq!(
            state.known_still_queued(&receipt),
            vec!["ours-1".to_string(), "ours-2".to_string()]
        );

        state.observe(&serde_json::json!({
            "type": "command_lifecycle",
            "command_uuid": "ours-1",
            "state": "completed"
        }));
        assert_eq!(
            state.known_still_queued(&receipt),
            vec!["ours-2".to_string()],
            "a terminal lifecycle frame retires only its exact UUID"
        );
        assert!(
            parse_stdout_line(
                r#"{"type":"command_lifecycle","command_uuid":"ours-2","state":"queued"}"#
            )
            .is_empty(),
            "lifecycle is internal protocol state, never transcript noise"
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

        let capabilities: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"init","tools":["Read","AskUserQuestion","ExitPlanMode"]}"#,
        )
        .unwrap();
        let desc = describe_system_event(&capabilities);
        assert!(desc.contains("tools=3"), "tool count is observable: {desc}");
        assert!(
            desc.contains("ask_user_question=true"),
            "question capability is detected from init: {desc}"
        );
        assert!(
            desc.contains("exit_plan_mode=true"),
            "plan capability is detected from init: {desc}"
        );
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

    #[test]
    fn native_events_redact_before_transcript_tool_activity_and_audit() {
        const SECRET: &str = "SYNTH_CLAUDE_SESSION_SECRET_81";
        let call = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "id": "tool-secret",
                "name": "Bash",
                "input": {
                    "command": format!("curl -H 'Authorization: Bearer {SECRET}' example.test"),
                    "password": SECRET,
                    "nextPageToken": "safe-page-2"
                }
            }]}
        })
        .to_string();
        let result = serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result",
                "tool_use_id": "tool-secret",
                "content": format!("private_key={SECRET}")
            }]}
        })
        .to_string();
        let text = serde_json::json!({
            "type": "stream_event",
            "event": {"type": "content_block_delta", "delta": {
                "type": "text_delta", "text": format!("password={SECRET}")
            }}
        })
        .to_string();
        let mut events = parse_stdout_line(&call);
        events.extend(parse_stdout_line(&result));
        events.extend(parse_stdout_line(&text));
        let audit_view = format!("{events:?}");
        assert!(
            !audit_view.contains(SECRET),
            "event/audit leaked: {audit_view}"
        );
        assert!(audit_view.contains("safe-page-2"));

        let mut activity = umadev_runtime::ToolActivity::default();
        for event in &events {
            activity.observe(event);
        }
    }
}
