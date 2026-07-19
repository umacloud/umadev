//! `OpenCodeSession` — drives `opencode` in the **`opencode serve` HTTP + SSE**
//! protocol as ONE long-lived agentic session (the continuous-session model;
//! see `docs/CONTINUOUS_SESSION_ARCHITECTURE.md` §4.3).
//!
//! This lives ALONGSIDE the single-shot [`OpenCodeDriver`](crate::OpenCodeDriver)
//! in `opencode.rs`, which is unchanged. Where that one is "prompt in -> one
//! text blob out" (a fresh `opencode run` process per phase), this one:
//!
//! - spawns `opencode serve --hostname 127.0.0.1 --port 0` **once** as a resident
//!   HTTP server, scrapes the real bound port from its stdout `listening on
//!   http://HOST:PORT` line, and talks to it over HTTP for the whole run;
//! - opens **one** session (`POST /session`) with a permission-profile ruleset:
//!   Auto silently pre-approves tools, Guarded only pre-approves known read-only
//!   inspection and asks on every other tool, and Plan denies every non-read tool.
//!   The base keeps context across phases and runs its own agentic tool loop (it
//!   WRITES files when the selected profile authorizes it), instead of narrating
//!   a paragraph and asking "shall I continue?";
//! - subscribes the server-sent-events stream (`GET /event`, long-lived) in a
//!   background task that parses each `data: {id,type,properties}` frame into a
//!   [`SessionEvent`](umadev_runtime::SessionEvent);
//! - injects one **directive per phase** (`POST /session/:id/prompt_async`, the
//!   same session = context flows);
//! - exposes the [`umadev_runtime::BaseSession`] contract the 9-phase runner
//!   drives.
//!
//! ## Wire protocol (verified against the upstream `anomalyco/opencode` source)
//!
//! Authoritative references (read directly, not from memory):
//! - serve + listening line: `packages/opencode/src/cli/cmd/serve.ts`
//!   (`opencode server listening on http://${hostname}:${port}`) and the
//!   `--hostname` / `--port` flags in `packages/opencode/src/cli/network.ts`.
//! - auth: `packages/opencode/src/server/auth.ts` — Basic
//!   `base64("opencode:<OPENCODE_SERVER_PASSWORD>")`; default username
//!   `opencode`.
//! - directory routing: the `x-opencode-directory` request header / `?directory=`
//!   query param, in `.../middleware/workspace-routing.ts`.
//! - routes: `.../groups/session.ts` plus `.../handlers/session.ts` (`POST
//!   /session`, `GET/PATCH /session/:id`,
//!   `POST /session/:id/prompt_async`, `POST /session/:id/abort`, and the
//!   destructive `DELETE /session/:id` used only by ephemeral critic forks),
//!   `.../groups/permission.ts` (`POST
//!   /permission/:id/reply`). NOTE the deprecated
//!   `/session/:id/permissions/:id` route — we use the live
//!   `/permission/.../reply`; and `.../groups/question.ts` (`POST
//!   /question/:id/reply|reject`).
//! - create vs prompt model shapes DIFFER: create's `model` is
//!   `{id,providerID,variant?}` (`session.ts CreateInput`); prompt's `model` is
//!   `{providerID,modelID}` (`session/prompt.ts PromptInput` -> `ModelRef`). We
//!   pass NEITHER by default (the base uses its own configured model) so we can
//!   never send a malformed shape; an explicit provider/model id is honored.
//! - SSE framing: `.../groups/event.ts` plus `.../handlers/event.ts` —
//!   `JSON.stringify({id,type,properties})` per `data:` line; first frame
//!   `server.connected`, 10s `server.heartbeat`.
//! - event semantics (mirrors opencode's OWN consumer,
//!   `packages/opencode/src/cli/cmd/run.ts`):
//!   - `message.part.updated` -> `properties.part`; `part.type=="tool"` carries
//!     `tool` (name), `state.status` (`pending`/`running`/`completed`/`error`),
//!     `state.input` (a record holding the write path), `state.output` /
//!     `state.error`. `part.type=="text"` carries `text`.
//!   - `session.error` -> `properties.error{name,data.message}`.
//!   - `permission.asked` -> `PermissionV1.Request` fields: `id` (`per...`),
//!     `permission`, `patterns`. Reply `once`/`always`/`reject`.
//!   - `question.asked` -> ordered QuestionV1 questions. Answers are arrays of
//!     selected labels/custom text, one array per question.
//!   - **turn done** is `session.status` with `properties.status.type=="idle"`.
//!   - tool-state schema: `packages/core/src/v1/session.ts` (`ToolPart`,
//!     `ToolState{Pending,Running,Completed,Error}`).
//!   - permission Rule/Reply: `packages/core/src/v1/permission.ts`.
//!
//! ## Fail-open by contract
//! Server won't start / SSE drops / an HTTP call errors / the session is busy ->
//! the session surfaces a [`umadev_runtime::TurnStatus::Failed`] (or
//! `next_event` -> `None`),
//! never a panic. A driver bug must never crash the host.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use umadev_runtime::{
    ApprovalDecision, BackgroundTaskSignal, BasePermissionProfile, BaseSession,
    DeliveryReceiptStage, DeliveryReport, HostAnswer, HostApprovalOption, HostApprovalOptionKind,
    HostQuestion, HostQuestionKind, HostQuestionOption, HostRequest, HostResponse, InputDelivery,
    ResumeCapability, SessionCapabilities, SessionError, SessionEvent, SteerSemantics,
    SubagentVisibility, TurnInput, TurnStatus, Usage,
};

use crate::spawn_parts;
use crate::stderr_tail::{StderrDrain, StderrTail};
use crate::{
    isolate_process_tree, kill_isolated_process_tree, kill_isolated_process_tree_blocking,
    reap_isolated_process_tree, END_REAP_BUDGET,
};

/// How many events the SSE-reader task may buffer ahead of the consumer.
const EVENT_CHANNEL_CAP: usize = 256;
const MAX_INPUT_BODY_BYTES: usize = 24 * 1024 * 1024;
const MAX_SSE_LINE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SSE_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_SERVE_STDOUT_LINE_BYTES: usize = 64 * 1024;
const MAX_TRACKED_MESSAGE_ROLES: usize = 512;

/// Correlate OpenCode's session-scoped SSE status edges with the locally sent
/// turn. The protocol does not carry a turn id on `session.status`, so an idle
/// edge is accepted only after this client armed a prompt *and* observed the
/// official `busy`/`retry` edge for that prompt.
type TurnSseGate = Arc<AtomicU8>;
const SSE_TURN_UNARMED: u8 = 0;
const SSE_TURN_ARMED: u8 = 1;
const SSE_TURN_ACTIVE: u8 = 2;

/// How long to wait for `opencode serve` to print its `listening on ...` line
/// before giving up (fail-open: a slow/stuck server start surfaces as a
/// [`SessionError::Start`], not an indefinite hang). Overridable via
/// `UMADEV_OPENCODE_SERVE_TIMEOUT_SECS` for slow machines / CI.
fn serve_start_timeout() -> Duration {
    std::env::var("UMADEV_OPENCODE_SERVE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or_else(|| Duration::from_secs(30), Duration::from_secs)
}

/// A live, long-lived `opencode serve` session: a resident HTTP/SSE server
/// child + one opencode session id, driven over reqwest.
pub struct OpenCodeSession {
    /// The `opencode serve` child process (killed on drop / `end`). Behind a
    /// [`std::sync::Mutex`] so the `&self` [`BaseSession::try_exit_status`] can
    /// do a non-blocking `try_wait()` peek without forcing the trait method to
    /// take `&mut self`; `kill_on_drop(true)` still fires on drop.
    child: std::sync::Mutex<Child>,
    /// Windows kill-on-close Job Object holding the `opencode serve` tree so a
    /// native server grandchild an npm trampoline execs is torn down on teardown
    /// instead of orphaning once the trampoline exits (`taskkill` walks only the
    /// live parent chain). Terminated in `end`/`Drop`. `None` off Windows / when
    /// the OS refused nested job assignment (the process group is the fallback).
    #[cfg(windows)]
    process_job: Option<umadev_process::KillOnCloseJob>,
    /// Bounded tail of the server child's STDERR, captured by the drain task,
    /// surfaced via [`BaseSession::stderr_tail`] to explain *why* a base went
    /// idle (a bad model / not logged in / a config error opencode prints to
    /// stderr before falling silent).
    stderr: StderrTail,
    stderr_drain: StderrDrain,
    /// HTTP transport (base url + auth header baked into each call).
    http: HttpCtx,
    /// The opencode session id (`ses_...`) created at `start`.
    session_id: String,
    /// Explicit provider/model selected for the writer session, when one was
    /// supplied by UmaDev. Read-only forks reuse it; otherwise they omit the
    /// field and inherit OpenCode's configured default model.
    model: Option<String>,
    /// SSE -> normalized event channel, fed by the background reader task.
    events: mpsc::Receiver<SessionEvent>,
    /// Own the long-lived SSE pump so `end`/`Drop` can stop it deterministically.
    sse_task: Option<tokio::task::JoinHandle<()>>,
    /// Writer sessions are persistent OpenCode conversations. Stopping UmaDev
    /// must never delete their transcript; only explicitly ephemeral critic
    /// sessions use the destructive cleanup policy.
    lifecycle: SessionLifecycle,
    /// Request kind and question ordering retained until a typed host reply is
    /// encoded back onto OpenCode's permission/question endpoint.
    pending_interactions: HashMap<String, PendingInteraction>,
    /// Armed immediately before a successful prompt can produce events. The SSE
    /// pump consumes it exactly once at the terminal idle/error edge, preventing
    /// startup or duplicated idle frames from completing a later turn.
    turn_sse_gate: TurnSseGate,
    /// `true` once a turn directive is in flight and not yet idle. The runner
    /// owns serial discipline (it sends the next directive only after the prior
    /// turn's idle `TurnDone`); this mirrors that state so a caller can cheaply
    /// assert "no turn in flight" via [`OpenCodeSession::is_turn_active`] before
    /// re-driving — sending a second prompt while busy is an opencode
    /// `SessionBusyError`.
    turn_active: bool,
}

/// Whether closing a local `opencode serve` attachment also destroys the
/// OpenCode conversation. Main writer/resumed sessions are always persistent;
/// isolated read-only critic forks are intentionally ephemeral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLifecycle {
    Persistent,
    Ephemeral,
}

impl SessionLifecycle {
    const fn deletes_session(self) -> bool {
        matches!(self, Self::Ephemeral)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingInteraction {
    Permission,
    Question { question_ids: Vec<String> },
}

fn remember_interaction(pending: &mut HashMap<String, PendingInteraction>, event: &SessionEvent) {
    let SessionEvent::HostRequest { req_id, request } = event else {
        return;
    };
    let interaction = match request {
        HostRequest::Approval { .. } => PendingInteraction::Permission,
        HostRequest::UserInput { questions, .. } => PendingInteraction::Question {
            question_ids: questions.iter().map(|q| q.id.clone()).collect(),
        },
        _ => return,
    };
    pending.insert(req_id.clone(), interaction);
}

fn ordered_question_answers(
    question_ids: &[String],
    answers: Vec<HostAnswer>,
) -> Option<Vec<Vec<String>>> {
    let mut by_id = HashMap::with_capacity(answers.len());
    for answer in answers {
        if by_id.insert(answer.question_id, answer.values).is_some() {
            return None;
        }
    }
    if by_id.len() != question_ids.len() {
        return None;
    }
    question_ids
        .iter()
        .map(|id| by_id.remove(id))
        .collect::<Option<Vec<_>>>()
}

fn permission_reply_for(response: HostResponse) -> &'static str {
    let HostResponse::Approval {
        decision,
        selected_option_id,
        ..
    } = response
    else {
        return "reject";
    };
    match (decision, selected_option_id.as_deref()) {
        (ApprovalDecision::Allow, None | Some("once")) => "once",
        (ApprovalDecision::Allow, Some("always")) => "always",
        // A response whose binary decision conflicts with the selected vendor
        // option, or carries an unknown option id, must never gain authority.
        _ => "reject",
    }
}

async fn respond_to_interaction(
    http: &HttpCtx,
    req_id: &str,
    interaction: PendingInteraction,
    response: HostResponse,
) -> Result<(), SessionError> {
    match interaction {
        PendingInteraction::Permission => http
            .permission_reply(req_id, permission_reply_for(response))
            .await
            .map_err(SessionError::Send),
        PendingInteraction::Question { question_ids } => match response {
            HostResponse::UserInput { answers } => {
                let Some(answers) = ordered_question_answers(&question_ids, answers) else {
                    return http
                        .question_reject(req_id)
                        .await
                        .map_err(SessionError::Send);
                };
                http.question_reply(req_id, &answers)
                    .await
                    .map_err(SessionError::Send)
            }
            _ => http
                .question_reject(req_id)
                .await
                .map_err(SessionError::Send),
        },
    }
}

/// Default per-request timeout for the non-streaming JSON calls (create / prompt
/// / abort / delete / permission-reply). Without it a local `opencode serve`
/// that accepts the connection but never responds would hang start / send /
/// interrupt / end FOREVER (the shared no-timeout client exists only for the
/// long-lived SSE GET). Fail-open: a timeout surfaces as a clean `Err`, never a
/// hang.
const JSON_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// No session may accept its first prompt until the server has accepted the SSE
/// subscription. Keep that handshake bounded independently from the
/// intentionally timeout-free long-lived event stream.
const SSE_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// OpenCode's parent `idle` can arrive while task-tool child sessions are still
/// busy. Keep the reconciliation bounded so an unhealthy local server can never
/// wedge the host indefinitely.
const CHILD_SETTLE_TIMEOUT: Duration = Duration::from_secs(300);
const CHILD_SETTLE_POLL: Duration = Duration::from_millis(250);
const CHILD_SETTLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const CHILD_SETTLE_MAX_ERRORS: usize = 3;
const CHILD_TREE_MAX_NODES: usize = 64;
const CHILD_TREE_MAX_DEPTH: usize = 8;

#[derive(Clone, Copy)]
struct ChildSettleConfig {
    timeout: Duration,
    poll: Duration,
    request_timeout: Duration,
    max_errors: usize,
}

impl Default for ChildSettleConfig {
    fn default() -> Self {
        Self {
            timeout: CHILD_SETTLE_TIMEOUT,
            poll: CHILD_SETTLE_POLL,
            request_timeout: CHILD_SETTLE_REQUEST_TIMEOUT,
            max_errors: CHILD_SETTLE_MAX_ERRORS,
        }
    }
}

/// State shared across all turns on one OpenCode session. Child sessions remain
/// listed after they finish, so remembering them avoids replaying old lifecycle
/// edges on every later parent turn.
#[derive(Default)]
struct ChildLifecycle {
    known: BTreeSet<String>,
    live: BTreeSet<String>,
    last_level: BTreeSet<String>,
}

impl ChildLifecycle {
    /// Prefer the project-wide SSE stream when it exposes child lineage/status;
    /// the HTTP reconciliation at parent-idle remains the loss-recovery path.
    fn observe_sse(&mut self, payload: &str, parent_id: &str) -> Vec<SessionEvent> {
        let Ok(frame) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        let props = frame.get("properties").unwrap_or(&Value::Null);
        let mut children = self.known.clone();
        let mut statuses = self
            .live
            .iter()
            .cloned()
            .map(|id| (id, "busy".to_string()))
            .collect::<BTreeMap<_, _>>();
        match kind {
            "session.created" | "session.updated" => {
                let Some(info) = props.get("info") else {
                    return Vec::new();
                };
                let Some(id) = info.get("id").and_then(Value::as_str) else {
                    return Vec::new();
                };
                let Some(parent) = info.get("parentID").and_then(Value::as_str) else {
                    return Vec::new();
                };
                if parent != parent_id && !self.known.contains(parent) {
                    return Vec::new();
                }
                children.insert(id.to_string());
                statuses.insert(id.to_string(), "busy".to_string());
            }
            "session.status" => {
                let Some(id) = props.get("sessionID").and_then(Value::as_str) else {
                    return Vec::new();
                };
                if !self.known.contains(id) {
                    return Vec::new();
                }
                match props
                    .get("status")
                    .and_then(|status| status.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("busy" | "retry") => {
                        statuses.insert(id.to_string(), "busy".to_string());
                    }
                    Some("idle") => {
                        statuses.remove(id);
                    }
                    _ => return Vec::new(),
                }
            }
            "session.error" => {
                let Some(id) = props.get("sessionID").and_then(Value::as_str) else {
                    return Vec::new();
                };
                if !self.known.contains(id) {
                    return Vec::new();
                }
                statuses.remove(id);
            }
            "permission.asked" => {
                let Some(id) = props.get("sessionID").and_then(Value::as_str) else {
                    return Vec::new();
                };
                return if self.known.contains(id) {
                    translate_permission(props, id)
                } else {
                    Vec::new()
                };
            }
            "question.asked" => {
                let Some(id) = props.get("sessionID").and_then(Value::as_str) else {
                    return Vec::new();
                };
                return if self.known.contains(id) {
                    translate_question(props, id)
                } else {
                    Vec::new()
                };
            }
            _ => return Vec::new(),
        }
        self.apply_snapshot(children, &statuses)
    }

    fn apply_snapshot(
        &mut self,
        children: BTreeSet<String>,
        statuses: &BTreeMap<String, String>,
    ) -> Vec<SessionEvent> {
        let live = children
            .iter()
            .filter(|id| {
                matches!(
                    statuses.get(*id).map(String::as_str),
                    Some("busy" | "retry")
                )
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut out = Vec::new();

        for id in children.difference(&self.known) {
            out.push(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started { id: id.clone() },
            ));
        }
        // A child can finish before the first parent-idle reconciliation. Close
        // that observed lifecycle immediately instead of leaving a stale task.
        for id in children.difference(&self.known) {
            if !live.contains(id) {
                out.push(SessionEvent::BackgroundTask(
                    BackgroundTaskSignal::Finished { id: id.clone() },
                ));
            }
        }
        for id in self.live.difference(&live) {
            out.push(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Finished { id: id.clone() },
            ));
        }
        for id in live.difference(&self.live) {
            if self.known.contains(id) {
                out.push(SessionEvent::BackgroundTask(
                    BackgroundTaskSignal::Started { id: id.clone() },
                ));
            }
        }
        if live != self.last_level {
            out.push(SessionEvent::BackgroundTask(BackgroundTaskSignal::Live {
                agent_ids: live.iter().cloned().collect(),
            }));
            self.last_level.clone_from(&live);
        }
        self.known.extend(children);
        self.live = live;
        out
    }

    fn fail_open_events(&mut self) -> Vec<SessionEvent> {
        let mut out = self
            .live
            .iter()
            .cloned()
            .map(|id| SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished { id }))
            .collect::<Vec<_>>();
        if !self.last_level.is_empty() {
            out.push(SessionEvent::BackgroundTask(BackgroundTaskSignal::Live {
                agent_ids: Vec::new(),
            }));
        }
        self.live.clear();
        self.last_level.clear();
        out
    }
}

fn parent_turn_is_active(payload: &str, parent_id: &str) -> bool {
    let Ok(frame) = serde_json::from_str::<Value>(payload) else {
        return false;
    };
    let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let props = frame.get("properties").unwrap_or(&Value::Null);
    match kind {
        "session.status" => {
            props.get("sessionID").and_then(Value::as_str) == Some(parent_id)
                && matches!(
                    props
                        .get("status")
                        .and_then(|status| status.get("type"))
                        .and_then(Value::as_str),
                    Some("busy" | "retry")
                )
        }
        _ => false,
    }
}

/// The HTTP context shared by every call: base url, auth header, project dir.
#[derive(Clone)]
struct HttpCtx {
    /// The no-timeout client — used ONLY for the long-lived SSE `/event` GET (a
    /// per-request timeout would sever the event stream). Never used for the
    /// short JSON calls.
    client: reqwest::Client,
    /// A SEPARATE client carrying [`JSON_REQUEST_TIMEOUT`], used for every
    /// non-streaming JSON request so a wedged server can't hang the session.
    json_client: reqwest::Client,
    /// e.g. `http://127.0.0.1:54321`.
    base_url: String,
    /// `Basic base64("opencode:<password>")`.
    auth: String,
    /// The percent-encoded absolute project path for `x-opencode-directory` and
    /// the `?directory=` query the SSE stream filters on.
    directory: String,
}

/// The pieces of a freshly spawned `opencode serve` process, kept together so the
/// caller can move the child into the session while a Windows kill-on-close Job
/// Object — attached BEFORE the trampoline execs its native grandchild — rides
/// along into the same struct.
struct SpawnedServe {
    child: Child,
    /// Windows kill-on-close Job Object over the whole `serve` tree; `None` off
    /// Windows / when the OS refused nested job assignment.
    #[cfg(windows)]
    process_job: Option<umadev_process::KillOnCloseJob>,
    stderr: StderrTail,
    stderr_drain: StderrDrain,
    http: HttpCtx,
}

async fn spawn_serve(
    program: &str,
    workspace: &Path,
    serve_timeout: Duration,
) -> Result<SpawnedServe, SessionError> {
    let password = random_password();
    let (prog, lead) = spawn_parts(program);
    let mut cmd = Command::new(prog);
    cmd.args(&lead);
    cmd.args(serve_args());
    cmd.current_dir(workspace);
    cmd.env("OPENCODE_SERVER_PASSWORD", &password);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    // Own process group so teardown can signal the WHOLE tree: an npm-installed
    // `opencode` can front the real server through a Node trampoline, and a
    // direct-child kill would reparent the actual `opencode serve` to init and
    // leak it. See [`isolate_process_tree`] / [`kill_isolated_process_tree`].
    isolate_process_tree(&mut cmd);

    let mut child = crate::spawn_retrying_etxtbsy(&mut cmd)
        .map_err(|e| SessionError::Start(spawn_err(program, &e)))?;
    // Windows: attach the kill-on-close Job Object BEFORE reading the listening URL.
    // The trampoline may already have exec'd the native `opencode serve` grandchild
    // by the time it prints the URL, and a Job only captures descendants spawned
    // AFTER assignment — so a late attach would miss the grandchild that `taskkill`
    // can no longer reach once the trampoline exits. `None` off Windows / if the OS
    // refuses nested assignment (the process group is the fallback).
    #[cfg(windows)]
    let process_job = umadev_process::KillOnCloseJob::attach(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SessionError::Start("opencode serve: no stdout pipe".to_string()))?;
    let stderr = StderrTail::new();
    let stderr_drain = child.stderr.take().map_or_else(StderrDrain::empty, |pipe| {
        StderrDrain::spawn(pipe, stderr.clone())
    });
    let base_url = match read_listening_url(stdout, serve_timeout).await {
        Ok(url) => url,
        Err(error) => {
            // `serve` start timed out / failed: kill the whole tree, not just the
            // trampoline, so no native `opencode serve` grandchild is orphaned.
            // Windows: terminate the Job Object (reaches a grandchild taskkill can't).
            #[cfg(windows)]
            if let Some(job) = &process_job {
                job.terminate();
            }
            kill_isolated_process_tree(&mut child);
            return Err(SessionError::Start(error));
        }
    };
    let http = HttpCtx::new(base_url, &password, workspace);
    Ok(SpawnedServe {
        child,
        #[cfg(windows)]
        process_job,
        stderr,
        stderr_drain,
        http,
    })
}

