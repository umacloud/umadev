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
//! - opens **one** session (`POST /session`) with a wildcard permission ruleset
//!   so tool calls (file writes / bash) are silently pre-approved — the base
//!   keeps context across phases and runs its own agentic tool loop (it WRITES
//!   files), instead of narrating a paragraph and asking "shall I continue?";
//! - subscribes the server-sent-events stream (`GET /event`, long-lived) in a
//!   background task that parses each `data: {id,type,properties}` frame into a
//!   [`SessionEvent`](umadev_runtime::SessionEvent);
//! - injects one **directive per phase** (`POST /session/:id/prompt_async`, the
//!   same session = context flows);
//! - exposes the [`BaseSession`] contract the 9-phase runner drives.
//!
//! ## Wire protocol (verified against opencode source — `opencode-dev/packages`)
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
//! - routes: `.../groups/session.ts` (`POST /session`, `POST
//!   /session/:id/prompt_async`, `POST /session/:id/abort`, `DELETE
//!   /session/:id`) and `.../groups/permission.ts` (`POST
//!   /permission/:id/reply`). NOTE the deprecated
//!   `/session/:id/permissions/:id` route — we use the live
//!   `/permission/.../reply`.
//! - create vs prompt model shapes DIFFER: create's `model` is
//!   `{id,providerID,variant?}` (`session.ts CreateInput`); prompt's `model` is
//!   `{providerID,modelID}` (`session/prompt.ts PromptInput` -> `ModelRef`). We
//!   pass NEITHER by default (the base uses its own configured model) so we can
//!   never send a malformed shape; an explicit provider/model id is honored.
//! - SSE framing: `.../handlers/event.ts` —
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
//!   - **turn done** is `session.status` with `properties.status.type=="idle"`.
//!   - tool-state schema: `packages/core/src/v1/session.ts` (`ToolPart`,
//!     `ToolState{Pending,Running,Completed,Error}`).
//!   - permission Rule/Reply: `packages/core/src/v1/permission.ts`.
//!
//! ## Fail-open by contract
//! Server won't start / SSE drops / an HTTP call errors / the session is busy ->
//! the session surfaces a [`TurnStatus::Failed`] (or `next_event` -> `None`),
//! never a panic. A driver bug must never crash the host.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;
use umadev_runtime::{ApprovalDecision, BaseSession, SessionError, SessionEvent, TurnStatus};

use crate::spawn_parts;
use crate::stderr_tail::{drain_stderr_into, StderrTail};

/// How many events the SSE-reader task may buffer ahead of the consumer.
const EVENT_CHANNEL_CAP: usize = 256;

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
    /// Bounded tail of the server child's STDERR, captured by the drain task,
    /// surfaced via [`BaseSession::stderr_tail`] to explain *why* a base went
    /// idle (a bad model / not logged in / a config error opencode prints to
    /// stderr before falling silent).
    stderr: StderrTail,
    /// HTTP transport (base url + auth header baked into each call).
    http: HttpCtx,
    /// The opencode session id (`ses_...`) created at `start`.
    session_id: String,
    /// SSE -> normalized event channel, fed by the background reader task.
    events: mpsc::Receiver<SessionEvent>,
    /// `true` once a turn directive is in flight and not yet idle. The runner
    /// owns serial discipline (it sends the next directive only after the prior
    /// turn's idle `TurnDone`); this mirrors that state so a caller can cheaply
    /// assert "no turn in flight" via [`OpenCodeSession::is_turn_active`] before
    /// re-driving — sending a second prompt while busy is an opencode
    /// `SessionBusyError`.
    turn_active: bool,
}

