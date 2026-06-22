//! `umadev-runtime` — the `Runtime` trait that every "brain" of the
//! pipeline implements.
//!
//! UmaDev is the **project-director Agent**. The actual coding work happens in one
//! of three "brain" implementations behind this trait:
//!
//! - a logged-in host CLI (`claude` / `codex` / `opencode`) driven as a subprocess, from
//!   [`umadev-host`]; or
//! - [`OfflineRuntime`], which returns empty bodies so the pipeline falls back
//!   to deterministic templates when no brain is selected.
//!
//! [`umadev-host`]: umadev_host

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::doc_markdown,
    clippy::must_use_candidate
)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use umadev_spec::RuntimeKind;

/// A single message in a runtime conversation. Wire format is normalised
/// to plain text + role; this struct does not model tool calls or
/// multi-modal parts — those live in the host CLI on the other side of
/// `umadev-host` and never touch this crate.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// `system` | `user` | `assistant`.
    pub role: String,
    /// Plain text body.
    pub content: String,
}

/// Request body the agent hands to a runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Model identifier; format is provider-specific (drivers may
    /// ignore this — `claude --print` decides its own model).
    pub model: String,
    /// Conversation so far, oldest → newest.
    pub messages: Vec<Message>,
    /// Optional max-tokens cap; drivers may ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Optional sampling temperature; drivers may ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Optional system prompt; host drivers merge it into the user
    /// prompt because the host CLIs only take one prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}

/// Response a runtime returns.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// Plain-text completion (host CLI's stdout, cleaned).
    pub text: String,
    /// Identifier — driver-specific ("claude-code-cli", "codex-cli", "offline").
    pub id: String,
    /// Effective model name reported by the backend (or whatever the
    /// caller asked for).
    pub model: String,
    /// Approximate token usage; defaults to zero when the backend does
    /// not report it (host CLIs typically do not).
    #[serde(default)]
    pub usage: Usage,
}

/// Approximate token usage. Fields default to 0 when the backend does
/// not report them.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed from the request.
    pub input_tokens: u32,
    /// Tokens produced in the response.
    pub output_tokens: u32,
}

/// Errors any runtime may return.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Response body could not be parsed as JSON (rarely surfaces — host
    /// drivers return plain text and never decode JSON).
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
    /// Required configuration missing.
    #[error("config: {0}")]
    Config(String),
    /// A host-CLI subprocess driver failed (spawn, non-zero exit).
    #[error("host process: {0}")]
    HostProcess(String),
    /// The host CLI timed out and was killed. Distinct from HostProcess so
    /// callers can retry on timeout without string-matching error messages.
    #[error("timeout after {0}s: {1}")]
    Timeout(u64, String),
}

/// What a borrowed brain can do.
///
/// UmaDev owns no model — it borrows whichever LLM the base CLI / external
/// API is connected to, and those brains have DIFFERENT powers. Rather than
/// smear `if backend == "claude-code"` literals across the agent crate, each
/// runtime declares its capabilities once and the director adapts: it only
/// emits a persistent-`/goal` directive to a brain that supports it, only runs
/// the streaming UI for a brain that streams, only meters a brain that reports
/// usage, and knows whether real-time pre-write governance is active.
// These are independent capability FLAGS (a feature table), not state that
// should collapse into an enum — a brain can have any combination.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BrainCapabilities {
    /// Supports a persistent "keep working until the goal is complete" mode
    /// (Claude Code's `/goal`). False → the director uses a prompt-level
    /// "work through every task, don't stop early" fallback instead.
    pub persistent_goal: bool,
    /// Emits real-time stream events (tool calls / text deltas) DURING a call.
    /// False → the director shows a heartbeat instead of a frozen spinner.
    pub streaming: bool,
    /// Reports real token usage in its responses (so `/usage` is truthful).
    pub reports_usage: bool,
    /// Fires a real-time pre-write governance hook (Claude Code `PreToolUse`).
    /// False → the director relies on a post-phase file scan + the quality gate.
    pub realtime_governance: bool,
}

/// The contract every backend implements.
///
/// In practice the implementations are:
/// - [`OfflineRuntime`] in this crate — returns empty bodies.
/// - `ClaudeCodeDriver` / `CodexDriver` / `OpenCodeDriver` in `umadev-host`
///   — drive a logged-in host CLI base as a subprocess.
#[async_trait]
pub trait Runtime: Send + Sync {
    /// Stable runtime kind (drives audit identifiers).
    fn kind(&self) -> RuntimeKind;