impl OpenCodeSession {
    /// Start a session driving the default `opencode` binary
    /// (`UMADEV_OPENCODE_BIN` override honored), serving in `workspace`.
    ///
    /// `agent` selects the opencode agent (e.g. `build`); `None` lets the base
    /// pick its default. `model` is an opencode provider/model id
    /// (`provider/model`); `None` (the default) uses whatever model the base is
    /// already configured with — UmaDev injects no model endpoint of its own.
    ///
    /// The permission profile selects read-only Plan, approval-gated Guarded, or
    /// pre-authorized Auto rules while governance keeps observing the event stream.
    pub async fn start(
        workspace: &Path,
        agent: Option<&str>,
        model: Option<&str>,
        permissions: BasePermissionProfile,
    ) -> Result<Self, SessionError> {
        let program =
            std::env::var("UMADEV_OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
        Self::start_with_program_timeout_profile(
            &program,
            workspace,
            agent,
            model,
            permissions,
            serve_start_timeout(),
        )
        .await
        .map_err(crate::redaction::sanitize_session_error)
    }

    /// Start a session against an explicit `program` (mainly for tests, where
    /// `program` is a fake `opencode serve` that prints a `listening on ...`
    /// line pointing at a fake HTTP server). Uses the env-configured
    /// serve-start timeout.
    pub async fn start_with_program(
        program: &str,
        workspace: &Path,
        agent: Option<&str>,
        model: Option<&str>,
        autonomous: bool,
    ) -> Result<Self, SessionError> {
        Self::start_with_program_timeout(
            program,
            workspace,
            agent,
            model,
            autonomous,
            serve_start_timeout(),
        )
        .await
        .map_err(crate::redaction::sanitize_session_error)
    }

    /// Start with an explicit serve-start `timeout` — the testable core, so a
    /// test passes its own bound instead of mutating a process-global env var
    /// (which would race other parallel tests).
    pub async fn start_with_program_timeout(
        program: &str,
        workspace: &Path,
        agent: Option<&str>,
        model: Option<&str>,
        autonomous: bool,
        serve_timeout: Duration,
    ) -> Result<Self, SessionError> {
        let permissions = if autonomous {
            BasePermissionProfile::Auto
        } else {
            BasePermissionProfile::Guarded
        };
        Self::start_with_program_timeout_profile(
            program,
            workspace,
            agent,
            model,
            permissions,
            serve_timeout,
        )
        .await
        .map_err(crate::redaction::sanitize_session_error)
    }

    async fn start_with_program_timeout_profile(
        program: &str,
        workspace: &Path,
        agent: Option<&str>,
        model: Option<&str>,
        permissions: BasePermissionProfile,
        serve_timeout: Duration,
    ) -> Result<Self, SessionError> {
        // Do not rely on the startup picker having run: configured users and
        // programmatic callers can open a session directly. The read-only Plan
        // floor therefore runs here too, immediately before `serve`, scoped to
        // the posture: a read-only Plan session on a sub-minimum OpenCode is
        // refused (a Task subagent there can escape Plan's read-only rules and
        // write files), while Guarded/Auto — already permitted to write — open
        // on ANY installed version.
        crate::opencode::ensure_opencode_version_permits(program, workspace, permissions)
            .await
            .map_err(SessionError::Start)?;
        let spawned = spawn_serve(program, workspace, serve_timeout).await?;
        let mut child = spawned.child;
        // Held for the session lifetime on Windows; drops (killing any surviving
        // tree member) if an early error return below skips `Self` construction.
        #[cfg(windows)]
        let process_job = spawned.process_job;
        let stderr_tail = spawned.stderr;
        let stderr_drain = spawned.stderr_drain;
        let http = spawned.http;

        // Open the one session for the whole run. The ruleset follows the permission
        // profile: Auto is wildcard-allow, Guarded is ask-by-default with a narrow
        // read-only allowlist, and Plan is deny-by-default. This keeps opencode's
        // gate posture aligned with codex / claude without silently authorizing a
        // future tool or an MCP-provided mutator.
        let session_id = match http.create_session_profile(agent, model, permissions).await {
            Ok(id) => id,
            Err(e) => {
                kill_isolated_process_tree(&mut child);
                return Err(SessionError::Start(format!("create session: {e}")));
            }
        };

        let (rx, sse_task, turn_sse_gate) = match http.attach_sse(&session_id).await {
            Ok(attached) => attached,
            Err(error) => {
                kill_isolated_process_tree(&mut child);
                return Err(SessionError::Start(error));
            }
        };

        Ok(Self {
            child: std::sync::Mutex::new(child),
            #[cfg(windows)]
            process_job,
            stderr: stderr_tail,
            stderr_drain,
            http,
            session_id,
            model: model.map(str::to_owned),
            events: rx,
            sse_task: Some(sse_task),
            lifecycle: SessionLifecycle::Persistent,
            pending_interactions: HashMap::new(),
            turn_sse_gate,
            turn_active: false,
        })
    }

    /// Re-attach to a persisted OpenCode conversation on a newly spawned
    /// `opencode serve` process. OpenCode stores sessions outside the resident
    /// HTTP process, so cross-process resume is a GET + permission refresh +
    /// fresh SSE subscription, not a new `POST /session`.
    pub async fn resume(
        workspace: &Path,
        model: Option<&str>,
        session_id: &str,
        permissions: BasePermissionProfile,
    ) -> Result<Self, SessionError> {
        let program =
            std::env::var("UMADEV_OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
        Self::resume_with_program_timeout(
            &program,
            workspace,
            model,
            session_id,
            permissions,
            serve_start_timeout(),
        )
        .await
        .map_err(crate::redaction::sanitize_session_error)
    }

    /// Testable resume constructor with an explicit serve program and timeout.
    pub async fn resume_with_program_timeout(
        program: &str,
        workspace: &Path,
        model: Option<&str>,
        session_id: &str,
        permissions: BasePermissionProfile,
        serve_timeout: Duration,
    ) -> Result<Self, SessionError> {
        let session_id = session_id.trim();
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(SessionError::Start(
                "opencode resume requires a valid session id".to_string(),
            ));
        }
        // Same posture-scoped floor as a fresh start: a read-only Plan resume on
        // a sub-minimum version is refused; Guarded/Auto resume on any installed
        // version.
        crate::opencode::ensure_opencode_version_permits(program, workspace, permissions)
            .await
            .map_err(SessionError::Start)
            .map_err(crate::redaction::sanitize_session_error)?;

        let spawned = spawn_serve(program, workspace, serve_timeout)
            .await
            .map_err(crate::redaction::sanitize_session_error)?;
        let mut child = spawned.child;
        // Held for the session lifetime on Windows; drops (killing any surviving
        // tree member) if an early error return below skips `Self` construction.
        #[cfg(windows)]
        let process_job = spawned.process_job;
        let stderr = spawned.stderr;
        let stderr_drain = spawned.stderr_drain;
        let http = spawned.http;

        if let Err(error) = http.get_session(session_id).await {
            kill_isolated_process_tree(&mut child);
            return Err(crate::redaction::sanitize_session_error(
                SessionError::Start(format!("resume opencode session `{session_id}`: {error}")),
            ));
        }
        // Always replace the persisted ruleset before attaching the stream.
        // This is the fail-closed boundary that prevents a prior Auto session's
        // wildcard allow from surviving a Guarded/Plan resume.
        if let Err(error) = http.apply_permissions(session_id, permissions).await {
            kill_isolated_process_tree(&mut child);
            return Err(crate::redaction::sanitize_session_error(
                SessionError::Start(format!(
                    "refresh opencode session permissions `{session_id}`: {error}"
                )),
            ));
        }

        let (rx, sse_task, turn_sse_gate) = match http.attach_sse(session_id).await {
            Ok(attached) => attached,
            Err(error) => {
                kill_isolated_process_tree(&mut child);
                return Err(crate::redaction::sanitize_session_error(
                    SessionError::Start(error),
                ));
            }
        };
        Ok(Self {
            child: std::sync::Mutex::new(child),
            #[cfg(windows)]
            process_job,
            stderr,
            stderr_drain,
            http,
            session_id: session_id.to_string(),
            model: model.map(str::to_owned),
            events: rx,
            sse_task: Some(sse_task),
            lifecycle: SessionLifecycle::Persistent,
            pending_interactions: HashMap::new(),
            turn_sse_gate,
            turn_active: false,
        })
    }

    async fn stop_sse(&mut self) {
        if let Some(task) = self.sse_task.take() {
            task.abort();
            let _ = task.await;
        }
    }

    /// The opencode session id this session drives. Exposed for diagnostics.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Whether a turn is in flight (a directive was sent and no idle `TurnDone`
    /// has been observed yet). The runner serializes turns off this so a second
    /// `send_turn` never races into an opencode `SessionBusyError`.
    #[must_use]
    pub fn is_turn_active(&self) -> bool {
        self.turn_active
    }
}

impl Drop for OpenCodeSession {
    fn drop(&mut self) {
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }
        // A dropped session (timed-out / cancelled turn, or normal teardown that
        // skipped `end()`) must kill the whole process GROUP: killing only the
        // direct child can leave a native `opencode serve` grandchild orphaned.
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

fn opencode_parts(
    input: &crate::turn_input::PreparedTurnInput,
) -> Result<(Vec<Value>, Vec<InputDelivery>), SessionError> {
    let mut parts = Vec::with_capacity(input.blocks.len());
    let mut deliveries = Vec::with_capacity(input.blocks.len());
    for (index, block) in input.blocks.iter().enumerate() {
        match block {
            crate::turn_input::PreparedBlock::Text(text) => {
                parts.push(json!({"type":"text", "text":text}));
            }
            crate::turn_input::PreparedBlock::Image(attachment)
            | crate::turn_input::PreparedBlock::File { attachment, .. } => {
                let uri =
                    crate::turn_input::file_uri(&attachment.canonical_path, index, block.kind())?;
                let filename = attachment
                    .canonical_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("attachment");
                parts.push(json!({
                    "type":"file",
                    "mime":attachment.media_type,
                    "url":uri,
                    "filename":filename
                }));
            }
        }
        deliveries.push(InputDelivery::Native);
    }
    Ok((parts, deliveries))
}

fn acknowledged_delivery_report(
    prepared: &crate::turn_input::PreparedTurnInput,
    deliveries: &[InputDelivery],
    encoded_bytes: usize,
) -> DeliveryReport {
    let mut report = prepared.report(deliveries, encoded_bytes);
    // The documented prompt_async endpoint has returned a successful HTTP
    // response for this exact request. This proves server acceptance, not that
    // the model has started or completed processing it.
    report.receipt = DeliveryReceiptStage::ProtocolAcknowledged;
    report
}

#[async_trait]
impl BaseSession for OpenCodeSession {
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            mid_turn_steer: false,
            set_model: false,
            set_mode: false,
            set_thinking: false,
            text_input: InputDelivery::Native,
            image_input: InputDelivery::Native,
            file_input: InputDelivery::Native,
            steer: SteerSemantics::Unsupported,
            resume: ResumeCapability::Native,
            subagents: SubagentVisibility::AuthoritativeLiveSet,
            prompt_queue: umadev_runtime::PromptQueueCapability::Unsupported,
            background_process_control:
                umadev_runtime::BackgroundProcessControlCapability::Unsupported,
        }
    }

    async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
        // A read-only critic fork: open a NEW, INDEPENDENT opencode session on
        // the SAME resident server, but with a DENY ruleset so every tool call
        // that would mutate the workspace is rejected (the single-writer
        // invariant — only the main session writes the blackboard). A separate
        // session id means it can never collide with the main writer's in-flight
        // turn. The fork reads the same on-disk blackboard the main line wrote.
        //
        // Fail-open: a `create_session` failure surfaces as `Start`, which the
        // caller treats exactly like `ForkUnsupported` (degrade, never block).
        self.http
            .start_readonly_fork(self.model.as_deref())
            .await
            .map(|fork| Box::new(fork) as Box<dyn BaseSession>)
            .map_err(SessionError::Start)
            .map_err(crate::redaction::sanitize_session_error)
    }

    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        // `prompt_async` returns immediately (202/NoContent) and the same
        // session retains context. Serial discipline: the runner only sends the
        // next directive after observing the previous turn's idle TurnDone, so
        // we never hit a `SessionBusyError` here. Fail-open: an HTTP error is a
        // Send error the runner can surface as a failed turn.
        if self
            .sse_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return Err(SessionError::Send(
                "opencode event stream is not running".to_string(),
            ));
        }
        self.turn_sse_gate.store(SSE_TURN_ARMED, Ordering::Release);
        self.turn_active = true;
        let res = self
            .http
            .prompt_async(&self.session_id, &directive, self.model.as_deref())
            .await
            .map_err(SessionError::Send);
        if res.is_err() {
            // The turn never actually started (the async prompt POST failed):
            // clear the flag so the state machine stays honest and a later
            // `is_turn_active` / re-drive isn't blocked by a phantom turn.
            self.turn_active = false;
            self.turn_sse_gate
                .store(SSE_TURN_UNARMED, Ordering::Release);
        }
        res.map_err(crate::redaction::sanitize_session_error)
    }

    async fn send_input(&mut self, input: TurnInput) -> Result<DeliveryReport, SessionError> {
        crate::turn_input::ensure_supported(&input, self.capabilities())?;
        let prepared = crate::turn_input::prepare(input).await?;
        let (parts, deliveries) = opencode_parts(&prepared)?;
        if self
            .sse_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return Err(SessionError::Send(
                "opencode event stream is not running".to_string(),
            ));
        }
        self.turn_sse_gate.store(SSE_TURN_ARMED, Ordering::Release);
        self.turn_active = true;
        let result = self
            .http
            .prompt_async_parts(&self.session_id, parts, self.model.as_deref())
            .await
            .map_err(SessionError::Send);
        if result.is_err() {
            self.turn_active = false;
            self.turn_sse_gate
                .store(SSE_TURN_UNARMED, Ordering::Release);
        }
        let encoded_bytes = result.map_err(crate::redaction::sanitize_session_error)?;
        Ok(acknowledged_delivery_report(
            &prepared,
            &deliveries,
            encoded_bytes,
        ))
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        // No internal timeout BY DESIGN — the runner owns phase/run budgets and
        // races this against them (then calls `interrupt`). Keep the session a
        // pure relay so a synthetic TurnDone never races a real `idle`.
        let ev = self
            .events
            .recv()
            .await
            .map(crate::redaction::sanitize_session_event);
        if let Some(event) = ev.as_ref() {
            remember_interaction(&mut self.pending_interactions, event);
        }
        if matches!(ev, Some(SessionEvent::TurnDone { .. }) | None) {
            self.turn_active = false;
        }
        ev
    }

    async fn respond(
        &mut self,
        req_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError> {
        let interaction = self.pending_interactions.remove(req_id);
        // opencode permission reply vocabulary is `once`/`always`/`reject`
        // (`PermissionV1.Reply`). Allow -> `once` (grant just this call); Deny ->
        // `reject`. We never auto-`always` — escalation stays the runner's call.
        let reply = match decision {
            ApprovalDecision::Allow => "once",
            ApprovalDecision::Deny => "reject",
        };
        let result = if matches!(
            interaction.as_ref(),
            Some(PendingInteraction::Question { .. })
        ) {
            self.http.question_reject(req_id).await
        } else {
            self.http.permission_reply(req_id, reply).await
        }
        .map_err(SessionError::Send);
        if result.is_err() {
            if let Some(interaction) = interaction {
                self.pending_interactions
                    .insert(req_id.to_string(), interaction);
            }
        }
        result.map_err(crate::redaction::sanitize_session_error)
    }

    async fn respond_host(
        &mut self,
        req_id: &str,
        response: HostResponse,
    ) -> Result<(), SessionError> {
        let Some(interaction) = self.pending_interactions.remove(req_id) else {
            return Err(SessionError::Send(format!(
                "opencode host response has no pending request `{req_id}`"
            )));
        };
        let retry = interaction.clone();
        let result = respond_to_interaction(&self.http, req_id, interaction, response).await;
        if result.is_err() {
            self.pending_interactions.insert(req_id.to_string(), retry);
        }
        result.map_err(crate::redaction::sanitize_session_error)
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        self.turn_active = false;
        self.turn_sse_gate
            .store(SSE_TURN_UNARMED, Ordering::Release);
        self.http
            .abort(&self.session_id)
            .await
            .map_err(SessionError::Send)
            .map_err(crate::redaction::sanitize_session_error)
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        // A writer conversation is durable OpenCode state. Closing UmaDev only
        // detaches its SSE stream and stops the resident HTTP process; deleting
        // here would make cross-process `/continue` impossible.
        self.stop_sse().await;
        let _ = self
            .http
            .finish_session(&self.session_id, self.lifecycle)
            .await;
        self.pending_interactions.clear();
        // Windows: terminate the Job Object first so the native `opencode serve`
        // grandchild dies even after the trampoline has exited (taskkill can no
        // longer reach it); the group reap below is the cross-platform backstop.
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

    fn session_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    fn try_exit_status(&self) -> Option<std::process::ExitStatus> {
        // Non-blocking peek at the `opencode serve` child (lock + try_wait both
        // never block); a contended lock / try_wait error fails open to None.
        self.child.try_lock().ok()?.try_wait().ok().flatten()
    }
}

/// A READ-ONLY critic fork of an [`OpenCodeSession`]: a SEPARATE opencode
/// session (created with a deny ruleset) on the SAME resident `opencode serve`.
///
/// Unlike the main session it does NOT own the server child — the parent owns
/// the `opencode serve` process lifetime — so `end()` only deletes its own
/// session id and never kills the shared server. A critic seat drives it like
/// any [`BaseSession`]: one strict-JSON judge directive, drain events for the
/// verdict text, end. Its deny ruleset + its own session id keep it read-only
/// and collision-free with the main writer (the single-writer invariant).
pub struct OpenCodeForkSession {
    http: HttpCtx,
    session_id: String,
    events: mpsc::Receiver<SessionEvent>,
    /// The fork owns its SSE subscription. Keeping the handle makes `end` and
    /// `Drop` deterministic instead of leaking one pump per intent/review turn.
    sse_task: Option<tokio::task::JoinHandle<()>>,
    lifecycle: SessionLifecycle,
    pending_interactions: HashMap<String, PendingInteraction>,
    turn_sse_gate: TurnSseGate,
    turn_active: bool,
}

impl OpenCodeForkSession {
    async fn stop_sse(&mut self) {
        if let Some(task) = self.sse_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for OpenCodeForkSession {
    fn drop(&mut self) {
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }
    }
}

#[async_trait]
impl BaseSession for OpenCodeForkSession {
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            mid_turn_steer: false,
            set_model: false,
            set_mode: false,
            set_thinking: false,
            text_input: InputDelivery::Native,
            image_input: InputDelivery::Native,
            file_input: InputDelivery::Native,
            steer: SteerSemantics::Unsupported,
            resume: ResumeCapability::Unsupported,
            subagents: SubagentVisibility::AuthoritativeLiveSet,
            prompt_queue: umadev_runtime::PromptQueueCapability::Unsupported,
            background_process_control:
                umadev_runtime::BackgroundProcessControlCapability::Unsupported,
        }
    }

    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        if self
            .sse_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return Err(SessionError::Send(
                "opencode fork event stream is not running".to_string(),
            ));
        }
        self.turn_sse_gate.store(SSE_TURN_ARMED, Ordering::Release);
        self.turn_active = true;
        let res = self
            .http
            .prompt_async(&self.session_id, &directive, None)
            .await
            .map_err(SessionError::Send);
        if res.is_err() {
            // Reset on a failed send so the fork's state machine stays honest.
            self.turn_active = false;
            self.turn_sse_gate
                .store(SSE_TURN_UNARMED, Ordering::Release);
        }
        res.map_err(crate::redaction::sanitize_session_error)
    }

    async fn send_input(&mut self, input: TurnInput) -> Result<DeliveryReport, SessionError> {
        crate::turn_input::ensure_supported(&input, self.capabilities())?;
        let prepared = crate::turn_input::prepare(input).await?;
        let (parts, deliveries) = opencode_parts(&prepared)?;
        if self
            .sse_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return Err(SessionError::Send(
                "opencode fork event stream is not running".to_string(),
            ));
        }
        self.turn_sse_gate.store(SSE_TURN_ARMED, Ordering::Release);
        self.turn_active = true;
        let result = self
            .http
            .prompt_async_parts(&self.session_id, parts, None)
            .await
            .map_err(SessionError::Send);
        if result.is_err() {
            self.turn_active = false;
            self.turn_sse_gate
                .store(SSE_TURN_UNARMED, Ordering::Release);
        }
        let encoded_bytes = result.map_err(crate::redaction::sanitize_session_error)?;
        Ok(acknowledged_delivery_report(
            &prepared,
            &deliveries,
            encoded_bytes,
        ))
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        let ev = self
            .events
            .recv()
            .await
            .map(crate::redaction::sanitize_session_event);
        if let Some(event) = ev.as_ref() {
            remember_interaction(&mut self.pending_interactions, event);
        }
        if matches!(ev, Some(SessionEvent::TurnDone { .. }) | None) {
            self.turn_active = false;
        }
        ev
    }

    async fn respond(
        &mut self,
        req_id: &str,
        _decision: ApprovalDecision,
    ) -> Result<(), SessionError> {
        let interaction = self.pending_interactions.remove(req_id);
        // Defense in depth: a read-only fork never turns a permission prompt
        // into authority. This also rejects a future/unknown or MCP tool if a
        // server version asks despite the deny-by-default session ruleset.
        let result = if matches!(
            interaction.as_ref(),
            Some(PendingInteraction::Question { .. })
        ) {
            self.http.question_reject(req_id).await
        } else {
            self.http.permission_reply(req_id, "reject").await
        }
        .map_err(SessionError::Send);
        if result.is_err() {
            if let Some(interaction) = interaction {
                self.pending_interactions
                    .insert(req_id.to_string(), interaction);
            }
        }
        result.map_err(crate::redaction::sanitize_session_error)
    }

    async fn respond_host(
        &mut self,
        req_id: &str,
        _response: HostResponse,
    ) -> Result<(), SessionError> {
        let interaction = self.pending_interactions.remove(req_id);
        let result = match interaction.as_ref() {
            Some(PendingInteraction::Question { .. }) => self
                .http
                .question_reject(req_id)
                .await
                .map_err(SessionError::Send),
            Some(PendingInteraction::Permission) | None => self
                .http
                .permission_reply(req_id, "reject")
                .await
                .map_err(SessionError::Send),
        };
        if result.is_err() {
            if let Some(interaction) = interaction {
                self.pending_interactions
                    .insert(req_id.to_string(), interaction);
            }
        }
        result.map_err(crate::redaction::sanitize_session_error)
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        self.turn_active = false;
        self.turn_sse_gate
            .store(SSE_TURN_UNARMED, Ordering::Release);
        self.http
            .abort(&self.session_id)
            .await
            .map_err(SessionError::Send)
            .map_err(crate::redaction::sanitize_session_error)
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        self.stop_sse().await;
        let _ = self
            .http
            .finish_session(&self.session_id, self.lifecycle)
            .await;
        self.pending_interactions.clear();
        Ok(())
    }
}