/// The HTTP context shared by every call: base url, auth header, project dir.
#[derive(Clone)]
struct HttpCtx {
    client: reqwest::Client,
    /// e.g. `http://127.0.0.1:54321`.
    base_url: String,
    /// `Basic base64("opencode:<password>")`.
    auth: String,
    /// The percent-encoded absolute project path for `x-opencode-directory` and
    /// the `?directory=` query the SSE stream filters on.
    directory: String,
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
    /// `autonomous` selects the session's permission ruleset, mirroring codex's
    /// `approvalPolicy` tiering so all three bases behave consistently: `true`
    /// (the `auto` trust tier) installs a wildcard `allow` ruleset so the agentic
    /// loop runs silently; `false` (the `guarded` tier) installs a finer ruleset
    /// that routes writes / dangerous bash through `permission.asked` (→ a
    /// `NeedApproval` the orchestrator answers), so the guarded human-in-the-loop
    /// posture is the same on opencode as on codex (`on-request`) and claude
    /// (`default`). Governance still audits every tool call via the event stream
    /// regardless of tier.
    pub async fn start(
        workspace: &Path,
        agent: Option<&str>,
        model: Option<&str>,
        autonomous: bool,
    ) -> Result<Self, SessionError> {
        let program =
            std::env::var("UMADEV_OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
        Self::start_with_program(&program, workspace, agent, model, autonomous).await
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
        let password = random_password();
        let (prog, lead) = spawn_parts(program);
        let mut cmd = Command::new(prog);
        cmd.args(&lead);
        cmd.args(serve_args());
        cmd.current_dir(workspace);
        // The customer's full environment is inherited UNCHANGED (the base
        // self-authenticates with its own login) — we ONLY add the server
        // password so our HTTP calls can authenticate against this private,
        // loopback-only server. UmaDev injects no model endpoint.
        cmd.env("OPENCODE_SERVER_PASSWORD", &password);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| SessionError::Start(spawn_err(program, &e)))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SessionError::Start("opencode serve: no stdout pipe".to_string()))?;
        // Drain stderr on its own task (so a noisy server can't stall on a full
        // pipe) AND capture a bounded tail for idle diagnosis.
        let stderr_tail = StderrTail::new();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr_into(stderr, stderr_tail.clone()));
        }

        // Scrape the real bound base url from the server's stdout (port 0 = the
        // OS picks the port, so we MUST read it back; cannot assume a port).
        let base_url = match read_listening_url(stdout, serve_timeout).await {
            Ok(url) => url,
            Err(e) => {
                let _ = child.start_kill();
                return Err(SessionError::Start(e));
            }
        };

        let http = HttpCtx::new(base_url, &password, workspace);

        // Open the one session for the whole run. The ruleset follows the autonomy
        // tier (`autonomous` → wildcard allow; guarded → writes/dangerous bash ask),
        // so opencode's gate posture matches codex / claude.
        let session_id = match http.create_session(agent, model, autonomous).await {
            Ok(id) => id,
            Err(e) => {
                let _ = child.start_kill();
                return Err(SessionError::Start(format!("create session: {e}")));
            }
        };

        // Subscribe the SSE stream for THIS session id and pump normalized
        // events into a channel a background task owns.
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let stream_http = http.clone();
        let stream_session = session_id.clone();
        tokio::spawn(pump_sse(stream_http, stream_session, tx));

        Ok(Self {
            child: std::sync::Mutex::new(child),
            stderr: stderr_tail,
            http,
            session_id,
            events: rx,
            turn_active: false,
        })
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