    /// What this borrowed brain can do — see [`BrainCapabilities`]. The default
    /// is conservative (a generic brain does nothing special); the host CLI
    /// drivers override it to declare their real powers.
    fn capabilities(&self) -> BrainCapabilities {
        BrainCapabilities::default()
    }

    /// Whether this runtime is the deterministic offline-template backend (no
    /// real brain). The reliable signal for "should we drive a model?" — used
    /// instead of inspecting a backend-id string. Default `false` (real brain);
    /// only [`OfflineRuntime`] overrides it to `true`.
    fn is_offline(&self) -> bool {
        false
    }

    /// Return a fresh, INDEPENDENT instance for CONCURRENT use — a clean session
    /// (no resume) so parallel pipeline steps (e.g. drafting the architecture and
    /// the UI/UX docs at the same time) don't collide on one base CLI session.
    /// `None` = this runtime can't be safely forked (offline / generic), so the
    /// caller must fall back to sequential execution. The host CLI drivers
    /// override this to clone themselves with a reset session.
    fn fork(&self) -> Option<Box<dyn Runtime>> {
        None
    }

    /// One completion turn.
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, RuntimeError>;

    /// Streaming completion turn. Calls `on_event` for each real-time event
    /// (text delta, tool use, tool result) as the worker produces output.
    /// Returns the final assembled response when the worker is done.
    ///
    /// The default implementation simply calls [`complete`](Self::complete)
    /// and emits a single [`StreamEvent::Text`] — so non-streaming runtimes
    /// (offline, HTTP) work unchanged. Host CLI drivers override this to
    /// parse the live JSONL event stream from `claude --output-format
    /// stream-json` / `codex --json`.
    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<CompletionResponse, RuntimeError> {
        let resp = self.complete(req).await?;
        if !resp.text.is_empty() {
            on_event(StreamEvent::Text {
                delta: resp.text.clone(),
            });
        }
        Ok(resp)
    }
}

/// A single real-time event from a streaming worker.
///
/// Host CLI drivers (Claude Code `--output-format stream-json`, Codex
/// `--json`) emit newline-delimited JSON. Each line is parsed into one of
/// these variants so the TUI can show live progress — the user sees
/// "[tool] Reading src/app.tsx..." and "[write] Writing..." instead of a blank spinner.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum StreamEvent {
    /// A chunk of assistant text (partial — concatenate for the full message).
    Text {
        /// UTF-8 text delta since the last event.
        delta: String,
    },
    /// The worker invoked a tool (read file, write file, run bash, search).
    /// `name` is the tool id ("Read", "Write", "Bash", "Grep", …);
    /// `detail` is a human-readable summary (file path, command, query).
    ToolUse {
        /// Tool name (e.g. "Read", "Write", "Bash").
        name: String,
        /// Human-readable description (file path, command).
        detail: String,
    },
    /// The worker received a result from a tool call.
    /// `ok` = success/failure; `summary` is a truncated result preview.
    ToolResult {
        /// Whether the tool call succeeded.
        ok: bool,
        /// Truncated result preview (first ~200 chars).
        summary: String,
    },
    /// Non-fatal warning from the worker (rate limit, retried call, …).
    Warning {
        /// Warning message.
        message: String,
    },
    /// The worker is in extended thinking mode (Claude's `thinking` blocks).
    /// Emitted once when a thinking block starts so the TUI can show a
    /// `[thinking] thinking...` indicator. No text payload — the thinking content is
    /// private and not displayed.
    Thinking,
}

/// Lets a boxed runtime be used wherever a concrete `Runtime` is
/// expected — e.g. `AgentRunner<Box<dyn Runtime>>`, which the TUI uses
/// to pick its brain (offline / host CLI) at runtime.
#[async_trait]
impl Runtime for Box<dyn Runtime> {
    fn kind(&self) -> RuntimeKind {
        (**self).kind()
    }

    // These three carry the brain's real powers — forwarding is NOT optional.
    // Without it a boxed runtime (the TUI drives `AgentRunner<Box<dyn Runtime>>`)
    // silently reports the trait DEFAULTS: fork()=None kills the parallel docs
    // fan-out, capabilities()=all-false disables persistent /goal + realtime
    // governance + usage/streaming. Mirrors the `Box<dyn HostDriver>` fork fix.
    fn capabilities(&self) -> BrainCapabilities {
        (**self).capabilities()
    }

