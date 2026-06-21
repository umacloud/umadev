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