#[async_trait]
impl BaseSession for OpenCodeSession {
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
        let session_id = self
            .http
            .create_readonly_session()
            .await
            .map_err(SessionError::Start)?;
        // Its own SSE subscription, scoped to the fork session id.
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        tokio::spawn(pump_sse(self.http.clone(), session_id.clone(), tx));
        Ok(Box::new(OpenCodeForkSession {
            http: self.http.clone(),
            session_id,
            events: rx,
            turn_active: false,
        }))
    }

    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        // `prompt_async` returns immediately (202/NoContent) and the same
        // session retains context. Serial discipline: the runner only sends the
        // next directive after observing the previous turn's idle TurnDone, so
        // we never hit a `SessionBusyError` here. Fail-open: an HTTP error is a
        // Send error the runner can surface as a failed turn.
        self.turn_active = true;
        self.http
            .prompt_async(&self.session_id, &directive)
            .await
            .map_err(SessionError::Send)
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        // No internal timeout BY DESIGN — the runner owns phase/run budgets and
        // races this against them (then calls `interrupt`). Keep the session a
        // pure relay so a synthetic TurnDone never races a real `idle`.
        let ev = self.events.recv().await;
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
        // opencode permission reply vocabulary is `once`/`always`/`reject`
        // (`PermissionV1.Reply`). Allow -> `once` (grant just this call); Deny ->
        // `reject`. We never auto-`always` — escalation stays the runner's call.
        let reply = match decision {
            ApprovalDecision::Allow => "once",
            ApprovalDecision::Deny => "reject",
        };
        self.http
            .permission_reply(req_id, reply)
            .await
            .map_err(SessionError::Send)
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        self.turn_active = false;
        self.http
            .abort(&self.session_id)
            .await
            .map_err(SessionError::Send)
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        // Best-effort: delete the session, then kill the resident server so no
        // orphan `opencode serve` lingers (kill_on_drop is a backstop).
        let _ = self.http.delete_session(&self.session_id).await;
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        self.stderr.snapshot()
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
    turn_active: bool,
}

#[async_trait]
impl BaseSession for OpenCodeForkSession {
    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        self.turn_active = true;
        self.http
            .prompt_async(&self.session_id, &directive)
            .await
            .map_err(SessionError::Send)
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        let ev = self.events.recv().await;
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
        // A read-only fork should never need to approve a write, but honor the
        // contract: Allow→`once`, Deny→`reject` (the deny ruleset means the base
        // would already have rejected the mutating call).
        let reply = match decision {
            ApprovalDecision::Allow => "once",
            ApprovalDecision::Deny => "reject",
        };
        self.http
            .permission_reply(req_id, reply)
            .await
            .map_err(SessionError::Send)
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        self.turn_active = false;
        self.http
            .abort(&self.session_id)
            .await
            .map_err(SessionError::Send)
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        // Delete ONLY this fork's session — NEVER the shared resident server
        // (the parent OpenCodeSession owns that child's lifetime).
        let _ = self.http.delete_session(&self.session_id).await;
        Ok(())
    }
}