    fn is_offline(&self) -> bool {
        (**self).is_offline()
    }

    fn fork(&self) -> Option<Box<dyn Runtime>> {
        (**self).fork()
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, RuntimeError> {
        (**self).complete(req).await
    }

    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<CompletionResponse, RuntimeError> {
        (**self).complete_streaming(req, on_event).await
    }
}

/// A runtime that never touches the network: every completion returns
/// an empty body, so the pipeline falls back to deterministic
/// templates. This is what gets used when no host CLI is selected.
#[derive(Debug, Clone, Copy)]
pub struct OfflineRuntime {
    kind: RuntimeKind,
}

impl OfflineRuntime {
    /// Build an offline runtime that reports `kind` (for audit labels).
    #[must_use]
    pub fn new(kind: RuntimeKind) -> Self {
        Self { kind }
    }
}

impl Default for OfflineRuntime {
    fn default() -> Self {
        Self {
            kind: RuntimeKind::Anthropic,
        }
    }
}

#[async_trait]
impl Runtime for OfflineRuntime {
    fn kind(&self) -> RuntimeKind {
        self.kind
    }

    fn is_offline(&self) -> bool {
        true
    }

    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, RuntimeError> {
        Ok(CompletionResponse {
            text: String::new(),
            id: "offline".to_string(),
            model: "offline".to_string(),
            usage: Usage::default(),
        })
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Continuous-session driving (the long-session model — see
// docs/CONTINUOUS_SESSION_ARCHITECTURE.md). This is ADDITIVE and lives
// ALONGSIDE the single-shot `Runtime` trait above; it does not replace it.
// Where `Runtime::complete` is "prompt in → one text blob out" (a fresh,
// stateless base process per call), `BaseSession` is "one long-lived base
// session, inject a directive per phase, observe a stream of tool-call /
// text / done events". The base keeps context across phases and runs its own
// agentic tool loop (it WRITES files), instead of narrating a paragraph and
// asking "shall I continue?".
// ───────────────────────────────────────────────────────────────────────────

/// How a turn ended — the authoritative "this phase is done" signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStatus {
    /// The base finished the turn cleanly (e.g. claude `result.success` with
    /// `stop_reason == "end_turn"`).
    Completed,
    /// The base hit a turn/budget ceiling mid-work (e.g. claude
    /// `error_max_turns`) — partial work may exist; not a clean finish.
    Truncated,
    /// The turn was interrupted (ESC / abort / a parent-initiated stop).
    Interrupted,
    /// The turn failed (base error, an unparseable stream, or the session
    /// process died mid-turn). Carries a human-readable reason. **Fail-open:
    /// the session surfaces a failure as this status, never a panic.**
    Failed(String),
}

/// A single event observed from a live [`BaseSession`] turn.
///
/// `ToolCall` (and the file system it mutates) is the SOURCE OF TRUTH for what
/// the base actually did — `TextDelta` is only what it *said*. Governance
/// auditing, the "real code produced" hard gate, and the TUI tool rows all key
/// off `ToolCall`, not `TextDelta`.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// A chunk of assistant text (concatenate for the full message).
    TextDelta(String),
    /// The base invoked a tool — `name` is the tool id (`Write`/`Edit`/`Bash`/
    /// `Read`/…), `input` the raw tool input (e.g. `{"file_path": "..."}`).
    /// This is where a real file write shows up.
    ToolCall {
        /// Tool id (`Write`, `Edit`, `Bash`, `Read`, …).
        name: String,
        /// Raw tool input as the base reported it.
        input: serde_json::Value,
    },
    /// A tool returned. `ok` = success/failure, `summary` a truncated preview.
    ToolResult {
        /// Whether the tool call succeeded.
        ok: bool,
        /// Truncated result preview.
        summary: String,
    },
    /// The base is asking permission for a (potentially dangerous) action —
    /// the orchestrator must answer via [`BaseSession::respond`]. Maps to
    /// claude `can_use_tool` / codex `requestApproval` / opencode
    /// `permission.asked`. This is the wiring point for the confirm gates.
    NeedApproval {
        /// Correlates with the [`BaseSession::respond`] reply.
        req_id: String,
        /// What it wants to do (tool id / action class).
        action: String,
        /// The target (file path / command / resource).
        target: String,
    },
    /// The current turn ended — see [`TurnStatus`]. After this the orchestrator
    /// either sends the next phase's directive (same session, context retained)
    /// or stops at a gate.
    TurnDone {
        /// How the turn ended.
        status: TurnStatus,
    },
}

/// A decision handed back to the base for a [`SessionEvent::NeedApproval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow the action.
    Allow,
    /// Deny the action (the base gets an error result and continues / stops).
    Deny,
}

/// Errors a continuous session can surface.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Failed to start the base session process / server.
    #[error("session start: {0}")]
    Start(String),
    /// Failed to write a directive / control message to the live session
    /// (e.g. the base process already exited).
    #[error("session send: {0}")]
    Send(String),
    /// The session has ended (process exited / EOF) and can take no more turns.
    #[error("session closed")]
    Closed,
    /// This base can't open a read-only fork (the underlying CLI has no native
    /// fork / read-only-session form, or the fork attempt failed). **Fail-open
    /// signal:** the caller degrades to the existing single-runtime consult path,
    /// never blocks. The string is a human-readable reason.
    #[error("session fork unsupported: {0}")]
    ForkUnsupported(String),
}