impl HttpCtx {
    /// Build the HTTP context. The directory is percent-encoded for the
    /// `x-opencode-directory` header (header values must be ASCII) and reused as
    /// the `?directory=` query the event stream filters on.
    fn new(base_url: String, password: &str, workspace: &Path) -> Self {
        Self::new_with_timeout(base_url, password, workspace, JSON_REQUEST_TIMEOUT)
    }

    /// Build the HTTP context with an explicit JSON-request timeout — the
    /// testable core, so a test can point at a never-responding server with a
    /// short bound and assert the call fails-open instead of hanging (rather than
    /// waiting out the 45s production default).
    fn new_with_timeout(
        base_url: String,
        password: &str,
        workspace: &Path,
        json_timeout: Duration,
    ) -> Self {
        use std::fmt::Write as _;
        // base64 without pulling a crate: opencode auth is
        // `Basic base64("opencode:<password>")` (server/auth.ts).
        let auth = format!("Basic {}", base64_encode(format!("opencode:{password}")));
        // Percent-encode the absolute path for an ASCII-safe header / query.
        let dir = workspace.to_string_lossy();
        let mut encoded = String::with_capacity(dir.len());
        for b in dir.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
                encoded.push(b as char);
            } else {
                let _ = write!(encoded, "%{b:02X}");
            }
        }
        Self {
            // A client with no global request timeout: the SSE stream is a
            // long-lived GET, so a per-call timeout would kill the event stream.
            client: reqwest::Client::builder()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            // A SEPARATE client WITH a request timeout for the short JSON calls
            // (create / prompt / abort / delete / permission-reply) so a wedged
            // server can never hang start / send / interrupt / end.
            json_client: reqwest::Client::builder()
                .timeout(json_timeout)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url,
            auth,
            directory: encoded,
        }
    }

    /// Common headers every authenticated (non-streaming) JSON call carries.
    /// Built on the timeout-bearing [`HttpCtx::json_client`] — NOT the
    /// no-timeout SSE client — so any such call fails open on a wedged server
    /// instead of hanging.
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.json_client
            .request(method, format!("{}{path}", self.base_url))
            .header(reqwest::header::AUTHORIZATION, &self.auth)
            .header("x-opencode-directory", &self.directory)
    }

    /// `POST /session` with a permission ruleset chosen by the autonomy tier
    /// (see [`session_ruleset`]). Returns the created `session.id`. The `model`
    /// here, if any, uses CREATE's shape `{id,providerID,variant?}` (distinct from
    /// prompt's `{providerID,modelID}`).
    #[cfg(test)]
    async fn create_session(
        &self,
        agent: Option<&str>,
        model: Option<&str>,
        autonomous: bool,
    ) -> Result<String, String> {
        let permissions = if autonomous {
            BasePermissionProfile::Auto
        } else {
            BasePermissionProfile::Guarded
        };
        self.create_session_profile(agent, model, permissions).await
    }

    async fn create_session_profile(
        &self,
        agent: Option<&str>,
        model: Option<&str>,
        permissions: BasePermissionProfile,
    ) -> Result<String, String> {
        let mut body = session_permission_payload(permissions);
        if let Some(a) = agent {
            body["agent"] = json!(a);
        }
        if let Some((provider, m)) = model.and_then(split_provider_model) {
            // CREATE model shape: {id, providerID, variant?}.
            body["model"] = json!({ "id": m, "providerID": provider });
        }
        let resp = self
            .req(reqwest::Method::POST, "/session")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("POST /session: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("POST /session: HTTP {}", resp.status()));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| format!("POST /session decode: {e}"))?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "POST /session: response missing `id`".to_string())
    }

    /// Validate that a persisted conversation exists on this newly spawned
    /// server attachment before we expose a resumed session to the runner.
    async fn get_session(&self, session_id: &str) -> Result<Value, String> {
        let resp = self
            .req(reqwest::Method::GET, &format!("/session/{session_id}"))
            .send()
            .await
            .map_err(|e| format!("GET /session/{session_id}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET /session/{session_id}: HTTP {}", resp.status()));
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("GET /session/{session_id} decode: {e}"))?;
        if value.get("id").and_then(Value::as_str) != Some(session_id) {
            return Err(format!(
                "GET /session/{session_id}: response id does not match"
            ));
        }
        Ok(value)
    }

    /// Replace (not merge) the persisted session ruleset before resume. The
    /// official PATCH route accepts `permission` as a complete ruleset.
    async fn apply_permissions(
        &self,
        session_id: &str,
        permissions: BasePermissionProfile,
    ) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::PATCH, &format!("/session/{session_id}"))
            .json(&session_permission_payload(permissions))
            .send()
            .await
            .map_err(|e| format!("PATCH /session/{session_id}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "PATCH /session/{session_id}: HTTP {}",
                resp.status()
            ));
        }
        Ok(())
    }

    /// `POST /session` with a deny-by-default READ-ONLY ruleset for an
    /// intent/review fork. Only the local source-inspection tools are allowed;
    /// `task`, `patch`, shells, unknown future tools and MCP tools therefore all
    /// remain denied by the wildcard floor.
    ///
    /// Reuse an explicitly selected writer model when available. With no
    /// explicit selection, omit both model and agent so OpenCode applies the
    /// user's configured defaults instead of UmaDev hard-coding `build`.
    async fn create_readonly_session(&self, model: Option<&str>) -> Result<String, String> {
        let mut body = json!({
            "permission": readonly_session_ruleset(),
        });
        if let Some((provider, m)) = model.and_then(split_provider_model) {
            body["model"] = json!({ "id": m, "providerID": provider });
        }
        let resp = self
            .req(reqwest::Method::POST, "/session")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("POST /session (fork): {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("POST /session (fork): HTTP {}", resp.status()));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| format!("POST /session (fork) decode: {e}"))?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "POST /session (fork): response missing `id`".to_string())
    }

    async fn attach_sse(
        &self,
        session_id: &str,
    ) -> Result<
        (
            mpsc::Receiver<SessionEvent>,
            tokio::task::JoinHandle<()>,
            TurnSseGate,
        ),
        String,
    > {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let (ready_tx, ready_rx) = oneshot::channel();
        let turn_sse_gate: TurnSseGate = Arc::new(AtomicU8::new(SSE_TURN_UNARMED));
        let task = tokio::spawn(pump_sse_with_ready(
            self.clone(),
            session_id.to_string(),
            tx,
            Some(ready_tx),
            Arc::clone(&turn_sse_gate),
        ));
        match tokio::time::timeout(SSE_READY_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(()))) => Ok((rx, task, turn_sse_gate)),
            Ok(Ok(Err(error))) => {
                task.abort();
                Err(format!("event stream: {error}"))
            }
            Ok(Err(_)) => {
                task.abort();
                Err("event stream ended before ready".to_string())
            }
            Err(_) => {
                task.abort();
                Err(format!(
                    "event stream was not ready within {}s",
                    SSE_READY_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// Create a read-only fork and wait until its independent SSE subscription
    /// is accepted before returning it to the caller. Without this barrier a
    /// fast `prompt_async` can complete before the event stream is listening,
    /// losing the verdict and leaving intent routing to time out.
    async fn start_readonly_fork(
        &self,
        model: Option<&str>,
    ) -> Result<OpenCodeForkSession, String> {
        let session_id = self.create_readonly_session(model).await?;
        match self.attach_sse(&session_id).await {
            Ok((rx, sse_task, turn_sse_gate)) => Ok(OpenCodeForkSession {
                http: self.clone(),
                session_id,
                events: rx,
                sse_task: Some(sse_task),
                lifecycle: SessionLifecycle::Ephemeral,
                pending_interactions: HashMap::new(),
                turn_sse_gate,
                turn_active: false,
            }),
            Err(error) => {
                let _ = self.delete_session(&session_id).await;
                Err(format!("fork event stream: {error}"))
            }
        }
    }

    /// `POST /session/:id/prompt_async` — inject a phase directive. Returns
    /// immediately (NoContent); the work streams over SSE.
    async fn prompt_async(
        &self,
        session_id: &str,
        directive: &str,
        model: Option<&str>,
    ) -> Result<(), String> {
        self.prompt_async_parts(
            session_id,
            vec![json!({"type":"text", "text":directive})],
            model,
        )
        .await
        .map(|_| ())
    }

    async fn prompt_async_parts(
        &self,
        session_id: &str,
        parts: Vec<Value>,
        model: Option<&str>,
    ) -> Result<usize, String> {
        let mut body = json!({"parts":parts});
        if let Some((provider, model)) = model.and_then(split_provider_model) {
            body["model"] = json!({ "providerID": provider, "modelID": model });
        }
        let encoded =
            serde_json::to_vec(&body).map_err(|error| format!("prompt_async: {error}"))?;
        if encoded.len() > MAX_INPUT_BODY_BYTES {
            return Err("prompt_async: encoded input exceeds the 24 MiB body limit".to_string());
        }
        let encoded_bytes = encoded.len();
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/session/{session_id}/prompt_async"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(encoded)
            .send()
            .await
            .map_err(|e| format!("prompt_async: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("prompt_async: HTTP {}", resp.status()));
        }
        Ok(encoded_bytes)
    }

    /// `POST /permission/:id/reply {reply}` — answer a `permission.asked`.
    async fn permission_reply(&self, request_id: &str, reply: &str) -> Result<(), String> {
        let request_id = encode_path_segment(request_id);
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/permission/{request_id}/reply"),
            )
            .json(&json!({ "reply": reply }))
            .send()
            .await
            .map_err(|e| format!("permission reply: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("permission reply: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// `POST /question/:id/reply {answers}`. OpenCode expects one array of
    /// selected labels/custom text per question, in the original order.
    async fn question_reply(
        &self,
        request_id: &str,
        answers: &[Vec<String>],
    ) -> Result<(), String> {
        let request_id = encode_path_segment(request_id);
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/question/{request_id}/reply"),
            )
            .json(&json!({ "answers": answers }))
            .send()
            .await
            .map_err(|e| format!("question reply: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("question reply: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// Explicitly dismiss an unsupported, cancelled, or malformed question so
    /// the OpenCode turn cannot remain blocked forever.
    async fn question_reject(&self, request_id: &str) -> Result<(), String> {
        let request_id = encode_path_segment(request_id);
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/question/{request_id}/reject"),
            )
            .send()
            .await
            .map_err(|e| format!("question reject: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("question reject: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// `POST /session/:id/abort` — interrupt the in-flight turn.
    async fn abort(&self, session_id: &str) -> Result<(), String> {
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/session/{session_id}/abort"),
            )
            .send()
            .await
            .map_err(|e| format!("abort: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("abort: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// `DELETE /session/:id` — best-effort cleanup at `end`.
    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::DELETE, &format!("/session/{session_id}"))
            .send()
            .await
            .map_err(|e| format!("delete session: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("delete session: HTTP {}", resp.status()));
        }
        Ok(())
    }

    async fn finish_session(
        &self,
        session_id: &str,
        lifecycle: SessionLifecycle,
    ) -> Result<(), String> {
        if lifecycle.deletes_session() {
            self.delete_session(session_id).await
        } else {
            Ok(())
        }
    }

    /// Official OpenCode API: direct child sessions created by the task tool.
    async fn child_sessions(&self, session_id: &str) -> Result<BTreeSet<String>, String> {
        let resp = self
            .req(
                reqwest::Method::GET,
                &format!("/session/{session_id}/children"),
            )
            .send()
            .await
            .map_err(|e| format!("children: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("children: HTTP {}", resp.status()));
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("children decode: {e}"))?;
        let Some(children) = value.as_array() else {
            return Err("children decode: expected array".to_string());
        };
        Ok(children
            .iter()
            .filter_map(|child| child.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect())
    }

    /// Bounded breadth-first traversal. OpenCode exposes direct children per
    /// endpoint; task agents may themselves delegate, so one-level polling can
    /// miss a live grandchild when its SSE creation edge was lost.
    async fn child_session_tree(&self, root_id: &str) -> Result<BTreeSet<String>, String> {
        let mut found = BTreeSet::new();
        let mut visited = BTreeSet::from([root_id.to_string()]);
        let mut frontier = vec![(root_id.to_string(), 0usize)];
        while let Some((parent, depth)) = frontier.pop() {
            let children = self.child_sessions(&parent).await?;
            if depth >= CHILD_TREE_MAX_DEPTH && !children.is_empty() {
                return Err(format!(
                    "child session tree exceeds depth cap {CHILD_TREE_MAX_DEPTH}"
                ));
            }
            for child in children {
                if visited.insert(child.clone()) {
                    found.insert(child.clone());
                    if found.len() > CHILD_TREE_MAX_NODES {
                        return Err(format!(
                            "child session tree exceeds node cap {CHILD_TREE_MAX_NODES}"
                        ));
                    }
                    frontier.push((child, depth + 1));
                }
            }
        }
        Ok(found)
    }

    /// Official OpenCode API: map of currently non-idle session statuses. The
    /// server removes idle sessions from this map, so a listed child absent from
    /// the map is terminal/idle.
    async fn session_statuses(&self) -> Result<BTreeMap<String, String>, String> {
        let resp = self
            .req(reqwest::Method::GET, "/session/status")
            .send()
            .await
            .map_err(|e| format!("session status: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("session status: HTTP {}", resp.status()));
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("session status decode: {e}"))?;
        let Some(statuses) = value.as_object() else {
            return Err("session status decode: expected object".to_string());
        };
        Ok(statuses
            .iter()
            .filter_map(|(id, status)| {
                status
                    .get("type")
                    .and_then(Value::as_str)
                    .map(|kind| (id.clone(), kind.to_string()))
            })
            .collect())
    }
}

async fn send_event(tx: &mpsc::Sender<SessionEvent>, event: SessionEvent) -> bool {
    tx.send(crate::redaction::sanitize_session_event(event))
        .await
        .is_ok()
}

fn failed_turn(reason: impl Into<String>) -> SessionEvent {
    SessionEvent::TurnDone {
        status: TurnStatus::Failed(reason.into()),
        usage: None,
    }
}

async fn send_events(tx: &mpsc::Sender<SessionEvent>, events: Vec<SessionEvent>) -> bool {
    for event in events {
        if !send_event(tx, event).await {
            return false;
        }
    }
    true
}

async fn forward_open_code_events(
    http: &HttpCtx,
    session_id: &str,
    tx: &mpsc::Sender<SessionEvent>,
    lifecycle: &Arc<Mutex<ChildLifecycle>>,
    settle_state: &Arc<AtomicU8>,
    turn_sse_gate: &TurnSseGate,
    events: Vec<SessionEvent>,
) -> bool {
    for event in events {
        if !terminal_event_belongs_to_armed_turn(&event, turn_sse_gate) {
            // An initial, duplicated, or delayed terminal edge is not evidence
            // that the currently displayed turn completed.
            continue;
        }
        match event {
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage,
            } => {
                if settle_state
                    .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let http = http.clone();
                    let session_id = session_id.to_string();
                    let tx = tx.clone();
                    let lifecycle = lifecycle.clone();
                    let settle_state = settle_state.clone();
                    tokio::spawn(async move {
                        let result = settle_children(
                            &http,
                            &session_id,
                            &tx,
                            &lifecycle,
                            ChildSettleConfig::default(),
                        )
                        .await;
                        if settle_state
                            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            return;
                        }
                        let status = match result {
                            Ok(()) => TurnStatus::Completed,
                            Err(error) => {
                                tracing::warn!(target: "opencode_session", "{error}");
                                let events = lifecycle.lock().await.fail_open_events();
                                if !send_events(&tx, events).await {
                                    return;
                                }
                                TurnStatus::Failed(error)
                            }
                        };
                        let _ = send_event(&tx, SessionEvent::TurnDone { status, usage }).await;
                    });
                }
            }
            event => {
                if matches!(event, SessionEvent::TurnDone { .. }) {
                    settle_state.store(2, Ordering::SeqCst);
                }
                if !send_event(tx, event).await {
                    return false;
                }
            }
        }
    }
    true
}

fn terminal_event_belongs_to_armed_turn(event: &SessionEvent, gate: &TurnSseGate) -> bool {
    match event {
        // An idle status is session-scoped rather than turn-scoped. Requiring
        // the preceding busy/retry edge prevents an initial or delayed idle
        // from completing a newly armed turn. A premature idle leaves the gate
        // armed so the real busy -> idle sequence can still complete it.
        SessionEvent::TurnDone {
            status: TurnStatus::Completed,
            ..
        } => gate
            .compare_exchange(
                SSE_TURN_ACTIVE,
                SSE_TURN_UNARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok(),
        // A session.error is already an explicit failure for this session. It
        // may precede the busy edge, so accept it for either armed state while
        // still rejecting errors arriving with no locally outstanding prompt.
        SessionEvent::TurnDone { .. } => {
            gate.swap(SSE_TURN_UNARMED, Ordering::AcqRel) != SSE_TURN_UNARMED
        }
        _ => true,
    }
}

/// Reconcile task-tool child sessions after the parent reports `idle`. Polling
/// runs in a detached task while the SSE pump keeps forwarding approvals/events.
/// Requests and total duration are bounded. Completion is honest: all children
/// must be idle, or the turn ends Failed instead of hanging.
async fn settle_children(
    http: &HttpCtx,
    parent_id: &str,
    tx: &mpsc::Sender<SessionEvent>,
    lifecycle: &Arc<Mutex<ChildLifecycle>>,
    cfg: ChildSettleConfig,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + cfg.timeout;
    let mut errors = 0usize;
    loop {
        let snapshot = tokio::time::timeout(cfg.request_timeout, async {
            tokio::join!(http.child_session_tree(parent_id), http.session_statuses())
        })
        .await;
        match snapshot {
            Ok((Ok(mut children), Ok(statuses))) => {
                errors = 0;
                // Direct `/children` is authoritative for the first level. SSE
                // can additionally reveal nested descendants; keep any such
                // known session while the status endpoint still marks it live.
                let (events, settled) = {
                    let mut lifecycle = lifecycle.lock().await;
                    children.extend(
                        lifecycle
                            .known
                            .iter()
                            .filter(|id| {
                                matches!(
                                    statuses.get(*id).map(String::as_str),
                                    Some("busy" | "retry")
                                )
                            })
                            .cloned(),
                    );
                    let events = lifecycle.apply_snapshot(children, &statuses);
                    (events, lifecycle.live.is_empty())
                };
                if !send_events(tx, events).await {
                    return Ok(());
                }
                if settled {
                    return Ok(());
                }
            }
            Ok((children, statuses)) => {
                errors += 1;
                if errors >= cfg.max_errors.max(1) {
                    return Err(format!(
                        "opencode child-session reconciliation failed: children={:?}; status={:?}",
                        children.err(),
                        statuses.err()
                    ));
                }
            }
            Err(_) => {
                errors += 1;
                if errors >= cfg.max_errors.max(1) {
                    return Err("opencode child-session reconciliation timed out".to_string());
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "opencode child sessions did not settle within {}s",
                cfg.timeout.as_secs()
            ));
        }
        tokio::time::sleep(cfg.poll).await;
    }
}

/// Incremental SSE decoder with independent line and logical-frame ceilings.
/// HTTP chunks may split anywhere (including in CRLF and UTF-8), so framing is
/// byte-based and text is decoded only after LF. Unknown SSE fields are ignored.
struct BoundedSseDecoder {
    line: Vec<u8>,
    data_lines: Vec<String>,
    frame_bytes: usize,
    max_line_bytes: usize,
    max_frame_bytes: usize,
}

impl Default for BoundedSseDecoder {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data_lines: Vec::new(),
            frame_bytes: 0,
            max_line_bytes: MAX_SSE_LINE_BYTES,
            max_frame_bytes: MAX_SSE_FRAME_BYTES,
        }
    }
}

impl BoundedSseDecoder {
    #[cfg(test)]
    fn with_limits(max_line_bytes: usize, max_frame_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            max_frame_bytes,
            ..Self::default()
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, String> {
        let mut payloads = Vec::new();
        for byte in chunk {
            if self.line.len() >= self.max_line_bytes {
                return Err("opencode SSE line exceeded the 32 MiB safety limit".to_string());
            }
            self.line.push(*byte);
            if *byte != b'\n' {
                continue;
            }
            let line_bytes = std::mem::take(&mut self.line);
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data_lines.is_empty() {
                    payloads.push(self.data_lines.join("\n"));
                    self.data_lines.clear();
                    self.frame_bytes = 0;
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.strip_prefix(' ').unwrap_or(rest);
                let separator = usize::from(!self.data_lines.is_empty());
                self.frame_bytes = self
                    .frame_bytes
                    .checked_add(separator)
                    .and_then(|bytes| bytes.checked_add(data.len()))
                    .ok_or_else(|| "opencode SSE frame size overflow".to_string())?;
                if self.frame_bytes > self.max_frame_bytes {
                    return Err(
                        "opencode SSE data frame exceeded the 32 MiB safety limit".to_string()
                    );
                }
                self.data_lines.push(data.to_string());
            }
        }
        Ok(payloads)
    }
}

/// Background task: open the long-lived SSE stream and pump normalized events
/// into `tx` forever. On stream end / error (the server died or the connection
/// dropped) emit a terminal `Failed` so a mid-turn drop surfaces as
/// `TurnDone{Failed}` rather than a silent hang. Fail-open throughout.
#[cfg(test)]
async fn pump_sse(http: HttpCtx, session_id: String, tx: mpsc::Sender<SessionEvent>) {
    pump_sse_with_ready(
        http,
        session_id,
        tx,
        None,
        Arc::new(AtomicU8::new(SSE_TURN_ACTIVE)),
    )
    .await;
}

/// SSE pump core with an optional one-shot subscription barrier. The ready
/// signal fires only after OpenCode accepted the event-stream request and sent
/// successful response headers; failure is reported through both the barrier
/// and the normal terminal session event.
async fn pump_sse_with_ready(
    http: HttpCtx,
    session_id: String,
    tx: mpsc::Sender<SessionEvent>,
    mut ready: Option<oneshot::Sender<Result<(), String>>>,
    turn_sse_gate: TurnSseGate,
) {
    // Loopback-only: `base_url` is the address WE scraped from our OWN
    // `opencode serve` child's stdout (always 127.0.0.1) — not attacker input.
    let url = format!("{}/event?directory={}", http.base_url, http.directory);
    let resp = http
        .client
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, &http.auth)
        .header("x-opencode-directory", &http.directory)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await;
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let error = format!("event stream: HTTP {}", r.status());
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(error.clone()));
            }
            let _ = send_event(&tx, failed_turn(error)).await;
            return;
        }
        Err(e) => {
            let error = format!("event stream connect: {e}");
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(error.clone()));
            }
            let _ = send_event(&tx, failed_turn(error)).await;
            return;
        }
    };
    if let Some(ready) = ready.take() {
        let _ = ready.send(Ok(()));
    }

    // SSE framing: lines `event: ...` / `data: ...`, frames separated by a blank
    // line (handlers/event.ts encodes `JSON.stringify(payload)` per data line).
    // Accumulate `data:` lines until a blank line, then parse one frame.
    let mut byte_stream = resp.bytes_stream();
    let mut decoder = BoundedSseDecoder::default();
    // Per-part streaming state (text suffix lengths + which tools already emitted a
    // ToolCall), so a cumulative text update forwards only its new suffix and a
    // tool that skipped its `running` frame still gets a back-filled ToolCall (F6).
    // Lives for the whole subscription.
    let mut tracker = PartTracker::default();
    let child_lifecycle = Arc::new(Mutex::new(ChildLifecycle::default()));
    // 0 = active/ready for idle, 1 = child reconciliation in flight, 2 = one
    // terminal boundary already won. Parent activity resets it for the next turn.
    let settle_state = Arc::new(AtomicU8::new(0));
    while let Some(chunk) = byte_stream.next().await {
        let Ok(bytes) = chunk else {
            break; // stream error -> fall through to terminal Failed
        };
        let payloads = match decoder.push(&bytes) {
            Ok(payloads) => payloads,
            Err(error) => {
                if turn_sse_gate.swap(SSE_TURN_UNARMED, Ordering::AcqRel) != SSE_TURN_UNARMED {
                    let _ = send_event(&tx, failed_turn(error)).await;
                }
                return;
            }
        };
        for payload in payloads {
            if parent_turn_is_active(&payload, &session_id) {
                if turn_sse_gate
                    .compare_exchange(
                        SSE_TURN_ARMED,
                        SSE_TURN_ACTIVE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    // A fresh official busy/retry edge is the exact boundary at
                    // which usage from the previous prompt must become
                    // unreachable. Duplicate retry/busy frames do not reset an
                    // already-active turn.
                    tracker.begin_turn();
                }
                settle_state.store(0, Ordering::SeqCst);
            }
            let child_events = child_lifecycle
                .lock()
                .await
                .observe_sse(&payload, &session_id);
            if !send_events(&tx, child_events).await {
                return;
            }
            let capture_usage = turn_sse_gate.load(Ordering::Acquire) == SSE_TURN_ACTIVE;
            let events = translate_frame_tracked_for_turn(
                &payload,
                &session_id,
                &mut tracker,
                capture_usage,
            );
            if !forward_open_code_events(
                &http,
                &session_id,
                &tx,
                &child_lifecycle,
                &settle_state,
                &turn_sse_gate,
                events,
            )
            .await
            {
                return;
            }
            // Keep streaming after a settled TurnDone — the SAME
            // subscription serves every phase's turn (one long GET).
        }
    }
    // Stream ended / errored -> terminal failure so the runner never hangs.
    settle_state.store(2, Ordering::SeqCst);
    if turn_sse_gate.swap(SSE_TURN_UNARMED, Ordering::AcqRel) != SSE_TURN_UNARMED {
        let _ = send_event(&tx, failed_turn("event stream ended")).await;
    }
}

/// Per-subscription state the streaming pump threads across SSE frames.
///
/// - `text_lens`: how much of each cumulative TEXT part has already been emitted,
///   so a resent full-text update forwards only its NEW suffix.
/// - `tools_called`: the tool part ids for which a [`SessionEvent::ToolCall`] has
///   already been emitted, so the `completed`/`error` branch can BACK-FILL a
///   missing `ToolCall` when a fast / SSE-merged tool skipped its `running` frame
///   (F6) — without double-emitting for the normal `running → completed` path.
#[derive(Default)]
pub struct PartTracker {
    text_lens: std::collections::HashMap<String, usize>,
    reasoning_lens: std::collections::HashMap<String, usize>,
    tools_called: std::collections::HashSet<String>,
    /// Tool part ids whose terminal result was already emitted. OpenCode may
    /// replay the same completed/error SSE frame; without this guard one real
    /// execution became multiple pitfall episodes downstream.
    tools_finished: std::collections::HashSet<String>,
    session_model: Option<String>,
    /// Authoritative `message.updated.info.role` keyed by message id. OpenCode
    /// publishes the just-submitted user message through the same
    /// `message.part.updated` stream as assistant output, so text without an
    /// assistant role must never become [`SessionEvent::TextDelta`].
    message_roles: HashMap<String, OpenCodeMessageRole>,
    message_role_order: VecDeque<String>,
    /// Latest exact usage for every assistant message in the current locally
    /// armed turn. OpenCode resends `message.updated` as tokens grow; replacing
    /// by message id avoids double-counting, while summing distinct assistant
    /// messages includes tool-continuation model calls in the same turn.
    assistant_usage: HashMap<String, Usage>,
}

impl PartTracker {
    fn begin_turn(&mut self) {
        self.assistant_usage.clear();
    }

    fn observe_message_role(&mut self, props: &Value, session_id: &str) {
        if props.get("sessionID").and_then(Value::as_str) != Some(session_id) {
            return;
        }
        let Some(info) = props.get("info") else {
            return;
        };
        if info.get("sessionID").and_then(Value::as_str) != Some(session_id) {
            return;
        }
        let Some(message_id) = info
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let role = match info.get("role").and_then(Value::as_str) {
            Some("assistant") => OpenCodeMessageRole::Assistant,
            Some("user") => OpenCodeMessageRole::User,
            _ => return,
        };
        if self
            .message_roles
            .insert(message_id.to_string(), role)
            .is_none()
        {
            self.message_role_order.push_back(message_id.to_string());
        }
        while self.message_role_order.len() > MAX_TRACKED_MESSAGE_ROLES {
            if let Some(expired) = self.message_role_order.pop_front() {
                self.message_roles.remove(&expired);
            }
        }
    }

    fn text_part_is_assistant(&self, part: &Value) -> bool {
        let Some(message_id) = part
            .get("messageID")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            // Compatibility for old recorded frames that predate message ids.
            return true;
        };
        matches!(
            self.message_roles.get(message_id),
            Some(OpenCodeMessageRole::Assistant)
        )
    }

    fn observe_message_usage(&mut self, props: &Value, session_id: &str) {
        if props.get("sessionID").and_then(Value::as_str) != Some(session_id) {
            return;
        }
        let Some(info) = props.get("info") else {
            return;
        };
        if info.get("sessionID").and_then(Value::as_str) != Some(session_id)
            || info.get("role").and_then(Value::as_str) != Some("assistant")
        {
            return;
        }
        let Some(message_id) = info
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let Some(usage) = info.get("tokens").and_then(parse_opencode_usage) else {
            return;
        };
        self.assistant_usage.insert(message_id.to_string(), usage);
    }

    fn take_turn_usage(&mut self) -> Option<Usage> {
        if self.assistant_usage.is_empty() {
            return None;
        }
        let usage = self
            .assistant_usage
            .values()
            .copied()
            .reduce(Usage::merge)?;
        self.assistant_usage.clear();
        Some(usage)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenCodeMessageRole {
    User,
    Assistant,
}

/// Parse the official assistant `info.tokens` shape. Cache reads/writes are
/// consumed input and reasoning is generated output, matching the accounting
/// convention used by the other four base drivers. A partial, negative,
/// fractional, or otherwise malformed shape is ignored rather than replacing a
/// previously valid update for that assistant message.
fn parse_opencode_usage(tokens: &Value) -> Option<Usage> {
    let component = |value: Option<&Value>| value?.as_u64();
    let uncached_input = component(tokens.get("input"))?;
    let plain_output = component(tokens.get("output"))?;
    let reasoning_tokens = component(tokens.get("reasoning"))?;
    let cached_read_tokens = component(tokens.pointer("/cache/read"))?;
    let cached_write_tokens = component(tokens.pointer("/cache/write"))?;
    let input_tokens = uncached_input
        .checked_add(cached_read_tokens)?
        .checked_add(cached_write_tokens)?;
    let output_tokens = plain_output.checked_add(reasoning_tokens)?;
    let total_tokens = input_tokens.checked_add(output_tokens)?;
    if let Some(wire_total) = tokens.get("total") {
        if wire_total.as_u64()? != total_tokens {
            return None;
        }
    }
    Some(Usage {
        cached_read_tokens,
        cached_write_tokens,
        reasoning_tokens,
        ..Usage::exact(input_tokens, output_tokens)
    })
}

/// Translate one SSE frame's JSON payload (`{id,type,properties}`) into zero or
/// more normalized [`SessionEvent`]s, scoped to `session_id`. Unknown / off-
/// session / malformed frames yield nothing (fail-open). Mirrors opencode's own
/// consumer in `cli/cmd/run.ts`.
#[must_use]
pub fn translate_frame(payload: &str, session_id: &str) -> Vec<SessionEvent> {
    // Fresh tracker — correct for a single, standalone frame (the whole text IS the
    // suffix). The streaming pump uses `translate_frame_tracked` with a persistent
    // tracker so multi-update text parts only forward their new suffix.
    translate_frame_tracked(payload, session_id, &mut PartTracker::default())
}

/// Like [`translate_frame`] but threads a persistent [`PartTracker`] so a
/// cumulative text part (opencode resends the whole accumulated text each update)
/// only forwards its NEW suffix, and a tool that skipped its `running` frame still
/// gets a back-filled `ToolCall` (F6).
#[must_use]
pub fn translate_frame_tracked(
    payload: &str,
    session_id: &str,
    tracker: &mut PartTracker,
) -> Vec<SessionEvent> {
    translate_frame_tracked_for_turn(payload, session_id, tracker, true)
}

fn translate_frame_tracked_for_turn(
    payload: &str,
    session_id: &str,
    tracker: &mut PartTracker,
    capture_usage: bool,
) -> Vec<SessionEvent> {
    translate_frame_tracked_raw(payload, session_id, tracker, capture_usage)
        .into_iter()
        .map(crate::redaction::sanitize_session_event)
        .collect()
}

fn translate_frame_tracked_raw(
    payload: &str,
    session_id: &str,
    tracker: &mut PartTracker,
    capture_usage: bool,
) -> Vec<SessionEvent> {
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return Vec::new();
    };
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let props = v.get("properties").cloned().unwrap_or(Value::Null);
    match kind {
        "message.updated" => {
            tracker.observe_message_role(&props, session_id);
            if capture_usage {
                tracker.observe_message_usage(&props, session_id);
            }
            Vec::new()
        }
        "message.part.updated" => translate_part(&props, session_id, tracker),
        "session.error" => translate_error(&props, session_id, tracker),
        "permission.asked" => translate_permission(&props, session_id),
        "question.asked" => translate_question(&props, session_id),
        "session.status" => translate_status(&props, session_id, tracker),
        "session.created" | "session.updated" => {
            translate_session_model(&props, session_id, tracker)
        }
        // Connection / liveness frames carry no turn semantics.
        _ => Vec::new(),
    }
}

fn translate_session_model(
    props: &Value,
    session_id: &str,
    tracker: &mut PartTracker,
) -> Vec<SessionEvent> {
    if props.get("sessionID").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    let Some(model) = props
        .pointer("/info/model")
        .or_else(|| props.get("model"))
        .and_then(opencode_model_ref)
    else {
        return Vec::new();
    };
    if tracker.session_model.as_deref() == Some(model.as_str()) {
        return Vec::new();
    }
    tracker.session_model = Some(model.clone());
    vec![SessionEvent::SessionModel(model)]
}

fn opencode_model_ref(v: &Value) -> Option<String> {
    let model_id = v
        .get("modelID")
        .or_else(|| v.get("model_id"))
        .or_else(|| v.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let provider_id = v
        .get("providerID")
        .or_else(|| v.get("provider_id"))
        .or_else(|| v.get("provider"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let base = match provider_id {
        Some(provider) if model_id.starts_with(&format!("{provider}/")) => model_id.to_string(),
        Some(provider) => format!("{provider}/{model_id}"),
        None => model_id.to_string(),
    };
    let variant = v
        .get("variant")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default");
    Some(match variant {
        Some(variant) => format!("{base}/{variant}"),
        None => base,
    })
}

/// Map opencode's lowercase tool name to the claude-shaped name the agent-side
/// consumers match on (`Write`/`Edit`/…), so an opencode write/edit renders a diff
/// card and enters the audit + governance trail. An unknown tool gets a
/// capitalized first letter (consistent display; it isn't a file edit anyway).
fn normalize_tool_name(name: &str) -> String {
    match name {
        "write" => "Write".to_string(),
        "edit" => "Edit".to_string(),
        "multiedit" => "MultiEdit".to_string(),
        "read" => "Read".to_string(),
        "bash" => "Bash".to_string(),
        "grep" => "Grep".to_string(),
        "glob" => "Glob".to_string(),
        "list" | "ls" => "LS".to_string(),
        "webfetch" => "WebFetch".to_string(),
        "task" => "Task".to_string(),
        other => {
            let mut chars = other.chars();
            chars.next().map_or_else(
                || other.to_string(),
                |first| first.to_uppercase().collect::<String>() + chars.as_str(),
            )
        }
    }
}

/// Rename opencode's camelCase tool-input keys to the snake_case keys the agent
/// reads (`filePath`→`file_path`, `oldString`→`old_string`, `newString`→
/// `new_string`); `content` is already shared. Non-object input is returned as-is.
fn normalize_tool_input(mut input: Value) -> Value {
    if let Some(obj) = input.as_object_mut() {
        for (from, to) in [
            ("filePath", "file_path"),
            ("oldString", "old_string"),
            ("newString", "new_string"),
        ] {
            if let Some(v) = obj.remove(from) {
                obj.entry(to.to_string()).or_insert(v);
            }
        }
    }
    input
}

/// `message.part.updated` -> `ToolCall`/`ToolResult` (for `part.type=="tool"`)
/// or `TextDelta` (for `part.type=="text"`). Tool input/output schema:
/// `core/src/v1/session.ts ToolPart`/`ToolState`. `text_lens` tracks how much of
/// each text part we have already emitted, so a cumulative text update only emits
/// its NEW suffix (opencode resends the whole accumulated text every update).
fn translate_part(props: &Value, session_id: &str, tracker: &mut PartTracker) -> Vec<SessionEvent> {
    let Some(part) = props.get("part") else {
        return Vec::new();
    };
    if part.get("sessionID").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    match part.get("type").and_then(Value::as_str) {
        Some("tool") => translate_tool_part(part, tracker),
        Some("text" | "reasoning") => {
            // The global SSE stream repeats the user's submitted message as a
            // text part. Only an explicitly assistant-attributed message may be
            // projected into the assistant transcript. A modern part with an
            // unknown role is withheld rather than risking a user→assistant
            // attribution error; legacy parts without messageID retain their
            // historical behavior.
            if !tracker.text_part_is_assistant(part) {
                return Vec::new();
            }
            let Some(full) = part
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            else {
                return Vec::new();
            };
            // opencode resends the FULL accumulated text of the part on every
            // update; emit only the NEW suffix since we last saw THIS part (by id)
            // so the consumer's append doesn't pile up 'H','He','Hel',… Without
            // this the reply is duplicated and grows quadratically.
            let id = part
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let reasoning = part.get("type").and_then(Value::as_str) == Some("reasoning");
            let lengths = if reasoning {
                &mut tracker.reasoning_lens
            } else {
                &mut tracker.text_lens
            };
            let prev = lengths.get(&id).copied().unwrap_or(0);
            // Cumulative growth → suffix; a non-monotonic / replaced part (shorter,
            // or `prev` not a char boundary) re-emits the whole current text.
            let suffix = if full.len() >= prev && full.is_char_boundary(prev) {
                &full[prev..]
            } else {
                full
            };
            lengths.insert(id, full.len());
            if suffix.is_empty() {
                Vec::new()
            } else if reasoning {
                vec![SessionEvent::ThinkingDelta(suffix.to_string())]
            } else {
                vec![SessionEvent::TextDelta(suffix.to_string())]
            }
        }
        _ => Vec::new(),
    }
}

/// Translate a `part.type=="tool"` update into `ToolCall` / `ToolResult` events,
/// tracking emission so a tool that SKIPPED its `running` frame still surfaces.
///
/// opencode normally streams a tool as `pending → running → completed`, and we
/// emit the `ToolCall` truth on `running` (input finalized, incl. the write
/// path). But a fast tool, or coalesced SSE frames, can deliver `pending →
/// completed` with NO standalone `running` frame — and the old code then emitted
/// ONLY a `ToolResult`, so that write never entered the audit trail and rendered
/// no tool row / diff (F6). Now, on `completed`/`error`, if this part never
/// emitted a `ToolCall` and we have a usable input, we BACK-FILL the `ToolCall`
/// (normalized name + input) before the `ToolResult`. The `tools_called` set keeps
/// the normal `running → completed` path from double-emitting.
fn opencode_tool_call_event(call_id: Option<&str>, name: String, input: Value) -> SessionEvent {
    match call_id.filter(|id| !id.is_empty()) {
        Some(call_id) => SessionEvent::ToolCallCorrelated {
            call_id: call_id.to_string(),
            name,
            input,
        },
        None => SessionEvent::ToolCall { name, input },
    }
}

fn opencode_tool_result_event(call_id: Option<&str>, ok: bool, summary: String) -> SessionEvent {
    match call_id.filter(|id| !id.is_empty()) {
        Some(call_id) => SessionEvent::ToolResultCorrelated {
            call_id: call_id.to_string(),
            ok,
            summary,
        },
        None => SessionEvent::ToolResult { ok, summary },
    }
}

fn translate_tool_part(part: &Value, tracker: &mut PartTracker) -> Vec<SessionEvent> {
    // Normalize opencode's tool shape to the claude-shaped names + input keys the
    // agent-side consumers (diff card, audit, governance, tool-row detail)
    // recognize. opencode emits lowercase `write`/`edit` and camelCase
    // `filePath`/`oldString`/`newString`; without this an opencode edit renders NO
    // diff card and its audit/path attribution is blank.
    let name = normalize_tool_name(part.get("tool").and_then(Value::as_str).unwrap_or("tool"));
    let state = part.get("state").cloned().unwrap_or(Value::Null);
    let status = state.get("status").and_then(Value::as_str).unwrap_or("");
    let input = normalize_tool_input(state.get("input").cloned().unwrap_or(Value::Null));
    let vendor_part_id = part
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    // Tracking may fall back to callID for old frames, but only the official
    // part.id is exposed as the cross-component correlation id.
    let tracking_id = vendor_part_id
        .or_else(|| part.get("callID").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let call_id = vendor_part_id;
    match status {
        // running = the tool actually started (input now finalized) — the ToolCall
        // truth. Mark the part so a later `completed` doesn't re-emit it.
        "running" => {
            tracker.tools_called.insert(tracking_id);
            vec![opencode_tool_call_event(call_id, name, input)]
        }
        "completed" => {
            // The cap widens to the full captured output when the user opts into
            // process logs (`UMADEV_SHOW_PROCESS_LOGS`), so a long-running command's
            // build log reaches the transcript; OFF keeps the tight 200-char clip.
            // Direction follows the path: verbose keeps the TAIL (the build's failure
            // verdict at the END survives); OFF keeps the head clip (a summary).
            let on = crate::process_logs::show_process_logs();
            let raw = state
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| state.get("output").and_then(Value::as_str))
                .unwrap_or("");
            let summary =
                crate::process_logs::truncate_preview(raw, crate::process_logs::cap_for(on), on);
            backfilled_tool_events(
                tracker,
                &tracking_id,
                call_id,
                &name,
                &input,
                opencode_tool_result_event(call_id, true, summary),
            )
        }
        "error" => {
            let on = crate::process_logs::show_process_logs();
            let raw = state
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("tool error");
            let summary =
                crate::process_logs::truncate_preview(raw, crate::process_logs::cap_for(on), on);
            backfilled_tool_events(
                tracker,
                &tracking_id,
                call_id,
                &name,
                &input,
                opencode_tool_result_event(call_id, false, summary),
            )
        }
        // pending = queued, no finalized input yet -> wait for running/completed.
        _ => Vec::new(),
    }
}

/// Build the events for a terminal (`completed` / `error`) tool frame, BACK-FILLING
/// a `ToolCall` first if this part never emitted one and it carries a usable input
/// (F6). A terminal result is emitted once per non-empty part id; replayed SSE
/// terminal frames are idempotent so one execution cannot inflate pitfall hits.
fn backfilled_tool_events(
    tracker: &mut PartTracker,
    tracking_id: &str,
    call_id: Option<&str>,
    name: &str,
    input: &Value,
    result: SessionEvent,
) -> Vec<SessionEvent> {
    if !tracking_id.is_empty() && !tracker.tools_finished.insert(tracking_id.to_string()) {
        return Vec::new();
    }
    let already_called = tracker.tools_called.contains(tracking_id);
    // Only back-fill when we have a real input object — a terminal frame with no
    // recoverable input can't be a faithful ToolCall (fail-open: just the result,
    // exactly as before). A non-null object (even `{}`) is enough: the consumer
    // keys off the tool NAME for the audit/diff, and a `{}` still attributes it.
    let have_input = !input.is_null();
    if !already_called && have_input {
        tracker.tools_called.insert(tracking_id.to_string());
        vec![
            opencode_tool_call_event(call_id, name.to_string(), input.clone()),
            result,
        ]
    } else {
        vec![result]
    }
}

/// `session.error` -> a terminal `TurnDone{Failed}` so a base-side error ends
/// the phase (rather than hanging until the run budget). `properties.error`
/// carries `{name, data.message}` (see `cli/cmd/run.ts`).
fn translate_error(
    props: &Value,
    session_id: &str,
    tracker: &mut PartTracker,
) -> Vec<SessionEvent> {
    // session.error may omit sessionID for global errors; if present it must
    // match. Either way we treat it as a turn-ending failure (fail-open).
    if let Some(sid) = props.get("sessionID").and_then(Value::as_str) {
        if sid != session_id {
            return Vec::new();
        }
    }
    let err = props.get("error");
    let msg = err
        .and_then(|e| e.get("data"))
        .and_then(|d| d.get("message"))
        .and_then(Value::as_str)
        .or_else(|| err.and_then(|e| e.get("name")).and_then(Value::as_str))
        .unwrap_or("opencode session error")
        .to_string();
    vec![SessionEvent::TurnDone {
        status: TurnStatus::Failed(msg),
        usage: tracker.take_turn_usage(),
    }]
}

/// Preserve OpenCode's permission request as a typed approval, including its
/// three exact reply choices. Unknown/future permissions remain approval-gated.
fn translate_permission(props: &Value, session_id: &str) -> Vec<SessionEvent> {
    if props.get("sessionID").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    let Some(req_id) = props.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let action = props
        .get("permission")
        .and_then(Value::as_str)
        .unwrap_or("permission")
        .to_string();
    let target = props
        .get("patterns")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let message = props
        .get("metadata")
        .and_then(|metadata| metadata.get("description"))
        .and_then(Value::as_str)
        .map(str::to_string);
    vec![SessionEvent::HostRequest {
        req_id: req_id.to_string(),
        request: HostRequest::Approval {
            action,
            target,
            message,
            options: vec![
                HostApprovalOption {
                    id: "once".to_string(),
                    label: "Allow once".to_string(),
                    kind: HostApprovalOptionKind::AllowOnce,
                },
                HostApprovalOption {
                    id: "always".to_string(),
                    label: "Always allow".to_string(),
                    kind: HostApprovalOptionKind::AllowAlways,
                },
                HostApprovalOption {
                    id: "reject".to_string(),
                    label: "Reject".to_string(),
                    kind: HostApprovalOptionKind::RejectOnce,
                },
            ],
            metadata: json!({
                "session_id": session_id,
                "patterns": props.get("patterns").cloned().unwrap_or(Value::Null),
                "always": props.get("always").cloned().unwrap_or(Value::Null),
            }),
        },
    }]
}

/// Convert OpenCode QuestionV1 (`question.asked`) into ordered typed questions.
/// Question IDs are deterministic derivatives of the request id because the
/// protocol identifies the request and question position, not each question.
fn translate_question(props: &Value, session_id: &str) -> Vec<SessionEvent> {
    if props.get("sessionID").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    let Some(req_id) = props.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(raw_questions) = props.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    if raw_questions.is_empty() {
        return Vec::new();
    }

    let questions = raw_questions
        .iter()
        .enumerate()
        .map(|(index, raw)| {
            let header = raw
                .get("header")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let prompt = raw
                .get("question")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| header.clone())
                .unwrap_or_else(|| "OpenCode question".to_string());
            let multiple = raw
                .get("multiple")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let custom = raw.get("custom").and_then(Value::as_bool).unwrap_or(true);
            let options = raw
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let label = option
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())?;
                    Some(HostQuestionOption {
                        value: label.to_string(),
                        label: label.to_string(),
                        description: option
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                        preview: None,
                    })
                })
                .collect::<Vec<_>>();
            let kind = if options.is_empty() {
                HostQuestionKind::Text
            } else if multiple && custom {
                HostQuestionKind::Other("multi_choice_or_text".to_string())
            } else if multiple {
                HostQuestionKind::MultiChoice
            } else if custom {
                HostQuestionKind::Other("single_choice_or_text".to_string())
            } else {
                HostQuestionKind::SingleChoice
            };
            HostQuestion {
                id: format!("{req_id}:{index}"),
                header,
                prompt,
                kind,
                required: !multiple,
                options,
            }
        })
        .collect::<Vec<_>>();

    let custom = raw_questions
        .iter()
        .map(|question| {
            question
                .get("custom")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    vec![SessionEvent::HostRequest {
        req_id: req_id.to_string(),
        request: HostRequest::UserInput {
            questions,
            metadata: json!({
                "session_id": session_id,
                "custom_answers": custom,
                "tool": props.get("tool").cloned().unwrap_or(Value::Null),
            }),
        },
    }]
}

/// `session.status` -> `TurnDone{Completed}` when `status.type=="idle"` for our
/// session. This is THE authoritative turn-done boundary (`cli/cmd/run.ts`
/// breaks its loop on exactly this). `busy`/`retry` carry no turn semantics.
fn translate_status(
    props: &Value,
    session_id: &str,
    tracker: &mut PartTracker,
) -> Vec<SessionEvent> {
    if props.get("sessionID").and_then(Value::as_str) != Some(session_id) {
        return Vec::new();
    }
    let idle = props
        .get("status")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        == Some("idle");
    if idle {
        vec![SessionEvent::TurnDone {
            status: TurnStatus::Completed,
            usage: tracker.take_turn_usage(),
        }]
    } else {
        Vec::new()
    }
}

/// Read the resident server's stdout until its `... listening on
/// http://HOST:PORT` line, returning the scraped base url. Bounded by `timeout`
/// (fail-open: a server that never prints the line errors out instead of
/// hanging).
async fn read_listening_url(stdout: ChildStdout, timeout: Duration) -> Result<String, String> {
    let read = async {
        // Read raw bytes per line (lossy decode) so one odd byte in the announce
        // banner can't abort the scrape, and so the SAME reader can be handed to
        // the lifetime drain below.
        let mut reader = BufReader::new(stdout);
        loop {
            match read_bounded_serve_line(&mut reader, MAX_SERVE_STDOUT_LINE_BYTES).await {
                Ok(None) => {
                    return Err(
                        "opencode serve exited before announcing a listen address".to_string()
                    );
                }
                Ok(Some(line_buf)) => {
                    let line = String::from_utf8_lossy(&line_buf);
                    if let Some(url) = parse_listening_url(&line) {
                        // M8: the server is LONG-LIVED. If we drop its stdout reader
                        // here, anything it later logs to stdout fills the ~64 KiB
                        // pipe buffer and the next write EPIPE/SIGPIPE-kills the
                        // server mid-run (stderr is already drained on its own task).
                        // Keep draining stdout in the background for the session's
                        // lifetime; the drain ends at EOF when the child is killed
                        // (kill_on_drop), so it never leaks.
                        tokio::spawn(async move {
                            let mut sink = [0u8; 8192];
                            while let Ok(n) =
                                tokio::io::AsyncReadExt::read(&mut reader, &mut sink).await
                            {
                                if n == 0 {
                                    break; // EOF — the server exited
                                }
                            }
                        });
                        return Ok(url);
                    }
                }
                Err(e) => return Err(format!("opencode serve stdout read error: {e}")),
            }
        }
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(res) => res,
        Err(_) => Err(format!(
            "opencode serve did not announce a listen address within {}s",
            timeout.as_secs()
        )),
    }
}

async fn read_bounded_serve_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|error| format!("opencode serve stdout read error: {error}"))?;
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
        Err(format!(
            "opencode serve stdout line exceeded the {limit}-byte safety limit"
        ))
    } else {
        Ok(Some(bytes))
    }
}

/// Pull the `http://HOST:PORT` base url out of opencode's listening line
/// (`opencode server listening on http://127.0.0.1:54321`, per `serve.ts`).
/// Returns the bare `scheme://host:port` (no trailing path), trimming any
/// trailing punctuation. Exposed for tests.
#[must_use]
pub fn parse_listening_url(line: &str) -> Option<String> {
    let idx = line.find("http://").or_else(|| line.find("https://"))?;
    let rest = &line[idx..];
    // Stop at the first whitespace / trailing punctuation; the listen line has
    // no path component, so the url is `scheme://host:port`.
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')', '"', '\'']);
    if url.len() > "http://".len() {
        Some(url.to_string())
    } else {
        None
    }
}