impl HttpCtx {
    /// Build the HTTP context. The directory is percent-encoded for the
    /// `x-opencode-directory` header (header values must be ASCII) and reused as
    /// the `?directory=` query the event stream filters on.
    fn new(base_url: String, password: &str, workspace: &Path) -> Self {
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
            base_url,
            auth,
            directory: encoded,
        }
    }

    /// Common headers every authenticated call carries.
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(reqwest::header::AUTHORIZATION, &self.auth)
            .header("x-opencode-directory", &self.directory)
    }

    /// `POST /session` with a permission ruleset chosen by the autonomy tier
    /// (see [`session_ruleset`]). Returns the created `session.id`. The `model`
    /// here, if any, uses CREATE's shape `{id,providerID,variant?}` (distinct from
    /// prompt's `{providerID,modelID}`).
    async fn create_session(
        &self,
        agent: Option<&str>,
        model: Option<&str>,
        autonomous: bool,
    ) -> Result<String, String> {
        let mut body = json!({
            // Ruleset: `[{permission,pattern,action}]` (permission.ts Rule). The
            // tier picks it: autonomous → wildcard allow (silent); guarded →
            // writes / dangerous bash `ask` (→ `permission.asked`). Governance
            // still audits every tool call via the event stream regardless.
            "permission": session_ruleset(autonomous),
        });
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

    /// `POST /session` with a DENY ruleset — a READ-ONLY session for a critic
    /// fork. `*`/`*`/`deny` rejects every tool call that would mutate the
    /// workspace, so the seat can read the blackboard but never writes it (the
    /// single-writer invariant). Returns the created `session.id`.
    async fn create_readonly_session(&self) -> Result<String, String> {
        let body = json!({
            "permission": [{ "permission": "*", "pattern": "*", "action": "deny" }],
            "agent": "build",
        });
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

    /// `POST /session/:id/prompt_async` — inject a phase directive. Returns
    /// immediately (NoContent); the work streams over SSE.
    async fn prompt_async(&self, session_id: &str, directive: &str) -> Result<(), String> {
        let body = json!({
            "parts": [{ "type": "text", "text": directive }],
        });
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("/session/{session_id}/prompt_async"),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("prompt_async: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("prompt_async: HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// `POST /permission/:id/reply {reply}` — answer a `permission.asked`.
    async fn permission_reply(&self, request_id: &str, reply: &str) -> Result<(), String> {
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
        self.req(reqwest::Method::DELETE, &format!("/session/{session_id}"))
            .send()
            .await
            .map_err(|e| format!("delete session: {e}"))?;
        Ok(())
    }
}

/// Background task: open the long-lived SSE stream and pump normalized events
/// into `tx` forever. On stream end / error (the server died or the connection
/// dropped) emit a terminal `Failed` so a mid-turn drop surfaces as
/// `TurnDone{Failed}` rather than a silent hang. Fail-open throughout.
async fn pump_sse(http: HttpCtx, session_id: String, tx: mpsc::Sender<SessionEvent>) {
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
            let _ = tx
                .send(SessionEvent::TurnDone {
                    status: TurnStatus::Failed(format!("event stream: HTTP {}", r.status())),
                    // opencode's SSE carries no token usage → always None; the
                    // consumer estimates (chars/4) so `/usage` stays honest (F3).
                    usage: None,
                })
                .await;
            return;
        }
        Err(e) => {
            let _ = tx
                .send(SessionEvent::TurnDone {
                    status: TurnStatus::Failed(format!("event stream connect: {e}")),
                    usage: None,
                })
                .await;
            return;
        }
    };

    // SSE framing: lines `event: ...` / `data: ...`, frames separated by a blank
    // line (handlers/event.ts encodes `JSON.stringify(payload)` per data line).
    // Accumulate `data:` lines until a blank line, then parse one frame.
    let mut byte_stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    // Per-part streaming state (text suffix lengths + which tools already emitted a
    // ToolCall), so a cumulative text update forwards only its new suffix and a
    // tool that skipped its `running` frame still gets a back-filled ToolCall (F6).
    // Lives for the whole subscription.
    let mut tracker = PartTracker::default();
    while let Some(chunk) = byte_stream.next().await {
        let Ok(bytes) = chunk else {
            break; // stream error -> fall through to terminal Failed
        };
        buf.extend_from_slice(&bytes);
        // Drain complete lines (split on '\n'; tolerate '\r\n').
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                // Frame boundary: parse the accumulated data payload.
                if !data_lines.is_empty() {
                    let payload = data_lines.join("\n");
                    data_lines.clear();
                    for ev in translate_frame_tracked(&payload, &session_id, &mut tracker) {
                        if tx.send(ev).await.is_err() {
                            return; // consumer dropped
                        }
                        // NOTE: keep streaming after an idle TurnDone — the SAME
                        // subscription serves every phase's turn (one long GET).
                    }
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // `event:` / `id:` / `:`-comment lines are ignored — `type` lives
            // inside the JSON payload, not the SSE `event:` field.
        }
    }
    // Stream ended / errored -> terminal failure so the runner never hangs.
    let _ = tx
        .send(SessionEvent::TurnDone {
            status: TurnStatus::Failed("event stream ended".to_string()),
            usage: None,
        })
        .await;
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
    tools_called: std::collections::HashSet<String>,
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
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return Vec::new();
    };
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let props = v.get("properties").cloned().unwrap_or(Value::Null);
    match kind {
        "message.part.updated" => translate_part(&props, session_id, tracker),
        "session.error" => translate_error(&props, session_id),
        "permission.asked" => translate_permission(&props, session_id),
        "session.status" => translate_status(&props, session_id),
        // Connection / liveness frames carry no turn semantics.
        _ => Vec::new(),
    }
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
        Some("text") => {
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
            let prev = tracker.text_lens.get(&id).copied().unwrap_or(0);
            // Cumulative growth → suffix; a non-monotonic / replaced part (shorter,
            // or `prev` not a char boundary) re-emits the whole current text.
            let suffix = if full.len() >= prev && full.is_char_boundary(prev) {
                &full[prev..]
            } else {
                full
            };
            tracker.text_lens.insert(id, full.len());
            if suffix.is_empty() {
                Vec::new()
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
    // A stable id for THIS tool part so back-fill is per-part (fail-open to the
    // call id, else the empty string → a single anonymous part, still correct for
    // the common single-tool frame).
    let part_id = part
        .get("id")
        .or_else(|| part.get("callID"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match status {
        // running = the tool actually started (input now finalized) — the ToolCall
        // truth. Mark the part so a later `completed` doesn't re-emit it.
        "running" => {
            tracker.tools_called.insert(part_id);
            vec![SessionEvent::ToolCall { name, input }]
        }
        "completed" => {
            let summary = state
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| state.get("output").and_then(Value::as_str))
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            backfilled_tool_events(
                tracker,
                &part_id,
                &name,
                &input,
                SessionEvent::ToolResult { ok: true, summary },
            )
        }
        "error" => {
            let summary = state
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("tool error")
                .chars()
                .take(200)
                .collect();
            backfilled_tool_events(
                tracker,
                &part_id,
                &name,
                &input,
                SessionEvent::ToolResult { ok: false, summary },
            )
        }
        // pending = queued, no finalized input yet -> wait for running/completed.
        _ => Vec::new(),
    }
}

/// Build the events for a terminal (`completed` / `error`) tool frame, BACK-FILLING
/// a `ToolCall` first if this part never emitted one and it carries a usable input
/// (F6). The `result` event is always emitted. Marks the part as called so a
/// duplicate terminal frame can't re-emit the `ToolCall`.
fn backfilled_tool_events(
    tracker: &mut PartTracker,
    part_id: &str,
    name: &str,
    input: &Value,
    result: SessionEvent,
) -> Vec<SessionEvent> {
    let already_called = tracker.tools_called.contains(part_id);
    // Only back-fill when we have a real input object — a terminal frame with no
    // recoverable input can't be a faithful ToolCall (fail-open: just the result,
    // exactly as before). A non-null object (even `{}`) is enough: the consumer
    // keys off the tool NAME for the audit/diff, and a `{}` still attributes it.
    let have_input = !input.is_null();
    if !already_called && have_input {
        tracker.tools_called.insert(part_id.to_string());
        vec![
            SessionEvent::ToolCall {
                name: name.to_string(),
                input: input.clone(),
            },
            result,
        ]
    } else {
        vec![result]
    }
}

/// `session.error` -> a terminal `TurnDone{Failed}` so a base-side error ends
/// the phase (rather than hanging until the run budget). `properties.error`
/// carries `{name, data.message}` (see `cli/cmd/run.ts`).
fn translate_error(props: &Value, session_id: &str) -> Vec<SessionEvent> {
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
        // opencode's SSE reports no token usage (F3) → estimate downstream.
        usage: None,
    }]
}

/// `permission.asked` -> `NeedApproval`. `properties` is a `PermissionV1.Request`
/// (`{id,sessionID,permission,patterns,...}`). The `id` (`per...`) is what
/// `respond` replies against. With the wildcard ruleset this is rare, but a
/// finer ruleset (gate / dangerous mode) can still ask.
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
    vec![SessionEvent::NeedApproval {
        req_id: req_id.to_string(),
        action,
        target,
    }]
}