/// A long-lived base session that the 9-phase runner drives one phase at a
/// time. ONE session spans an entire run; context flows research → docs →
/// code without re-priming. See `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`.
///
/// Contract:
/// - [`send_turn`](Self::send_turn) injects a phase directive (imperative).
/// - [`next_event`](Self::next_event) is then polled until it yields a
///   [`SessionEvent::TurnDone`]; that marks the phase complete. `None` means
///   the session itself ended (process dead) — treat as a failed turn.
/// - [`respond`](Self::respond) answers a [`SessionEvent::NeedApproval`].
/// - [`interrupt`](Self::interrupt) aborts the in-flight turn (ESC / timeout).
/// - [`end`](Self::end) closes the session.
///
/// **Fail-open by contract:** a dead/garbled session surfaces a
/// [`TurnStatus::Failed`] (or `next_event` → `None`), never a panic — a driver
/// bug must never crash the host.
#[async_trait]
pub trait BaseSession: Send {
    /// Open a READ-ONLY forked session for a review role (the critic team).
    ///
    /// The fork is a SEPARATE, isolated session a critic seat drives to review
    /// the main line's on-disk output — it MUST never write the workspace and
    /// MUST never collide with the main writer session (the single-writer
    /// invariant). Each base implements it with its own native read-only form:
    /// claude `--fork-session` + `--permission-mode plan`, codex
    /// `thread/fork {ephemeral:true}`, opencode a fresh independent read-only
    /// `POST /session`. The returned session is a normal [`BaseSession`]: the
    /// caller injects one strict-JSON judge directive via
    /// [`send_turn`](Self::send_turn), drains [`next_event`](Self::next_event)
    /// for the verdict text, then [`end`](Self::end)s it.
    ///
    /// **Fail-open by contract:** a base with no fork form — or a fork that
    /// fails to start — returns [`SessionError::ForkUnsupported`]. The caller
    /// degrades to its existing single-runtime read-only consult path and NEVER
    /// blocks. The default impl returns `ForkUnsupported` so a session that
    /// hasn't implemented a fork still compiles and degrades safely.
    ///
    /// Takes `&mut self` (not `&self`) so the returned future is `Send` without
    /// requiring `Self: Sync` — the host sessions hold non-`Sync` channels
    /// (`mpsc::Receiver`). The fork is itself a fresh, independent session, so it
    /// never aliases the parent; the `&mut` is just borrow plumbing.
    async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
        Err(SessionError::ForkUnsupported(
            "this base session does not support read-only forks".to_string(),
        ))
    }

    /// Inject one phase directive into the live session, starting a turn.
    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError>;

    /// Pull the next event of the in-flight turn. Yields events until a
    /// [`SessionEvent::TurnDone`]; `None` once the underlying session ends.
    async fn next_event(&mut self) -> Option<SessionEvent>;

    /// Answer a [`SessionEvent::NeedApproval`] (governance / gate decision).
    async fn respond(
        &mut self,
        req_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError>;

    /// Abort the in-flight turn (ESC / abort / timeout).
    async fn interrupt(&mut self) -> Result<(), SessionError>;

    /// Close the session and release the underlying process / server.
    async fn end(&mut self) -> Result<(), SessionError>;
}