/// Split a `provider/model` id into `(provider, model)`; `None` for a bare id
/// (which is NOT in opencode's provider/model shape). Mirrors the single-shot
/// driver's "only pass a model when it's an opencode-compatible id" rule.
fn split_provider_model(id: &str) -> Option<(&str, &str)> {
    let (provider, model) = id.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        None
    } else {
        Some((provider, model))
    }
}

fn encode_path_segment(value: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// Least-authority rules for model-side intent triage and critic review.
/// OpenCode resolves the specific tool rules over the wildcard floor, so this
/// is deliberately deny-by-default: only local, non-mutating source inspection
/// is admitted. In particular, delegation (`task`), all write/shell tools,
/// unknown future tools and MCP-provided tools stay denied without relying on
/// an exhaustive blocklist.
fn readonly_session_ruleset() -> Value {
    json!([
        { "permission": "*", "pattern": "*", "action": "deny" },
        { "permission": "read", "pattern": "*", "action": "allow" },
        { "permission": "grep", "pattern": "*", "action": "allow" },
        { "permission": "glob", "pattern": "*", "action": "allow" },
        { "permission": "list", "pattern": "*", "action": "allow" },
    ])
}

fn plan_session_ruleset() -> Value {
    json!([
        { "permission": "*", "pattern": "*", "action": "deny" },
        { "permission": "read", "pattern": "*", "action": "allow" },
        { "permission": "grep", "pattern": "*", "action": "allow" },
        { "permission": "glob", "pattern": "*", "action": "allow" },
        { "permission": "list", "pattern": "*", "action": "allow" },
        { "permission": "question", "pattern": "*", "action": "allow" },
    ])
}

/// Build the `POST /session` permission ruleset for the autonomy tier — the
/// opencode counterpart of codex's `approvalPolicy` (`never` vs `on-request`) and
/// claude's `--permission-mode` (`acceptEdits` vs `default`), so all three native
/// bases share ONE gate posture.
///
/// - `autonomous == true` (the `auto` trust tier): a single wildcard `allow` rule
///   — every tool call is silently pre-approved, the agentic loop runs without a
///   per-event round-trip. Governance still audits each call via the event stream.
/// - `autonomous == false` (the `guarded` tier): only known local source-inspection
///   permissions are pre-authorized. The wildcard floor is `ask`, so writes,
///   patches, shells, delegation, unknown future tools, and MCP-provided tools all
///   raise `permission.asked` (→ a `NeedApproval` the orchestrator answers via the
///   trust-tiered `approval_decision`). opencode evaluates a specific permission
///   over the wildcard floor. Mirrors codex's `on-request`, while making the
///   authorization boundary fail-closed when the tool vocabulary grows.
///
/// Runtime/protocol failures remain fail-open to UmaDev's governance contract (an
/// error is surfaced rather than crashing the host), but AUTHORIZATION itself is
/// fail-closed: an unrecognized permission is never silently granted in Guarded or
/// Plan. Pure; exposed for tests.
#[must_use]
pub fn session_ruleset(autonomous: bool) -> Value {
    let permissions = if autonomous {
        BasePermissionProfile::Auto
    } else {
        BasePermissionProfile::Guarded
    };
    session_ruleset_for_profile(permissions)
}

fn session_ruleset_for_profile(permissions: BasePermissionProfile) -> Value {
    if permissions.auto_approve() {
        // Questions are safe to start because the HTTP/SSE driver now carries
        // QuestionV1 answers back in-turn. Plan-mode toggles remain disabled: the
        // UmaDev permission profile is authoritative for the session.
        return json!([
            { "permission": "*", "pattern": "*", "action": "allow" },
            { "permission": "question", "pattern": "*", "action": "allow" },
            { "permission": "plan_enter", "pattern": "*", "action": "deny" },
            { "permission": "plan_exit", "pattern": "*", "action": "deny" },
        ]);
    }
    if matches!(permissions, BasePermissionProfile::Plan) {
        return plan_session_ruleset();
    }
    // Guarded: authorization is ASK-by-default. Only the small set of permissions
    // whose contract is local source inspection is pre-authorized. Explicit asks
    // for today's known mutation/delegation surfaces document the intended posture;
    // the wildcard ask contains every future or MCP-provided tool automatically.
    // opencode resolves the specific permission over the wildcard floor.
    json!([
        { "permission": "*", "pattern": "*", "action": "ask" },
        { "permission": "question", "pattern": "*", "action": "allow" },
        { "permission": "plan_enter", "pattern": "*", "action": "deny" },
        { "permission": "plan_exit", "pattern": "*", "action": "deny" },
        // Known read-only, local source inspection. Keep this list deliberately
        // narrow: adding a tool here is an authorization decision, not compatibility
        // plumbing. Network retrieval and external-directory access therefore ask.
        { "permission": "read", "pattern": "*", "action": "allow" },
        { "permission": "grep", "pattern": "*", "action": "allow" },
        { "permission": "glob", "pattern": "*", "action": "allow" },
        { "permission": "list", "pattern": "*", "action": "allow" },
        // Known potentially mutating/delegating surfaces. These explicit rules are
        // semantically redundant with the wildcard ask, but keep the contract clear
        // and guard against a future broad allow being reintroduced below.
        { "permission": "edit", "pattern": "*", "action": "ask" },
        { "permission": "write", "pattern": "*", "action": "ask" },
        { "permission": "patch", "pattern": "*", "action": "ask" },
        { "permission": "bash", "pattern": "*", "action": "ask" },
        { "permission": "task", "pattern": "*", "action": "ask" },
    ])
}

fn session_permission_payload(permissions: BasePermissionProfile) -> Value {
    json!({ "permission": session_ruleset_for_profile(permissions) })
}

/// The fixed `opencode serve` argument vector: loopback host, OS-assigned port.
/// Exposed for tests.
#[must_use]
pub fn serve_args() -> Vec<String> {
    vec![
        "serve".to_string(),
        "--hostname".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        "0".to_string(),
    ]
}

/// A random server password for the loopback-only `opencode serve`. Not a
/// secret-grade RNG — it only fences a 127.0.0.1 server from a same-host
/// process that doesn't know the value; derived from time + pid + an address.
fn random_password() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = u128::from(std::process::id());
    let salt = std::ptr::addr_of!(nanos) as u128;
    let mut x = nanos ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt.rotate_left(17);
    x ^= x >> 33;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^= x >> 29;
    format!("{x:032x}")
}