/// `session.status` -> `TurnDone{Completed}` when `status.type=="idle"` for our
/// session. This is THE authoritative turn-done boundary (`cli/cmd/run.ts`
/// breaks its loop on exactly this). `busy`/`retry` carry no turn semantics.
fn translate_status(props: &Value, session_id: &str) -> Vec<SessionEvent> {
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
            // opencode's SSE reports no token usage (F3) → estimate downstream.
            usage: None,
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
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = parse_listening_url(&line) {
                return Ok(url);
            }
        }
        Err("opencode serve exited before announcing a listen address".to_string())
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(res) => res,
        Err(_) => Err(format!(
            "opencode serve did not announce a listen address within {}s",
            timeout.as_secs()
        )),
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

/// Build the `POST /session` permission ruleset for the autonomy tier — the
/// opencode counterpart of codex's `approvalPolicy` (`never` vs `on-request`) and
/// claude's `--permission-mode` (`acceptEdits` vs `default`), so all three bases
/// share ONE gate posture.
///
/// - `autonomous == true` (the `auto` trust tier): a single wildcard `allow` rule
///   — every tool call is silently pre-approved, the agentic loop runs without a
///   per-event round-trip. Governance still audits each call via the event stream.
/// - `autonomous == false` (the `guarded` tier): writes (`edit`/`write`) and
///   DANGEROUS bash patterns route to `ask`, so the server raises
///   `permission.asked` (→ a `NeedApproval` the orchestrator answers via the
///   trust-tiered `approval_decision`); everything else stays `allow`. opencode
///   evaluates rules so that a more specific pattern wins, so the broad `*/*`
///   `allow` is the floor and the narrower `ask` rules override it for the
///   sensitive surfaces. Mirrors codex's `on-request` (where the server asks
///   before a command/file change) — a consistent human-in-the-loop tier.
///
/// **Fail-open posture:** the guarded ruleset is the CONSERVATIVE choice (it asks
/// rather than silently allowing), so even if a finer rule fails to register the
/// session never silently runs an ungoverned write — at worst it asks more than
/// needed, which the orchestrator answers. Pure; exposed for tests.
#[must_use]
pub fn session_ruleset(autonomous: bool) -> Value {
    if autonomous {
        // The whole loop runs silently (audited, not gated).
        return json!([{ "permission": "*", "pattern": "*", "action": "allow" }]);
    }
    // Guarded: allow by default, but ASK before a write / a dangerous shell verb.
    // Order matters only for human readability — opencode resolves by specificity,
    // not array order — so the broad allow comes first as the floor.
    json!([
        { "permission": "*", "pattern": "*", "action": "allow" },
        { "permission": "edit", "pattern": "*", "action": "ask" },
        { "permission": "write", "pattern": "*", "action": "ask" },
        // Destructive / irreversible shell verbs the orchestrator must vet. The
        // patterns mirror the dangerous-bash floor governance enforces elsewhere.
        { "permission": "bash", "pattern": "rm *", "action": "ask" },
        { "permission": "bash", "pattern": "*rm -rf*", "action": "ask" },
        { "permission": "bash", "pattern": "git push*", "action": "ask" },
        { "permission": "bash", "pattern": "*sudo *", "action": "ask" },
        { "permission": "bash", "pattern": "*curl *", "action": "ask" },
        { "permission": "bash", "pattern": "*wget *", "action": "ask" },
    ])
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
            SessionEvent::ToolCall { name, input } => {
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
            SessionEvent::ToolCall { name, input } => {
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
            vec![SessionEvent::ToolResult {
                ok: true,
                summary: "wrote src/app.ts".to_string()
            }],
            "completed after running must not re-emit the ToolCall"
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
            SessionEvent::ToolCall { name, input } => {
                assert_eq!(name, "Write", "back-filled call uses the normalized name");
                assert_eq!(
                    input["file_path"], "src/new.ts",
                    "back-filled call carries the normalized input (so audit + diff work)"
                );
            }
            other => panic!("expected a back-filled Write ToolCall first, got {other:?}"),
        }
        assert!(
            matches!(&evs[1], SessionEvent::ToolResult { ok: true, .. }),
            "the ToolResult still follows: {:?}",
            evs[1]
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
            vec![SessionEvent::ToolResult {
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
    fn translate_permission_asked_is_needapproval() {
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
            SessionEvent::NeedApproval {
                req_id,
                action,
                target,
            } => {
                assert_eq!(req_id, "per_xyz");
                assert_eq!(action, "bash");
                assert!(target.contains("rm -rf *"));
            }
            other => panic!("expected NeedApproval, got {other:?}"),
        }
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
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    s.write_all(resp.as_bytes()).unwrap();
                    s.write_all(body).unwrap();
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
                    // Hold briefly so the client drains all frames before close.
                    std::thread::sleep(std::time::Duration::from_millis(200));
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
        http.prompt_async(&session_id, "build the thing")
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

    #[tokio::test]
    async fn create_readonly_session_sends_deny_ruleset() {
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
                // The body must carry a DENY ruleset — the read-only fence.
                assert!(
                    req.contains("\"action\":\"deny\""),
                    "fork session must request a deny ruleset: {req}"
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
            .create_readonly_session()
            .await
            .expect("read-only session created");
        assert_eq!(id, "ses_fork");
        server.abort();
    }

    #[test]
    fn session_ruleset_autonomous_is_wildcard_allow() {
        // The `auto` tier: one wildcard `allow` rule — the loop runs silently.
        let r = session_ruleset(true);
        let arr = r.as_array().expect("ruleset is an array");
        assert_eq!(arr.len(), 1, "autonomous = a single wildcard rule");
        assert_eq!(arr[0]["permission"], "*");
        assert_eq!(arr[0]["pattern"], "*");
        assert_eq!(arr[0]["action"], "allow");
    }

    #[test]
    fn session_ruleset_guarded_asks_on_writes_and_dangerous_bash() {
        // The `guarded` tier: allow is the floor, but writes/edits and dangerous
        // shell verbs route to `ask` so the server raises `permission.asked`
        // (→ NeedApproval) — opencode's counterpart of codex `on-request`.
        let r = session_ruleset(false);
        let arr = r.as_array().expect("ruleset is an array");
        // There is a broad allow floor …
        assert!(
            arr.iter()
                .any(|x| x["permission"] == "*" && x["action"] == "allow"),
            "guarded keeps a broad allow floor: {arr:?}"
        );
        // … and an `ask` rule for each write permission.
        for perm in ["edit", "write"] {
            assert!(
                arr.iter()
                    .any(|x| x["permission"] == perm && x["action"] == "ask"),
                "guarded must ASK before a {perm}: {arr:?}"
            );
        }
        // … and at least one dangerous-bash `ask` rule.
        assert!(
            arr.iter()
                .any(|x| x["permission"] == "bash" && x["action"] == "ask"),
            "guarded must ASK before a dangerous bash verb: {arr:?}"
        );
        // No rule silently DENIES (the floor is allow/ask, never a blanket deny —
        // that's the read-only fork's posture, not the main writer's).
        assert!(
            !arr.iter().any(|x| x["action"] == "deny"),
            "the guarded main session never blanket-denies: {arr:?}"
        );
    }

    #[tokio::test]
    async fn guarded_create_session_sends_ask_ruleset() {
        // End-to-end: a guarded (`autonomous = false`) create_session POSTs the
        // ask-on-writes ruleset, so opencode will raise `permission.asked` for a
        // write — the same human-in-the-loop posture codex gets from `on-request`.
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
                // The guarded body carries an `ask` action (not pure wildcard allow).
                assert!(
                    req.contains("\"action\":\"ask\""),
                    "guarded session must request an ask ruleset: {req}"
                );
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
        // server's port), then sleep so the child stays alive while we drive it.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-opencode-serve");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho 'opencode server listening on http://127.0.0.1:{port}'\nsleep 5\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Explicit timeout (not the global env var) so parallel tests can't
        // race each other's serve-start budget.
        let session = OpenCodeSession::start_with_program_timeout(
            script.to_str().unwrap(),
            dir.path(),
            None,
            None,
            true,
            Duration::from_secs(10),
        )
        .await
        .expect("start should scrape the port and create the session");
        assert_eq!(session.session_id(), "ses_spawned");
        server.abort();
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
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
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
}