/// Standard base64 (RFC 4648) of `input`. Avoids pulling a base64 crate for the
/// one place we need it (the Basic-auth header).
fn base64_encode(input: impl AsRef<[u8]>) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_ref();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn spawn_err(program: &str, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("`{program}` not found on PATH")
    } else {
        format!("failed to spawn `{program}`: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_json_response(stream: &mut std::net::TcpStream, body: &[u8]) {
        use std::io::Write as _;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    }

    // pure-unit (platform-independent)

    #[test]
    fn serve_args_request_loopback_ephemeral_port() {
        assert_eq!(
            serve_args(),
            vec![
                "serve".to_string(),
                "--hostname".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "0".to_string(),
            ]
        );
    }

    #[test]
    fn parse_listening_url_extracts_real_port() {
        // The exact line opencode prints (serve.ts).
        let line = "opencode server listening on http://127.0.0.1:54321";
        assert_eq!(
            parse_listening_url(line).as_deref(),
            Some("http://127.0.0.1:54321")
        );
        // Trailing punctuation / surrounding text is trimmed.
        assert_eq!(
            parse_listening_url("ready (http://127.0.0.1:8080).").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        // No url -> None.
        assert!(parse_listening_url("starting opencode serve...").is_none());
        assert!(parse_listening_url("http://").is_none());
    }

    #[tokio::test]
    async fn bounded_serve_reader_discards_oversize_and_recovers_at_next_line() {
        let bytes = b"0123456789\nlistening on http://127.0.0.1:7\r\nlast";
        let mut reader = BufReader::with_capacity(2, &bytes[..]);
        assert!(read_bounded_serve_line(&mut reader, 8).await.is_err());
        assert_eq!(
            read_bounded_serve_line(&mut reader, 64).await.unwrap(),
            Some(b"listening on http://127.0.0.1:7\r\n".to_vec())
        );
        assert_eq!(
            read_bounded_serve_line(&mut reader, 64).await.unwrap(),
            Some(b"last".to_vec())
        );
        assert!(read_bounded_serve_line(&mut reader, 64)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors + the actual `opencode:<pw>` shape.
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("f"), "Zg==");
        assert_eq!(base64_encode("fo"), "Zm8=");
        assert_eq!(base64_encode("foo"), "Zm9v");
        assert_eq!(base64_encode("foob"), "Zm9vYg==");
        assert_eq!(base64_encode("opencode:secret"), "b3BlbmNvZGU6c2VjcmV0");
    }

    #[test]
    fn split_provider_model_only_accepts_provider_slash_model() {
        assert_eq!(
            split_provider_model("anthropic/claude-sonnet-4-5"),
            Some(("anthropic", "claude-sonnet-4-5"))
        );
        assert!(split_provider_model("claude-sonnet-4-6").is_none());
        assert!(split_provider_model("/model").is_none());
        assert!(split_provider_model("provider/").is_none());
    }

    #[test]
    fn request_ids_are_encoded_as_one_url_path_segment() {
        assert_eq!(encode_path_segment("ask_safe-1"), "ask_safe-1");
        assert_eq!(
            encode_path_segment("../question?x=1"),
            "..%2Fquestion%3Fx%3D1"
        );
    }

    #[test]
    fn http_ctx_percent_encodes_directory_for_ascii_header() {
        let ctx = HttpCtx::new(
            "http://127.0.0.1:1".to_string(),
            "pw",
            Path::new("/tmp/my proj/uni cafe"),
        );
        // Spaces -> %XX; path separators preserved.
        assert!(ctx.directory.starts_with("/tmp/my%20proj/"));
        assert!(!ctx.directory.contains(' '));
        assert!(ctx.directory.is_ascii());
        // Auth header is Basic base64("opencode:pw").
        assert_eq!(ctx.auth, format!("Basic {}", base64_encode("opencode:pw")));
    }

    #[test]
    fn random_password_is_nonempty_hex() {
        let p = random_password();
        assert_eq!(p.len(), 32);
        assert!(p.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // frame translation (the SSE -> event core)

    #[test]
    fn translate_session_updated_yields_resolved_session_model_once() {
        let frame = serde_json::json!({
            "id": "evt_model",
            "type": "session.updated",
            "properties": {
                "sessionID": "ses_abc",
                "info": {
                    "id": "ses_abc",
                    "model": {
                        "providerID": "anthropic",
                        "id": "claude-sonnet-4-5",
                        "variant": "high"
                    }
                }
            }
        });
        let payload = serde_json::to_string(&frame).unwrap();
        let mut tracker = PartTracker::default();
        assert_eq!(
            translate_frame_tracked(&payload, "ses_abc", &mut tracker),
            vec![SessionEvent::SessionModel(
                "anthropic/claude-sonnet-4-5/high".to_string()
            )]
        );
        assert!(
            translate_frame_tracked(&payload, "ses_abc", &mut tracker).is_empty(),
            "duplicate session model reports should be idempotent"
        );
        assert!(translate_frame(&payload, "other_session").is_empty());
    }

    fn assistant_usage_frame(session_id: &str, message_id: &str, tokens: &Value) -> String {
        json!({
            "id": format!("evt_{message_id}"),
            "type": "message.updated",
            "properties": {
                "sessionID": session_id,
                "info": {
                    "id": message_id,
                    "sessionID": session_id,
                    "role": "assistant",
                    "tokens": tokens
                }
            }
        })
        .to_string()
    }

    fn idle_frame(session_id: &str) -> String {
        json!({
            "type": "session.status",
            "properties": {"sessionID": session_id, "status": {"type": "idle"}}
        })
        .to_string()
    }

    #[test]
    fn assistant_message_usage_is_exact_per_turn_and_does_not_double_count_updates() {
        let usage_frame = |message_id: &str,
                           input: u64,
                           output: u64,
                           reasoning: u64,
                           cache_read: u64,
                           cache_write: Value| {
            assistant_usage_frame(
                "ses_abc",
                message_id,
                &json!({
                    "input": input, "output": output, "reasoning": reasoning,
                    "cache": {"read": cache_read, "write": cache_write}
                }),
            )
        };
        let idle = idle_frame("ses_abc");

        let mut tracker = PartTracker::default();
        tracker.begin_turn();
        // `message.updated` is cumulative for one assistant message. The second
        // msg_1 frame replaces the first; it must not be added to it.
        assert!(translate_frame_tracked(
            &usage_frame("msg_1", 100, 10, 2, 20, json!(3)),
            "ses_abc",
            &mut tracker,
        )
        .is_empty());
        assert!(translate_frame_tracked(
            &usage_frame("msg_1", 120, 12, 3, 25, json!(5)),
            "ses_abc",
            &mut tracker,
        )
        .is_empty());
        // A distinct assistant message is a second model call in the same user
        // turn (for example after a tool result), so its usage is added once.
        assert!(translate_frame_tracked(
            &usage_frame("msg_2", 40, 4, 1, 10, json!(0)),
            "ses_abc",
            &mut tracker,
        )
        .is_empty());
        // A malformed later update cannot erase msg_2's last valid exact value.
        assert!(translate_frame_tracked(
            &usage_frame("msg_2", 99, 9, 1, 1, Value::Null),
            "ses_abc",
            &mut tracker,
        )
        .is_empty());

        assert_eq!(
            translate_frame_tracked(&idle, "ses_abc", &mut tracker),
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                // msg_1: (120+25+5, 12+3); msg_2: (40+10, 4+1).
                usage: Some(Usage {
                    cached_read_tokens: 35,
                    cached_write_tokens: 5,
                    reasoning_tokens: 4,
                    ..Usage::exact(200, 20)
                }),
            }]
        );
        assert_eq!(
            translate_frame_tracked(&idle, "ses_abc", &mut tracker),
            vec![SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }],
            "idle drains the turn accumulator; usage cannot leak into a later turn"
        );

        tracker.begin_turn();
        let _ = translate_frame_tracked(
            &usage_frame("msg_1", 7, 3, 4, 1, json!(2)),
            "ses_abc",
            &mut tracker,
        );
        assert!(matches!(
            translate_frame_tracked(&idle, "ses_abc", &mut tracker).as_slice(),
            [SessionEvent::TurnDone {
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 7,
                    cached_read_tokens: 1,
                    cached_write_tokens: 2,
                    reasoning_tokens: 4,
                    ..
                }),
                ..
            }]
        ));
    }

    #[test]
    fn usage_capture_ignores_replay_off_session_and_malformed_protocol_frames() {
        let exact = json!({
            "input": 8, "output": 3, "reasoning": 2,
            "cache": {"read": 4, "write": 1}
        });
        let mut tracker = PartTracker::default();

        // Startup/history replay arrives while no locally armed turn is active.
        let _ = translate_frame_tracked_for_turn(
            &assistant_usage_frame("ses_abc", "old", &exact),
            "ses_abc",
            &mut tracker,
            false,
        );
        // A sibling session must never enter this session's accumulator.
        let _ = translate_frame_tracked_for_turn(
            &assistant_usage_frame("ses_other", "other", &exact),
            "ses_abc",
            &mut tracker,
            true,
        );
        // Official fields are all required for exact accounting. A fractional,
        // negative, or missing cache member is not guessed.
        for tokens in [
            json!({"input":1.5,"output":2,"reasoning":0,"cache":{"read":0,"write":0}}),
            json!({"input":1,"output":-2,"reasoning":0,"cache":{"read":0,"write":0}}),
            json!({"input":1,"output":2,"reasoning":0,"cache":{"read":0}}),
            json!({"input":1,"output":2,"reasoning":0,"cache":{"read":0,"write":0},"total":99}),
            json!({"input":18_446_744_073_709_551_615_u64,"output":2,"reasoning":0,"cache":{"read":1,"write":0}}),
        ] {
            let _ = translate_frame_tracked_for_turn(
                &assistant_usage_frame("ses_abc", "bad", &tokens),
                "ses_abc",
                &mut tracker,
                true,
            );
        }
        assert!(tracker.take_turn_usage().is_none());
    }

    #[test]
    fn translate_tool_running_is_a_toolcall_with_input() {
        let frame = serde_json::json!({
            "id": "evt_1",
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "prt_1",
                    "sessionID": "ses_abc",
                    "messageID": "msg_1",
                    "type": "tool",
                    "callID": "call_1",
                    "tool": "write",
                    "state": {
                        "status": "running",
                        "input": { "filePath": "src/app.tsx", "content": "x" },
                        "time": { "start": 1 }
                    }
                }
            }
        })
        .to_string();
        let evs = translate_frame(&frame, "ses_abc");
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            SessionEvent::ToolCallCorrelated {
                call_id,
                name,
                input,
            } => {
                assert_eq!(call_id, "prt_1");
                // Normalized to the claude-shape the agent's diff/audit consumers
                // recognize: `write`→`Write`, `filePath`→`file_path`.
                assert_eq!(name, "Write");
                assert_eq!(
                    input.get("file_path").and_then(Value::as_str),
                    Some("src/app.tsx")
                );
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn opencode_tool_shape_is_normalized_to_claude_shape() {
        // An opencode edit must render a diff card + enter the audit/governance
        // trail, which requires the claude-shaped name + snake_case input keys.
        assert_eq!(normalize_tool_name("write"), "Write");
        assert_eq!(normalize_tool_name("edit"), "Edit");
        assert_eq!(normalize_tool_name("bash"), "Bash");
        assert_eq!(normalize_tool_name("customtool"), "Customtool");
        let n = normalize_tool_input(
            serde_json::json!({ "filePath": "a.ts", "oldString": "x", "newString": "y" }),
        );
        assert_eq!(n.get("file_path").and_then(Value::as_str), Some("a.ts"));
        assert_eq!(n.get("old_string").and_then(Value::as_str), Some("x"));
        assert_eq!(n.get("new_string").and_then(Value::as_str), Some("y"));
        assert!(n.get("filePath").is_none(), "camelCase key renamed away");
    }

    #[test]
    fn cumulative_text_part_emits_only_the_new_suffix() {
        // opencode resends the WHOLE accumulated text of a part on every update.
        // With a persistent tracker, each update must forward only its new suffix —
        // otherwise the consumer's append duplicates ('H','He','Hel',…).
        let mut tracker = PartTracker::default();
        let part = |text: &str| {
            serde_json::json!({
                "type": "message.part.updated",
                "properties": { "part": {
                    "id": "prt_1", "sessionID": "ses_abc", "type": "text", "text": text
                }}
            })
            .to_string()
        };
        let e1 = translate_frame_tracked(&part("Hello"), "ses_abc", &mut tracker);
        assert_eq!(e1, vec![SessionEvent::TextDelta("Hello".to_string())]);
        // Next update carries the FULL text again ("Hello world") → only " world".
        let e2 = translate_frame_tracked(&part("Hello world"), "ses_abc", &mut tracker);
        assert_eq!(e2, vec![SessionEvent::TextDelta(" world".to_string())]);
        // No growth → nothing emitted (not a duplicate).
        let e3 = translate_frame_tracked(&part("Hello world"), "ses_abc", &mut tracker);
        assert!(e3.is_empty(), "no new text → no delta: {e3:?}");
        // Reassembling the suffixes equals the final cumulative text.
        let joined: String = [e1, e2, e3]
            .concat()
            .into_iter()
            .map(|e| match e {
                SessionEvent::TextDelta(t) => t,
                _ => String::new(),
            })
            .collect();
        assert_eq!(joined, "Hello world");
    }

    #[test]
    fn user_message_parts_never_become_assistant_text() {
        let mut tracker = PartTracker::default();
        let message = json!({
            "type": "message.updated",
            "properties": {
                "sessionID": "ses_abc",
                "info": {
                    "id": "msg_user",
                    "sessionID": "ses_abc",
                    "role": "user"
                }
            }
        })
        .to_string();
        assert!(translate_frame_tracked(&message, "ses_abc", &mut tracker).is_empty());

        let echoed_prompt = json!({
            "type": "message.part.updated",
            "properties": { "part": {
                "id": "prt_user",
                "messageID": "msg_user",
                "sessionID": "ses_abc",
                "type": "text",
                "text": "the user's prompt"
            }}
        })
        .to_string();
        assert!(
            translate_frame_tracked(&echoed_prompt, "ses_abc", &mut tracker).is_empty(),
            "a user-authored part must not be projected as assistant output"
        );
    }

    #[test]
    fn modern_text_requires_explicit_assistant_attribution() {
        let mut tracker = PartTracker::default();
        let part = json!({
            "type": "message.part.updated",
            "properties": { "part": {
                "id": "prt_assistant",
                "messageID": "msg_assistant",
                "sessionID": "ses_abc",
                "type": "text",
                "text": "model reply"
            }}
        })
        .to_string();
        assert!(
            translate_frame_tracked(&part, "ses_abc", &mut tracker).is_empty(),
            "an unattributed modern part must fail closed"
        );

        let message = json!({
            "type": "message.updated",
            "properties": {
                "sessionID": "ses_abc",
                "info": {
                    "id": "msg_assistant",
                    "sessionID": "ses_abc",
                    "role": "assistant"
                }
            }
        })
        .to_string();
        assert!(translate_frame_tracked(&message, "ses_abc", &mut tracker).is_empty());
        assert_eq!(
            translate_frame_tracked(&part, "ses_abc", &mut tracker),
            [SessionEvent::TextDelta("model reply".to_string())]
        );
    }

    #[test]
    fn cumulative_reasoning_part_emits_thinking_suffixes() {
        let mut tracker = PartTracker::default();
        let part = |text: &str| {
            json!({
                "type": "message.part.updated",
                "properties": { "part": {
                    "id": "prt_reason", "sessionID": "ses_abc",
                    "type": "reasoning", "text": text
                }}
            })
            .to_string()
        };
        assert_eq!(
            translate_frame_tracked(&part("Inspect"), "ses_abc", &mut tracker),
            [SessionEvent::ThinkingDelta("Inspect".to_string())]
        );
        assert_eq!(
            translate_frame_tracked(&part("Inspect files"), "ses_abc", &mut tracker),
            [SessionEvent::ThinkingDelta(" files".to_string())]
        );
        assert!(
            translate_frame_tracked(&part("Inspect files"), "ses_abc", &mut tracker).is_empty()
        );
    }

    #[test]
    fn translate_tool_completed_and_error_carry_a_toolresult() {
        // A terminal tool frame always yields a ToolResult (the LAST event). With a
        // fresh tracker (no prior `running`), F6 ALSO back-fills a leading ToolCall —
        // asserted separately below; here we lock the result event itself.
        let done = serde_json::json!({
            "type": "message.part.updated",
            "properties": { "part": {
                "sessionID": "ses_abc", "type": "tool", "tool": "bash",
                "state": { "status": "completed", "input": {}, "output": "ok", "title": "ran npm test" }
            }}
        }).to_string();
        match translate_frame(&done, "ses_abc").last() {
            Some(SessionEvent::ToolResult { ok, summary }) => {
                assert!(ok);
                assert_eq!(summary, "ran npm test");
            }
            other => panic!("expected ok ToolResult last, got {other:?}"),
        }

        let err = serde_json::json!({
            "type": "message.part.updated",
            "properties": { "part": {
                "sessionID": "ses_abc", "type": "tool", "tool": "bash",
                "state": { "status": "error", "input": {}, "error": "exit 1" }
            }}
        })
        .to_string();
        match translate_frame(&err, "ses_abc").last() {
            Some(SessionEvent::ToolResult { ok, summary }) => {
                assert!(!ok);
                assert_eq!(summary, "exit 1");
            }
            other => panic!("expected failed ToolResult last, got {other:?}"),
        }
    }

    #[test]
    fn running_then_completed_emits_one_toolcall_then_a_result() {
        // The NORMAL path: a tool streams `running → completed`. The ToolCall fires
        // on `running`; the `completed` frame must NOT re-emit it (the back-fill is
        // suppressed because this part already emitted a ToolCall). F6 regression.
        let mut tracker = PartTracker::default();
        let frame = |status: &str| {
            serde_json::json!({
                "type": "message.part.updated",
                "properties": { "part": {
                    "id": "prt_w", "sessionID": "ses_abc", "type": "tool", "tool": "write",
                    "state": {
                        "status": status,
                        "input": { "filePath": "src/app.ts", "content": "export const x = 1;" },
                        "title": "wrote src/app.ts"
                    }
                }}
            })
            .to_string()
        };
        let running = translate_frame_tracked(&frame("running"), "ses_abc", &mut tracker);
        assert_eq!(
            running.len(),
            1,
            "running → exactly one ToolCall: {running:?}"
        );
        match &running[0] {
            SessionEvent::ToolCallCorrelated {
                call_id,
                name,
                input,
            } => {
                assert_eq!(call_id, "prt_w");
                assert_eq!(name, "Write");
                // camelCase input key was normalized to snake_case for the consumer.
                assert_eq!(input["file_path"], "src/app.ts");
            }
            other => panic!("expected a Write ToolCall, got {other:?}"),
        }
        // The completion now yields ONLY a ToolResult (no duplicate ToolCall).
        let completed = translate_frame_tracked(&frame("completed"), "ses_abc", &mut tracker);
        assert_eq!(
            completed,
            vec![SessionEvent::ToolResultCorrelated {
                call_id: "prt_w".to_string(),
                ok: true,
                summary: "wrote src/app.ts".to_string()
            }],
            "completed after running must not re-emit the ToolCall"
        );
    }

    #[test]
    fn parallel_tool_results_keep_part_ids_when_they_finish_out_of_order() {
        let mut tracker = PartTracker::default();
        let frame = |id: &str, status: &str, summary: &str| {
            serde_json::json!({
                "type":"message.part.updated",
                "properties":{"part":{
                    "id":id,
                    "sessionID":"ses_abc",
                    "type":"tool",
                    "tool":"bash",
                    "state":{
                        "status":status,
                        "input":{"command":format!("run {id}")},
                        "title":summary
                    }
                }}
            })
            .to_string()
        };

        let mut events = Vec::new();
        events.extend(translate_frame_tracked(
            &frame("part-A", "running", ""),
            "ses_abc",
            &mut tracker,
        ));
        events.extend(translate_frame_tracked(
            &frame("part-B", "running", ""),
            "ses_abc",
            &mut tracker,
        ));
        events.extend(translate_frame_tracked(
            &frame("part-B", "completed", "result B"),
            "ses_abc",
            &mut tracker,
        ));
        events.extend(translate_frame_tracked(
            &frame("part-A", "completed", "result A"),
            "ses_abc",
            &mut tracker,
        ));

        assert!(matches!(
            events.as_slice(),
            [
                SessionEvent::ToolCallCorrelated { call_id: a, .. },
                SessionEvent::ToolCallCorrelated { call_id: b, .. },
                SessionEvent::ToolResultCorrelated { call_id: rb, summary: sb, .. },
                SessionEvent::ToolResultCorrelated { call_id: ra, summary: sa, .. }
            ] if a == "part-A"
                && b == "part-B"
                && rb == "part-B"
                && sb == "result B"
                && ra == "part-A"
                && sa == "result A"
        ));

        let mut activity = umadev_runtime::ToolActivity::default();
        assert!(activity.observe(&events[0]));
        assert!(activity.observe(&events[1]));
        assert!(
            activity.observe(&events[2]),
            "finishing B must leave A active"
        );
        assert!(
            !activity.observe(&events[3]),
            "A then settles independently"
        );
    }

    #[test]
    fn merged_tool_frame_backfills_a_toolcall_so_the_write_is_audited() {
        // F6: a fast / SSE-merged tool jumps `pending → completed` with NO standalone
        // `running` frame. The old code emitted ONLY a ToolResult, so the write never
        // entered the audit trail and rendered no tool row / diff. Now the terminal
        // frame BACK-FILLS the ToolCall (normalized name + input) before the result.
        let mut tracker = PartTracker::default();
        let merged = serde_json::json!({
            "type": "message.part.updated",
            "properties": { "part": {
                "id": "prt_m", "sessionID": "ses_abc", "type": "tool", "tool": "write",
                "state": {
                    "status": "completed",
                    "input": { "filePath": "src/new.ts", "content": "export const y = 2;" },
                    "title": "wrote src/new.ts"
                }
            }}
        })
        .to_string();
        let evs = translate_frame_tracked(&merged, "ses_abc", &mut tracker);
        assert_eq!(evs.len(), 2, "merged tool → ToolCall + ToolResult: {evs:?}");
        match &evs[0] {
            SessionEvent::ToolCallCorrelated {
                call_id,
                name,
                input,
            } => {
                assert_eq!(call_id, "prt_m");
                assert_eq!(name, "Write", "back-filled call uses the normalized name");
                assert_eq!(
                    input["file_path"], "src/new.ts",
                    "back-filled call carries the normalized input (so audit + diff work)"
                );
            }
            other => panic!("expected a back-filled Write ToolCall first, got {other:?}"),
        }
        assert!(
            matches!(&evs[1], SessionEvent::ToolResultCorrelated { call_id, ok: true, .. } if call_id == "prt_m"),
            "the ToolResult still follows: {:?}",
            evs[1]
        );
        let replay = translate_frame_tracked(&merged, "ses_abc", &mut tracker);
        assert!(
            replay.is_empty(),
            "a replayed terminal frame is the same tool execution, not a second result: {replay:?}"
        );
    }

    #[test]
    fn merged_tool_frame_with_no_input_degrades_to_result_only() {
        // Fail-open: a terminal frame with NO recoverable input can't be a faithful
        // ToolCall → just the ToolResult (exactly as before F6), never a bogus call.
        let mut tracker = PartTracker::default();
        let no_input = serde_json::json!({
            "type": "message.part.updated",
            "properties": { "part": {
                "id": "prt_n", "sessionID": "ses_abc", "type": "tool", "tool": "bash",
                "state": { "status": "completed", "title": "ok" }
            }}
        })
        .to_string();
        let evs = translate_frame_tracked(&no_input, "ses_abc", &mut tracker);
        assert_eq!(
            evs,
            vec![SessionEvent::ToolResultCorrelated {
                call_id: "prt_n".to_string(),
                ok: true,
                summary: "ok".to_string()
            }],
            "no input → result only (no spurious ToolCall): {evs:?}"
        );
    }

    #[test]
    fn translate_text_part_is_a_textdelta() {
        let frame = serde_json::json!({
            "type": "message.part.updated",
            "properties": { "part": {
                "sessionID": "ses_abc", "type": "text", "text": "Here is the plan."
            }}
        })
        .to_string();
        match &translate_frame(&frame, "ses_abc")[0] {
            SessionEvent::TextDelta(t) => assert_eq!(t, "Here is the plan."),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn translate_idle_status_is_turndone_completed() {
        let frame = serde_json::json!({
            "type": "session.status",
            "properties": { "sessionID": "ses_abc", "status": { "type": "idle" } }
        })
        .to_string();
        match &translate_frame(&frame, "ses_abc")[0] {
            SessionEvent::TurnDone { status, .. } => assert_eq!(*status, TurnStatus::Completed),
            other => panic!("expected TurnDone(Completed), got {other:?}"),
        }
        // A `busy` status carries no turn semantics.
        let busy = serde_json::json!({
            "type": "session.status",
            "properties": { "sessionID": "ses_abc", "status": { "type": "busy" } }
        })
        .to_string();
        assert!(translate_frame(&busy, "ses_abc").is_empty());
    }

    #[test]
    fn idle_terminal_requires_busy_after_arm_and_is_consumed_once() {
        let gate: TurnSseGate = Arc::new(AtomicU8::new(SSE_TURN_UNARMED));
        let done = SessionEvent::TurnDone {
            status: TurnStatus::Completed,
            usage: None,
        };
        assert!(!terminal_event_belongs_to_armed_turn(&done, &gate));
        gate.store(SSE_TURN_ARMED, Ordering::Release);
        assert!(
            !terminal_event_belongs_to_armed_turn(&done, &gate),
            "idle before the official busy edge is not turn completion"
        );
        assert_eq!(gate.load(Ordering::Acquire), SSE_TURN_ARMED);
        gate.store(SSE_TURN_ACTIVE, Ordering::Release);
        assert!(terminal_event_belongs_to_armed_turn(&done, &gate));
        assert!(
            !terminal_event_belongs_to_armed_turn(&done, &gate),
            "a duplicated or delayed idle cannot complete the next turn"
        );
        assert!(terminal_event_belongs_to_armed_turn(
            &SessionEvent::TextDelta("progress".to_string()),
            &gate
        ));
    }

    #[test]
    fn bounded_sse_decoder_handles_half_crlf_multiline_unknown_and_oversize() {
        let mut decoder = BoundedSseDecoder::with_limits(64, 64);
        assert!(decoder.push(b"event: message\r\nda").unwrap().is_empty());
        let payloads = decoder
            .push(b"ta: {\"a\":1,\r\ndata: \"b\":2}\r\n\r\n")
            .unwrap();
        assert_eq!(payloads, vec!["{\"a\":1,\n\"b\":2}"]);

        let mut decoder = BoundedSseDecoder::with_limits(8, 64);
        assert!(decoder.push(b"012345678\n").is_err());

        let mut decoder = BoundedSseDecoder::with_limits(64, 8);
        assert!(decoder.push(b"data: 12345\ndata: 67890\n").is_err());
    }

    #[test]
    fn child_lifecycle_waits_for_every_child_and_normalizes_edges() {
        let mut lifecycle = ChildLifecycle::default();
        let children = ["child-a", "child-b"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let statuses = [
            ("child-a".to_string(), "busy".to_string()),
            ("child-b".to_string(), "retry".to_string()),
        ]
        .into_iter()
        .collect();
        let first = lifecycle.apply_snapshot(children, &statuses);
        assert_eq!(lifecycle.live.len(), 2);
        assert!(first.iter().any(|event| matches!(
            event,
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Live { agent_ids })
                if agent_ids == &["child-a".to_string(), "child-b".to_string()]
        )));

        let children = ["child-a", "child-b"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let statuses = [("child-b".to_string(), "busy".to_string())]
            .into_iter()
            .collect();
        let second = lifecycle.apply_snapshot(children, &statuses);
        assert!(second.iter().any(|event| matches!(
            event,
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished { id })
                if id == "child-a"
        )));
        assert_eq!(
            lifecycle.live,
            ["child-b".to_string()].into_iter().collect()
        );

        let children = ["child-a", "child-b"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let third = lifecycle.apply_snapshot(children, &BTreeMap::new());
        assert!(third.iter().any(|event| matches!(
            event,
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished { id })
                if id == "child-b"
        )));
        assert!(lifecycle.live.is_empty());
    }

    #[tokio::test]
    async fn child_reconciliation_service_failure_is_bounded_and_explicit() {
        let http = HttpCtx::new_with_timeout(
            "http://127.0.0.1:1".to_string(),
            "pw",
            Path::new("/proj"),
            Duration::from_millis(50),
        );
        let (tx, _rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let lifecycle = Arc::new(Mutex::new(ChildLifecycle::default()));
        let result = settle_children(
            &http,
            "parent",
            &tx,
            &lifecycle,
            ChildSettleConfig {
                timeout: Duration::from_millis(200),
                poll: Duration::from_millis(1),
                request_timeout: Duration::from_millis(50),
                max_errors: 2,
            },
        )
        .await;
        let error = result.expect_err("an unavailable server must be an explicit failure");
        assert!(error.contains("reconciliation failed") || error.contains("timed out"));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn child_reconciliation_keeps_sse_and_approvals_live_until_one_turn_done() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status_calls = Arc::new(AtomicUsize::new(0));
        let child_idle = Arc::new(AtomicBool::new(false));
        let calls = status_calls.clone();
        let idle = child_idle.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 4096];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let line = request.lines().next().unwrap_or("");
                let body: &[u8] = if line.starts_with("GET /session/parent/children") {
                    br#"[{"id":"child-a"}]"#
                } else if line.starts_with("GET /session/child-a/children") {
                    br#"[{"id":"grandchild-a"}]"#
                } else if line.starts_with("GET /session/grandchild-a/children") {
                    b"[]"
                } else if line.starts_with("GET /session/status") {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if idle.load(Ordering::SeqCst) {
                        b"{}"
                    } else {
                        br#"{"child-a":{"type":"busy"},"grandchild-a":{"type":"busy"}}"#
                    }
                } else {
                    b"{}"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, response.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, body).await;
            }
        });

        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        let (tx, mut rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let lifecycle = Arc::new(Mutex::new(ChildLifecycle::default()));
        let settle_state = Arc::new(AtomicU8::new(0));
        let turn_sse_gate: TurnSseGate = Arc::new(AtomicU8::new(SSE_TURN_ACTIVE));
        let created = json!({
            "type": "session.created",
            "properties": { "info": { "id": "child-a", "parentID": "parent" } }
        })
        .to_string();
        let started = lifecycle.lock().await.observe_sse(&created, "parent");
        assert!(send_events(&tx, started).await);
        assert!(
            forward_open_code_events(
                &http,
                "parent",
                &tx,
                &lifecycle,
                &settle_state,
                &turn_sse_gate,
                vec![SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: Some(Usage::exact(321, 45)),
                }],
            )
            .await
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while status_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached reconciler polled while SSE stayed available");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if lifecycle.lock().await.known.contains("grandchild-a") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bounded HTTP traversal recovers an SSE-missed grandchild");

        let permission = json!({
            "type": "permission.asked",
            "properties": {
                "id": "per-child", "sessionID": "child-a",
                "permission": "bash", "patterns": ["cargo test"]
            }
        })
        .to_string();
        let approval = lifecycle.lock().await.observe_sse(&permission, "parent");
        assert!(send_events(&tx, approval).await);
        assert!(
            forward_open_code_events(
                &http,
                "parent",
                &tx,
                &lifecycle,
                &settle_state,
                &turn_sse_gate,
                vec![
                    SessionEvent::TextDelta("parent progress".to_string()),
                    SessionEvent::ToolCall {
                        name: "Read".to_string(),
                        input: json!({"file_path": "README.md"}),
                    },
                ],
            )
            .await
        );

        let mut before_idle = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !(before_idle.iter().any(|event| {
                matches!(
                    event,
                    SessionEvent::HostRequest { req_id, request: HostRequest::Approval { .. } }
                        if req_id == "per-child"
                )
            }) && before_idle.iter().any(|event| {
                matches!(
                    event,
                    SessionEvent::TextDelta(text) if text == "parent progress"
                )
            }) && before_idle.iter().any(|event| {
                matches!(
                    event,
                    SessionEvent::ToolCall { name, .. } if name == "Read"
                )
            })) {
                before_idle.push(rx.recv().await.expect("channel remains open"));
            }
        })
        .await
        .expect("approval, parent text, and parent tool remain live during settle");
        assert!(before_idle.iter().any(|event| matches!(
            event,
            SessionEvent::HostRequest { req_id, request: HostRequest::Approval { .. } }
                if req_id == "per-child"
        )));
        assert!(before_idle.iter().any(
            |event| matches!(event, SessionEvent::TextDelta(text) if text == "parent progress")
        ));
        assert!(before_idle
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCall { name, .. } if name == "Read")));
        assert!(!before_idle
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnDone { .. })));

        child_idle.store(true, Ordering::SeqCst);
        let mut done = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("child terminal reconciliation completes")
                .expect("channel remains open");
            let terminal = matches!(event, SessionEvent::TurnDone { .. });
            done.push(event);
            if terminal {
                break;
            }
        }
        assert!(done.iter().any(|event| matches!(
            event,
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished { id }) if id == "child-a"
        )));
        assert!(before_idle.iter().any(|event| matches!(
            event,
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Started { id }) if id == "grandchild-a"
        )) || done.iter().any(|event| matches!(
            event,
            SessionEvent::BackgroundTask(BackgroundTaskSignal::Started { id }) if id == "grandchild-a"
        )));
        assert_eq!(
            done.iter()
                .filter(|event| matches!(event, SessionEvent::TurnDone { .. }))
                .count(),
            1
        );
        assert!(
            done.iter().any(|event| matches!(
                event,
                SessionEvent::TurnDone {
                    usage: Some(Usage {
                        input_tokens: 321,
                        output_tokens: 45,
                        ..
                    }),
                    ..
                }
            )),
            "child reconciliation must preserve the parent turn's exact usage: {done:?}"
        );
        assert_eq!(settle_state.load(Ordering::SeqCst), 2);
        assert!(lifecycle.lock().await.live.is_empty());
        server.abort();
    }

    #[test]
    fn translate_permission_asked_is_typed_approval() {
        let frame = serde_json::json!({
            "type": "permission.asked",
            "properties": {
                "id": "per_xyz", "sessionID": "ses_abc",
                "permission": "bash", "patterns": ["rm -rf *", "curl *"],
                "metadata": {}, "always": []
            }
        })
        .to_string();
        match &translate_frame(&frame, "ses_abc")[0] {
            SessionEvent::HostRequest {
                req_id,
                request:
                    HostRequest::Approval {
                        action,
                        target,
                        options,
                        ..
                    },
            } => {
                assert_eq!(req_id, "per_xyz");
                assert_eq!(action, "bash");
                assert!(target.contains("rm -rf *"));
                assert_eq!(
                    options
                        .iter()
                        .map(|option| option.id.as_str())
                        .collect::<Vec<_>>(),
                    ["once", "always", "reject"]
                );
            }
            other => panic!("expected typed approval, got {other:?}"),
        }
    }

    #[test]
    fn translate_question_asked_preserves_order_choices_and_custom_answers() {
        let frame = json!({
            "type": "question.asked",
            "properties": {
                "id": "ask_7", "sessionID": "ses_abc",
                "questions": [
                    {
                        "header": "Database", "question": "Pick one",
                        "options": [
                            {"label": "SQLite", "description": "Local"},
                            {"label": "Postgres", "description": "Server"}
                        ],
                        "multiple": false, "custom": false
                    },
                    {
                        "header": "Checks", "question": "Pick checks",
                        "options": [{"label": "Lint", "description": "Fast"}],
                        "multiple": true, "custom": true
                    }
                ],
                "tool": {"messageID": "msg_1", "callID": "call_1"}
            }
        })
        .to_string();
        match &translate_frame(&frame, "ses_abc")[0] {
            SessionEvent::HostRequest {
                req_id,
                request:
                    HostRequest::UserInput {
                        questions,
                        metadata,
                    },
            } => {
                assert_eq!(req_id, "ask_7");
                assert_eq!(questions.len(), 2);
                assert_eq!(questions[0].id, "ask_7:0");
                assert_eq!(questions[0].kind, HostQuestionKind::SingleChoice);
                assert_eq!(questions[0].options[1].value, "Postgres");
                assert_eq!(questions[1].id, "ask_7:1");
                assert_eq!(
                    questions[1].kind,
                    HostQuestionKind::Other("multi_choice_or_text".to_string())
                );
                assert_eq!(metadata["tool"]["callID"], "call_1");
                assert_eq!(metadata["custom_answers"], json!([false, true]));
            }
            other => panic!("expected typed user input, got {other:?}"),
        }
        assert!(translate_frame(&frame, "other").is_empty());
    }

    #[test]
    fn child_question_is_forwarded_as_typed_user_input() {
        let mut lifecycle = ChildLifecycle::default();
        let created = json!({
            "type": "session.created",
            "properties": {"info": {"id": "child", "parentID": "parent"}}
        })
        .to_string();
        let _ = lifecycle.observe_sse(&created, "parent");
        let question = json!({
            "type": "question.asked",
            "properties": {
                "id": "ask_child", "sessionID": "child",
                "questions": [{
                    "header": "Child", "question": "Continue?",
                    "options": [{"label": "Yes", "description": "Continue"}],
                    "multiple": false, "custom": false
                }]
            }
        })
        .to_string();
        let events = lifecycle.observe_sse(&question, "parent");
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::HostRequest {
                req_id,
                request: HostRequest::UserInput { .. }
            }] if req_id == "ask_child"
        ));
    }

    #[test]
    fn translate_session_error_is_turndone_failed() {
        let frame = serde_json::json!({
            "type": "session.error",
            "properties": { "sessionID": "ses_abc",
                "error": { "name": "ProviderError", "data": { "message": "rate limited" } } }
        })
        .to_string();
        match &translate_frame(&frame, "ses_abc")[0] {
            SessionEvent::TurnDone { status, .. } => {
                assert_eq!(*status, TurnStatus::Failed("rate limited".to_string()));
            }
            other => panic!("expected TurnDone(Failed), got {other:?}"),
        }
    }

    #[test]
    fn translate_ignores_off_session_and_liveness_frames() {
        // A part for a DIFFERENT session is dropped (multi-session isolation).
        let other_session = serde_json::json!({
            "type": "message.part.updated",
            "properties": { "part": { "sessionID": "ses_OTHER", "type": "text", "text": "hi" } }
        })
        .to_string();
        assert!(translate_frame(&other_session, "ses_abc").is_empty());
        // Liveness frames carry no turn semantics.
        for t in ["server.connected", "server.heartbeat", "message.updated"] {
            let f = serde_json::json!({ "type": t, "properties": {} }).to_string();
            assert!(translate_frame(&f, "ses_abc").is_empty());
        }
        // Garbage payload -> nothing (fail-open).
        assert!(translate_frame("not json", "ses_abc").is_empty());
        assert!(translate_frame("", "ses_abc").is_empty());
    }

    // end-to-end against a fake HTTP+SSE server.
    // A handwritten loopback HTTP/1.1 server (no extra deps) stands in for
    // `opencode serve`: it answers POST /session, streams the SSE /event frames
    // (tool running -> completed -> idle), and accepts prompt_async. This drives
    // the WHOLE OpenCodeSession path — handshake, injection, SSE parsing, idle
    // boundary — without a real opencode binary.

    #[cfg(unix)]
    #[tokio::test]
    async fn full_session_handshake_inject_and_idle_boundary() {
        use std::io::Write as _;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // The fake server: one connection per request (close after each).
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let n = match tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let request_line = req.lines().next().unwrap_or("").to_string();
                let std_sock = sock.into_std().unwrap();
                std_sock.set_nonblocking(false).unwrap();
                let mut s = std_sock;

                if request_line.starts_with("POST /session ") {
                    // Must carry the Basic auth + directory header (reqwest
                    // emits header names lowercased on the HTTP/1.1 wire).
                    let lower = req.to_ascii_lowercase();
                    assert!(lower.contains("authorization: basic "));
                    assert!(lower.contains("x-opencode-directory:"));
                    let body = br#"{"id":"ses_fake","title":"t","directory":"/x"}"#;
                    write_json_response(&mut s, body);
                } else if request_line.starts_with("GET /event") {
                    // Stream SSE frames: tool running, then completed, then idle.
                    s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                    let frames = [
                        r#"{"id":"e1","type":"server.connected","properties":{}}"#,
                        r#"{"id":"e2","type":"message.part.updated","properties":{"part":{"sessionID":"ses_fake","type":"tool","tool":"write","state":{"status":"running","input":{"filePath":"src/x.ts"},"time":{"start":1}}}}}"#,
                        r#"{"id":"e3","type":"message.part.updated","properties":{"part":{"sessionID":"ses_fake","type":"tool","tool":"write","state":{"status":"completed","input":{"filePath":"src/x.ts"},"output":"done","title":"wrote src/x.ts","metadata":{},"time":{"start":1,"end":2}}}}}"#,
                        r#"{"id":"e4","type":"session.status","properties":{"sessionID":"ses_fake","status":{"type":"idle"}}}"#,
                    ];
                    for f in frames {
                        s.write_all(format!("event: message\r\ndata: {f}\r\n\r\n").as_bytes())
                            .unwrap();
                        s.flush().unwrap();
                    }
                    // Keep this SSE socket alive on another thread so the fake
                    // server can concurrently answer child/status reconciliation,
                    // matching the real HTTP server's connection concurrency.
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        drop(s);
                    });
                } else if request_line.starts_with("GET /session/ses_fake/children") {
                    write_json_response(&mut s, b"[]");
                } else if request_line.starts_with("GET /session/status") {
                    write_json_response(&mut s, b"{}");
                } else {
                    // prompt_async / abort / delete -> 204 NoContent.
                    s.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .unwrap();
                }
            }
        });

        // Build a session directly against the fake server (bypass the serve
        // spawn — that path is covered by the unix fake-sh port-parse test).
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        let session_id = http
            .create_session(Some("build"), None, true)
            .await
            .unwrap();
        assert_eq!(session_id, "ses_fake");

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        tokio::spawn(pump_sse(http.clone(), session_id.clone(), tx));

        // Inject a directive (prompt_async).
        http.prompt_async(&session_id, "build the thing", None)
            .await
            .unwrap();

        // Drain events until the idle TurnDone boundary.
        let mut got: Vec<SessionEvent> = Vec::new();
        let mut rx = rx;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            let done = matches!(ev, SessionEvent::TurnDone { .. });
            got.push(ev);
            if done {
                break;
            }
        }

        // We must have seen the tool call (the write-path truth), its result,
        // and a clean idle TurnDone.
        assert!(
            got.iter().any(|e| matches!(e, SessionEvent::ToolCall { name, input }
                if name == "Write" && input.get("file_path").and_then(Value::as_str) == Some("src/x.ts"))),
            "expected a ToolCall(Write src/x.ts): {got:?}"
        );
        assert!(
            got.iter()
                .any(|e| matches!(e, SessionEvent::ToolResult { ok: true, .. })),
            "expected an ok ToolResult: {got:?}"
        );
        assert!(
            matches!(got.last(), Some(SessionEvent::TurnDone { status, .. }) if *status == TurnStatus::Completed),
            "last event must be a clean idle TurnDone: {got:?}"
        );

        server.abort();
    }

    #[test]
    fn readonly_ruleset_is_an_exact_read_allowlist() {
        let rules = readonly_session_ruleset();
        let rules = rules.as_array().expect("ruleset array");
        let effective_action = |permission: &str| {
            rules
                .iter()
                .filter(|rule| {
                    matches!(rule["permission"].as_str(), Some("*"))
                        || rule["permission"].as_str() == Some(permission)
                })
                .filter_map(|rule| rule["action"].as_str())
                .next_back()
        };

        for permission in ["read", "grep", "glob", "list"] {
            assert_eq!(effective_action(permission), Some("allow"), "{permission}");
        }
        for permission in [
            "task",
            "patch",
            "edit",
            "write",
            "bash",
            "webfetch",
            "mcp",
            "mcp_database_write",
            "future_unknown_tool",
        ] {
            assert_eq!(effective_action(permission), Some("deny"), "{permission}");
        }
        assert!(
            !rules
                .iter()
                .any(|rule| rule["permission"] == "*" && rule["action"] == "allow"),
            "read-only forks must never have a wildcard allow floor: {rules:?}"
        );
    }

    #[tokio::test]
    async fn create_readonly_session_sends_allowlist_and_reuses_model() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                // The transmitted body is deny-by-default, contains the exact
                // local-read allowlist, and never hard-codes a build agent.
                let posted: Value = serde_json::from_str(
                    req.split_once("\r\n\r\n")
                        .map(|(_, body)| body)
                        .expect("request body"),
                )
                .expect("JSON request body");
                assert_eq!(posted["permission"], readonly_session_ruleset());
                assert!(
                    posted.get("agent").is_none(),
                    "fork must not force build: {posted}"
                );
                assert_eq!(
                    posted["model"],
                    json!({ "id": "claude-sonnet", "providerID": "anthropic" }),
                    "fork must reuse the explicitly selected writer model"
                );
                let body = br#"{"id":"ses_fork"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, body).await;
            }
        });
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        let id = http
            .create_readonly_session(Some("anthropic/claude-sonnet"))
            .await
            .expect("read-only session created");
        assert_eq!(id, "ses_fork");
        server.abort();
    }

    #[tokio::test]
    async fn readonly_fork_waits_until_sse_subscription_is_ready() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut create, _) = listener.accept().await.expect("create connection");
            let mut buf = vec![0u8; 8192];
            let _ = tokio::io::AsyncReadExt::read(&mut create, &mut buf).await;
            let body = br#"{"id":"ses_fork"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            tokio::io::AsyncWriteExt::write_all(&mut create, response.as_bytes())
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut create, body)
                .await
                .unwrap();

            let (mut stream, _) = listener.accept().await.expect("SSE connection");
            let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                .await
                .unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.starts_with("GET /event?"),
                "unexpected SSE request: {request}"
            );
            let _ = seen_tx.send(());
            let _ = release_rx.await;
            tokio::io::AsyncWriteExt::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
            std::future::pending::<()>().await;
        });

        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        let opening = tokio::spawn(async move { http.start_readonly_fork(None).await });
        tokio::time::timeout(Duration::from_secs(2), seen_rx)
            .await
            .expect("SSE request observed")
            .expect("SSE observer alive");
        assert!(
            !opening.is_finished(),
            "fork must not return before SSE response establishes the subscription"
        );
        let _ = release_tx.send(());
        let fork = tokio::time::timeout(Duration::from_secs(2), opening)
            .await
            .expect("fork ready")
            .expect("join succeeds")
            .expect("fork opens");
        drop(fork);
        server.abort();
    }

    #[test]
    fn session_ruleset_autonomous_allows_questions_but_not_plan_toggles() {
        let r = session_ruleset(true);
        let arr = r.as_array().expect("ruleset is an array");
        assert!(
            arr.iter()
                .any(|x| x["permission"] == "*" && x["action"] == "allow"),
            "autonomous keeps the broad allow floor: {arr:?}"
        );
        assert_eq!(
            effective_permission_action(&r, "question"),
            Some("allow"),
            "QuestionV1 is handled by the typed host channel"
        );
        for perm in ["plan_enter", "plan_exit"] {
            assert!(
                arr.iter()
                    .any(|x| x["permission"] == perm && x["action"] == "deny"),
                "autonomous must DENY the interactive prompt {perm}: {arr:?}"
            );
        }
        assert_eq!(
            arr.iter().filter(|x| x["action"] == "deny").count(),
            2,
            "autonomous denies only base-side plan toggles: {arr:?}"
        );
        for permission in ["patch", "future_write_tool", "mcp__filesystem__write_file"] {
            assert_eq!(
                effective_permission_action(&r, permission),
                Some("allow"),
                "Auto explicitly authorizes {permission} through its wildcard floor"
            );
        }
    }

    #[test]
    fn session_ruleset_guarded_is_read_allowlisted_and_ask_by_default() {
        // The Guarded tier is authorization-fail-closed: local inspection is
        // pre-authorized, while every potentially mutating, delegating, future, or
        // MCP-provided permission routes through `permission.asked`.
        let r = session_ruleset(false);
        let arr = r.as_array().expect("ruleset is an array");
        assert!(
            arr.iter()
                .any(|x| x["permission"] == "*" && x["action"] == "ask"),
            "guarded must keep an ask-by-default wildcard floor: {arr:?}"
        );
        assert!(
            !arr.iter()
                .any(|x| x["permission"] == "*" && x["action"] == "allow"),
            "guarded must never reintroduce a wildcard allow: {arr:?}"
        );

        for permission in ["read", "grep", "glob", "list"] {
            assert_eq!(
                effective_permission_action(&r, permission),
                Some("allow"),
                "known local inspection permission {permission} is safe to pre-authorize"
            );
        }

        for permission in [
            "edit",
            "write",
            "patch",
            "bash",
            "task",
            "future_write_tool",
            "mcp__filesystem__write_file",
        ] {
            assert_eq!(
                effective_permission_action(&r, permission),
                Some("ask"),
                "Guarded must ASK before {permission}"
            );
        }

        assert_eq!(
            effective_permission_action(&r, "question"),
            Some("allow"),
            "guarded questions use the typed host response channel"
        );
        for permission in ["plan_enter", "plan_exit"] {
            assert_eq!(
                effective_permission_action(&r, permission),
                Some("deny"),
                "non-interactive Guarded sessions must deny {permission}"
            );
        }
    }

    #[test]
    fn session_ruleset_plan_allows_only_local_inspection() {
        let rules = session_ruleset_for_profile(BasePermissionProfile::Plan);
        for permission in ["read", "grep", "glob", "list", "question"] {
            assert_eq!(
                effective_permission_action(&rules, permission),
                Some("allow"),
                "Plan permits local inspection/questions through {permission}"
            );
        }
        for permission in [
            "edit",
            "write",
            "patch",
            "bash",
            "task",
            "future_write_tool",
            "mcp__filesystem__write_file",
        ] {
            assert_eq!(
                effective_permission_action(&rules, permission),
                Some("deny"),
                "Plan must deny {permission} through its wildcard floor"
            );
        }
    }

    #[test]
    fn fresh_and_resume_share_the_exact_profile_payload_matrix() {
        for (profile, mutating_action) in [
            (BasePermissionProfile::Plan, "deny"),
            (BasePermissionProfile::Guarded, "ask"),
            (BasePermissionProfile::Auto, "allow"),
        ] {
            let payload = session_permission_payload(profile);
            assert_eq!(payload["permission"], session_ruleset_for_profile(profile));
            for permission in ["edit", "write", "bash", "future_mutator"] {
                assert_eq!(
                    effective_permission_action(&payload["permission"], permission),
                    Some(mutating_action),
                    "{profile:?} {permission}"
                );
            }
        }
    }

    /// Resolve the action for a permission when every rule uses the catch-all
    /// target pattern. OpenCode chooses a permission-specific rule over `*`; this
    /// helper makes the authorization behaviour tests read like the server outcome
    /// instead of merely asserting that individual JSON rows exist.
    fn effective_permission_action<'a>(rules: &'a Value, permission: &str) -> Option<&'a str> {
        let rules = rules.as_array()?;
        rules
            .iter()
            .rev()
            .find(|rule| rule["permission"] == permission && rule["pattern"] == "*")
            .or_else(|| {
                rules
                    .iter()
                    .rev()
                    .find(|rule| rule["permission"] == "*" && rule["pattern"] == "*")
            })?
            .get("action")?
            .as_str()
    }

    #[tokio::test]
    async fn json_requests_time_out_instead_of_hanging() {
        // Fix P1: the non-streaming JSON client carries a request timeout. A
        // server that accepts the TCP connection but NEVER responds must make the
        // call fail-open (Err) within the timeout, not hang start/send/end forever.
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept connections and hold them open forever without ever replying.
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            // Accept and hold every connection open forever without replying.
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let http = HttpCtx::new_with_timeout(
            format!("http://{addr}"),
            "pw",
            Path::new("/proj"),
            Duration::from_millis(300),
        );
        let started = tokio::time::Instant::now();
        let res = http.create_session(None, None, false).await;
        assert!(
            res.is_err(),
            "a never-responding server must fail, not hang"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the call must be bounded by the request timeout, not hang: {:?}",
            started.elapsed()
        );
        server.abort();
    }

    #[tokio::test]
    async fn fork_send_turn_resets_turn_active_on_send_error() {
        // Fix P2/P3: a failed `prompt_async` must clear `turn_active` so the
        // state machine stays honest. Point the fork at a refused port
        // (127.0.0.1:1) so the POST errors quickly.
        let (_tx, rx) = mpsc::channel(1);
        let http = HttpCtx::new_with_timeout(
            "http://127.0.0.1:1".to_string(),
            "pw",
            Path::new("/proj"),
            Duration::from_millis(300),
        );
        let mut fork = OpenCodeForkSession {
            http,
            session_id: "ses_x".to_string(),
            events: rx,
            sse_task: None,
            lifecycle: SessionLifecycle::Ephemeral,
            pending_interactions: HashMap::new(),
            turn_sse_gate: Arc::new(AtomicU8::new(SSE_TURN_UNARMED)),
            turn_active: false,
        };
        let res = fork.send_turn("hello".to_string()).await;
        assert!(res.is_err(), "send must fail against a refused port");
        assert!(
            !fork.turn_active,
            "turn_active must reset to false after a send failure"
        );
    }

    #[tokio::test]
    async fn fork_drop_aborts_its_sse_pump() {
        struct DropSignal(Option<oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let (started_tx, started_rx) = oneshot::channel();
        let (stopped_tx, stopped_rx) = oneshot::channel();
        let sse_task = tokio::spawn(async move {
            let _signal = DropSignal(Some(stopped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.expect("pump started");

        let (_tx, rx) = mpsc::channel(1);
        let fork = OpenCodeForkSession {
            http: HttpCtx::new_with_timeout(
                "http://127.0.0.1:1".to_string(),
                "pw",
                Path::new("/proj"),
                Duration::from_millis(100),
            ),
            session_id: "ses_drop".to_string(),
            events: rx,
            sse_task: Some(sse_task),
            lifecycle: SessionLifecycle::Ephemeral,
            pending_interactions: HashMap::new(),
            turn_sse_gate: Arc::new(AtomicU8::new(SSE_TURN_UNARMED)),
            turn_active: false,
        };
        drop(fork);
        tokio::time::timeout(Duration::from_secs(2), stopped_rx)
            .await
            .expect("aborted pump drops promptly")
            .expect("drop signal delivered");
    }

    #[tokio::test]
    async fn readonly_fork_rejects_even_an_allow_response() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("permission reply");
            let mut buf = vec![0u8; 4096];
            let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                .await
                .unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.contains("\"reply\":\"reject\""),
                "read-only fork must reject authority escalation: {request}"
            );
            assert!(!request.contains("\"reply\":\"once\""));
            tokio::io::AsyncWriteExt::write_all(
                &mut sock,
                b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        });
        let (_tx, rx) = mpsc::channel(1);
        let mut fork = OpenCodeForkSession {
            http: HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj")),
            session_id: "ses_readonly".to_string(),
            events: rx,
            sse_task: None,
            lifecycle: SessionLifecycle::Ephemeral,
            pending_interactions: HashMap::new(),
            turn_sse_gate: Arc::new(AtomicU8::new(SSE_TURN_UNARMED)),
            turn_active: false,
        };
        fork.respond("per_write", ApprovalDecision::Allow)
            .await
            .expect("reject reply succeeds");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn typed_responses_use_distinct_question_and_permission_routes() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for expected in ["question", "permission", "reject"] {
                let (mut sock, _) = listener.accept().await.expect("typed response");
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                match expected {
                    "question" => {
                        assert!(request.starts_with("POST /question/ask_1/reply "));
                        let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
                        assert_eq!(
                            serde_json::from_str::<Value>(body).unwrap(),
                            json!({"answers": [["first"], ["second", "custom"]]})
                        );
                    }
                    "permission" => {
                        assert!(request.starts_with("POST /permission/per_1/reply "));
                        assert!(request.contains("\"reply\":\"always\""));
                    }
                    _ => {
                        assert!(request.starts_with("POST /question/ask_bad/reject "));
                    }
                }
                tokio::io::AsyncWriteExt::write_all(
                    &mut sock,
                    b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            }
        });
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));

        respond_to_interaction(
            &http,
            "ask_1",
            PendingInteraction::Question {
                question_ids: vec!["ask_1:0".to_string(), "ask_1:1".to_string()],
            },
            HostResponse::UserInput {
                answers: vec![
                    HostAnswer {
                        question_id: "ask_1:1".to_string(),
                        values: vec!["second".to_string(), "custom".to_string()],
                    },
                    HostAnswer {
                        question_id: "ask_1:0".to_string(),
                        values: vec!["first".to_string()],
                    },
                ],
            },
        )
        .await
        .unwrap();
        respond_to_interaction(
            &http,
            "per_1",
            PendingInteraction::Permission,
            HostResponse::Approval {
                decision: ApprovalDecision::Allow,
                selected_option_id: Some("always".to_string()),
                message: None,
            },
        )
        .await
        .unwrap();
        respond_to_interaction(
            &http,
            "ask_bad",
            PendingInteraction::Question {
                question_ids: vec!["ask_bad:0".to_string()],
            },
            HostResponse::UserInput {
                answers: vec![HostAnswer {
                    question_id: "wrong".to_string(),
                    values: vec!["unsafe".to_string()],
                }],
            },
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_preserves_writer_and_deletes_only_ephemeral_fork() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));

        http.finish_session("ses_writer", SessionLifecycle::Persistent)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "persistent writer cleanup must not issue DELETE"
        );

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("fork DELETE");
            let mut buf = vec![0u8; 4096];
            let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                .await
                .unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("DELETE /session/ses_fork "));
            tokio::io::AsyncWriteExt::write_all(
                &mut sock,
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntrue",
            )
            .await
            .unwrap();
        });
        http.finish_session("ses_fork", SessionLifecycle::Ephemeral)
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn guarded_create_session_sends_ask_ruleset() {
        // End-to-end: a guarded (`autonomous = false`) create_session POSTs the
        // ask-by-default ruleset, so opencode will raise `permission.asked` for a
        // write, patch, shell, future tool, or MCP mutator — the same
        // human-in-the-loop posture codex gets from `on-request`.
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let posted: Value = serde_json::from_str(
                    req.split_once("\r\n\r\n")
                        .map(|(_, body)| body)
                        .expect("request body"),
                )
                .expect("JSON request body");
                assert_eq!(posted["permission"], session_ruleset(false));
                let body = br#"{"id":"ses_guarded"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, body).await;
            }
        });
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        let id = http
            .create_session(Some("build"), None, false)
            .await
            .expect("guarded session created");
        assert_eq!(id, "ses_guarded");
        server.abort();
    }

    #[tokio::test]
    async fn prompt_async_uses_prompt_model_shape_for_explicit_selection() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("prompt request");
            let mut buf = vec![0u8; 8192];
            let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                .await
                .unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("POST /session/ses_model/prompt_async "));
            let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
            let posted: Value = serde_json::from_str(body).unwrap();
            assert_eq!(
                posted["model"],
                json!({"providerID": "anthropic", "modelID": "claude-sonnet"})
            );
            assert!(posted["model"].get("id").is_none());
            tokio::io::AsyncWriteExt::write_all(
                &mut sock,
                b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        });
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        http.prompt_async("ses_model", "continue", Some("anthropic/claude-sonnet"))
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn successful_prompt_async_returns_an_exact_protocol_receipt() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("prompt request");
            let mut buf = vec![0_u8; 8192];
            let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                .await
                .unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("POST /session/ses_ack/prompt_async "));
            tokio::io::AsyncWriteExt::write_all(
                &mut sock,
                b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        });

        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        let (_tx, rx) = mpsc::channel(1);
        let mut session = OpenCodeForkSession {
            http,
            session_id: "ses_ack".to_string(),
            events: rx,
            sse_task: None,
            lifecycle: SessionLifecycle::Persistent,
            pending_interactions: HashMap::new(),
            turn_sse_gate: Arc::new(AtomicU8::new(SSE_TURN_UNARMED)),
            turn_active: false,
        };

        let report = session
            .send_input(TurnInput::text("continue"))
            .await
            .expect("documented 204 response accepts the exact prompt");
        assert_eq!(report.receipt, DeliveryReceiptStage::ProtocolAcknowledged);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_file_part_uses_a_roundtrippable_file_uri() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("中文 路径.txt");
        std::fs::write(&file, "fixture").unwrap();
        let prepared = crate::turn_input::prepare(TurnInput::new(vec![
            umadev_runtime::TurnInputBlock::Text {
                text: "before".into(),
            },
            umadev_runtime::TurnInputBlock::File {
                path: file.clone(),
                mode: umadev_runtime::FileInputMode::NativeOnly,
            },
            umadev_runtime::TurnInputBlock::Text {
                text: "after".into(),
            },
        ]))
        .await
        .unwrap();
        let (parts, deliveries) = opencode_parts(&prepared).unwrap();
        assert_eq!(parts[0]["text"], "before");
        assert_eq!(parts[1]["type"], "file");
        assert_eq!(parts[1]["mime"], "text/plain; charset=utf-8");
        assert_eq!(parts[2]["text"], "after");
        let uri = url::Url::parse(parts[1]["url"].as_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::canonicalize(uri.to_file_path().unwrap()).unwrap(),
            std::fs::canonicalize(file).unwrap()
        );
        assert_eq!(deliveries, vec![InputDelivery::Native; 3]);
    }

    #[tokio::test]
    async fn autonomous_create_session_sends_pure_allow_ruleset() {
        // The auto tier POSTs a pure wildcard allow — no `ask`, so the loop never
        // pauses for a per-write approval.
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                assert!(
                    req.contains("\"action\":\"allow\"") && !req.contains("\"action\":\"ask\""),
                    "autonomous session must request a pure allow ruleset: {req}"
                );
                let body = br#"{"id":"ses_auto"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, body).await;
            }
        });
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        let id = http
            .create_session(Some("build"), None, true)
            .await
            .expect("autonomous session created");
        assert_eq!(id, "ses_auto");
        server.abort();
    }

    #[tokio::test]
    async fn create_session_fails_open_on_http_error() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = tokio::io::AsyncWriteExt::write_all(
                    &mut sock,
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            }
        });
        let http = HttpCtx::new(format!("http://{addr}"), "pw", Path::new("/proj"));
        // A 500 surfaces as an Err string, not a panic (fail-open at the caller).
        let res = http.create_session(None, None, true).await;
        assert!(res.is_err(), "HTTP 500 must surface as Err: {res:?}");
        server.abort();
    }

    #[tokio::test]
    async fn pump_sse_emits_failed_turndone_when_stream_unreachable() {
        // No server listening -> the SSE connect fails -> a terminal Failed
        // TurnDone is emitted (fail-open: the runner never hangs).
        let http = HttpCtx::new("http://127.0.0.1:1".to_string(), "pw", Path::new("/proj"));
        let (tx, mut rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        tokio::spawn(pump_sse(http, "ses_dead".to_string(), tx));
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(SessionEvent::TurnDone { status, .. })) => {
                assert!(matches!(status, TurnStatus::Failed(_)));
            }
            other => panic!("expected a Failed TurnDone, got {other:?}"),
        }
    }

    // The serve-spawn -> port-parse path uses a fake `#!/bin/sh` server that
    // Windows cannot exec; the port-parse itself is also covered by the
    // platform-independent `parse_listening_url_extracts_real_port` test.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_with_program_scrapes_port_from_serve_stdout() {
        use std::os::unix::fs::PermissionsExt as _;
        use tokio::net::TcpListener;

        // A real loopback server the scraped url will point at, so create_session
        // (issued by start) actually succeeds.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            // Answer the create-session POST then the SSE GET (minimal).
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n]);
                let first = line.lines().next().unwrap_or("");
                if first.starts_with("POST /session ") {
                    let body = br#"{"id":"ses_spawned"}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, body).await;
                } else {
                    let _ = tokio::io::AsyncWriteExt::write_all(
                        &mut sock,
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                }
            }
        });

        // A fake `opencode serve`: print the listening line (pointing at our real
        // server's port), then sleep so the child stays alive while we drive it. The sleep
        // is a WALL-CLOCK lifetime, so it has to outlast a scrape that gets starved under
        // load — not just the happy-path microseconds.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-opencode-serve");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '1.17.16'; exit 0; fi\necho 'opencode server listening on http://127.0.0.1:{port}'\nsleep 60\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Explicit timeout (not the global env var) so parallel tests can't race each
        // other's serve-start budget — and a GENEROUS one, for the same reason its sibling
        // `read_listening_url_keeps_draining_stdout_for_the_session_lifetime` uses 30s: a
        // `/bin/sh` fake's spawn + first echo can be arbitrarily slow under a fully loaded
        // test runner, and this budget is a wall clock, not a work budget. What is under
        // test is that the port is SCRAPED and the session created off it — never how many
        // milliseconds a loaded machine took to get there. A tight bound here is a CI flake,
        // not a check.
        let session = OpenCodeSession::start_with_program_timeout(
            script.to_str().unwrap(),
            dir.path(),
            None,
            None,
            true,
            Duration::from_secs(30),
        )
        .await
        .expect("start should scrape the port and create the session");
        assert_eq!(session.session_id(), "ses_spawned");
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resume_gets_session_replaces_permissions_and_reattaches_sse() {
        use std::os::unix::fs::PermissionsExt as _;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sse_seen_tx, sse_seen_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut sse_seen_tx = Some(sse_seen_tx);
            for step in 0..3 {
                let (mut sock, _) = listener.accept().await.expect("resume request");
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                match step {
                    0 => {
                        assert!(request.starts_with("GET /session/ses_resume "));
                        let body = br#"{"id":"ses_resume","title":"persisted"}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        tokio::io::AsyncWriteExt::write_all(&mut sock, response.as_bytes())
                            .await
                            .unwrap();
                        tokio::io::AsyncWriteExt::write_all(&mut sock, body)
                            .await
                            .unwrap();
                    }
                    1 => {
                        assert!(request.starts_with("PATCH /session/ses_resume "));
                        let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
                        let posted: Value = serde_json::from_str(body).unwrap();
                        assert_eq!(
                            posted["permission"],
                            session_ruleset_for_profile(BasePermissionProfile::Plan)
                        );
                        assert_eq!(
                            effective_permission_action(&posted["permission"], "future_mutator"),
                            Some("deny"),
                            "resume must replace a possible stale Auto wildcard allow"
                        );
                        let response_body = br#"{"id":"ses_resume"}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            response_body.len()
                        );
                        tokio::io::AsyncWriteExt::write_all(&mut sock, response.as_bytes())
                            .await
                            .unwrap();
                        tokio::io::AsyncWriteExt::write_all(&mut sock, response_body)
                            .await
                            .unwrap();
                    }
                    _ => {
                        assert!(request.starts_with("GET /event?"));
                        let _ = sse_seen_tx.take().expect("one SSE").send(());
                        tokio::io::AsyncWriteExt::write_all(
                            &mut sock,
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    }
                }
            }
        });

        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-opencode-resume");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '1.18.1'; exit 0; fi\necho 'opencode server listening on http://127.0.0.1:{port}'\nsleep 60\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut session = OpenCodeSession::resume_with_program_timeout(
            script.to_str().unwrap(),
            dir.path(),
            None,
            "ses_resume",
            BasePermissionProfile::Plan,
            Duration::from_secs(30),
        )
        .await
        .expect("persisted session resumes");
        assert_eq!(session.session_id(), "ses_resume");
        assert_eq!(
            <OpenCodeSession as BaseSession>::session_id(&session),
            Some("ses_resume"),
            "workflow state must persist the OpenCode resume pointer"
        );
        tokio::time::timeout(Duration::from_secs(2), sse_seen_rx)
            .await
            .expect("SSE reattached")
            .expect("SSE observer alive");
        session.end().await.unwrap();
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_times_out_when_serve_never_announces() {
        use std::os::unix::fs::PermissionsExt as _;
        // A fake serve that prints nothing and just hangs -> start must fail
        // (fail-open) within the (explicit, short) timeout, not hang forever. We
        // pass the timeout directly so we never mutate a process-global env var
        // that a concurrent test would observe.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("silent-serve");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '1.17.16'; exit 0; fi\nsleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let res = OpenCodeSession::start_with_program_timeout(
            script.to_str().unwrap(),
            dir.path(),
            None,
            None,
            true,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(res, Err(SessionError::Start(_))),
            "a serve that never announces must fail-open as Start error"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plan_session_start_refuses_a_sub_minimum_version() {
        use std::os::unix::fs::PermissionsExt as _;
        // The read-only floor is scoped to Plan on the persistent session path
        // too: a Plan start on a sub-minimum OpenCode is refused BEFORE `serve`
        // is ever spawned, so no read-only Plan session can open on an old build.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("old-serve");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '1.14.30'; exit 0; fi\nsleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let res = OpenCodeSession::start_with_program_timeout_profile(
            script.to_str().unwrap(),
            dir.path(),
            Some("plan"),
            None,
            BasePermissionProfile::Plan,
            Duration::from_secs(5),
        )
        .await;
        match res {
            Err(SessionError::Start(e)) => {
                assert!(e.contains("minimum safe version 1.14.31"), "{e}");
            }
            Err(other) => panic!("expected a Start refusal, got a different error: {other}"),
            Ok(_) => panic!("Plan start on a sub-minimum version must be refused"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn guarded_session_start_passes_the_version_gate_on_an_old_build() {
        use std::os::unix::fs::PermissionsExt as _;
        // A Guarded session on a sub-minimum build must NOT be blocked by the
        // version NUMBER. The fake answers `--version` with an old value and then
        // never announces a serve URL, so start still fails — but at the serve
        // stage, NOT with the version-floor refusal. This proves Guarded cleared
        // the gate that Plan is refused at.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("old-guarded-serve");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '1.14.30'; exit 0; fi\nsleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let res = OpenCodeSession::start_with_program_timeout_profile(
            script.to_str().unwrap(),
            dir.path(),
            Some("build"),
            None,
            BasePermissionProfile::Guarded,
            Duration::from_secs(1),
        )
        .await;
        match res {
            Err(SessionError::Start(e)) => assert!(
                !e.contains("minimum safe version"),
                "Guarded must clear the version gate on any installed build: {e}"
            ),
            Err(other) => panic!("unexpected non-Start error: {other}"),
            Ok(_) => panic!("a serve that never announces must fail as a Start error"),
        }
    }

    // M8: after scraping the listening URL, the LONG-LIVED server's stdout must
    // keep being drained — otherwise anything it later logs fills the ~64 KiB pipe
    // buffer and the next write EPIPE/SIGPIPE-kills the server mid-run. The fake
    // announces the URL, FLOODS >64 KiB to stdout, then touches a sentinel; the
    // sentinel only lands once the flood write completes, which REQUIRES our
    // background drain (without it the child blocks on a full pipe forever).
    #[cfg(unix)]
    #[tokio::test]
    async fn read_listening_url_keeps_draining_stdout_for_the_session_lifetime() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let sentinel = dir.path().join("drained.flag");
        let script = dir.path().join("flood-serve");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo 'opencode server listening on http://127.0.0.1:1'\n\
                 i=0\n\
                 while [ $i -lt 200 ]; do printf '%01000d\\n' 0; i=$((i+1)); done\n\
                 : > '{}'\n\
                 sleep 5\n",
                sentinel.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (prog, lead) = spawn_parts(script.to_str().unwrap());
        let mut cmd = Command::new(prog);
        cmd.args(&lead);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        cmd.kill_on_drop(true);
        let mut child = crate::spawn_retrying_etxtbsy(&mut cmd).unwrap();
        let stdout = child.stdout.take().unwrap();

        // Generous scrape budget: a `/bin/sh` fake's spawn + first echo can be
        // arbitrarily slow under heavy parallel test load (same reason the sibling
        // serve tests use a large budget) — the thing under test is the lifetime
        // drain, not the announce latency.
        let url = read_listening_url(stdout, Duration::from_secs(30))
            .await
            .expect("should scrape the announce line");
        assert_eq!(url, "http://127.0.0.1:1");

        // The >64 KiB flood + sentinel only complete if our drain keeps the pipe
        // clear; poll (generously, for load) for the flag.
        let mut drained = false;
        for _ in 0..300 {
            if sentinel.exists() {
                drained = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            drained,
            "stdout was not drained for the session lifetime — the server blocked on a full pipe"
        );
        let _ = child.start_kill();
    }

    #[test]
    fn native_events_redact_before_transcript_tool_activity_and_audit() {
        const SECRET: &str = "SYNTH_OPENCODE_SESSION_SECRET_83";
        let mut tracker = PartTracker::default();
        let text = serde_json::json!({
            "type": "message.part.updated",
            "properties": {"part": {
                "id": "part-text-secret",
                "sessionID": "ses_secret",
                "type": "text",
                "text": format!("password={SECRET}")
            }}
        })
        .to_string();
        let tool = serde_json::json!({
            "type": "message.part.updated",
            "properties": {"part": {
                "id": "part-tool-secret",
                "sessionID": "ses_secret",
                "type": "tool",
                "tool": "bash",
                "state": {
                    "status": "completed",
                    "input": {
                        "command": format!("curl -H 'Authorization: Bearer {SECRET}' example.test"),
                        "password": SECRET,
                        "nextPageToken": "safe-page-3"
                    },
                    "title": format!("private_key={SECRET}")
                }
            }}
        })
        .to_string();
        let mut events = translate_frame_tracked(&text, "ses_secret", &mut tracker);
        events.extend(translate_frame_tracked(&tool, "ses_secret", &mut tracker));
        let audit_view = format!("{events:?}");
        assert!(
            !audit_view.contains(SECRET),
            "event/audit leaked: {audit_view}"
        );
        assert!(audit_view.contains("safe-page-3"));

        let mut activity = umadev_runtime::ToolActivity::default();
        for event in &events {
            activity.observe(event);
        }
    }
}
