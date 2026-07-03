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

/// A structured file edit pulled off a write/edit tool call, so the TUI can
/// render a real diff card (the Claude-Code "code added / code changed" feel)
/// instead of a one-line `Write src/app.tsx` row.
///
/// `before`/`after` are the **full** file regions the base reported:
/// - Claude `Edit` → `before = old_string`, `after = new_string` (a patch hunk).
/// - Claude `Write` → `before = ""`, `after = content` (a brand-new / replaced
///   file, all additions).
///
/// **Fail-open:** populated ONLY when the base hands us the actual content. A
/// tool that carries just a path (codex `file_change`, opencode's scraped
/// gutter) leaves [`StreamEvent::ToolUse::edit`] = `None`, and the TUI degrades
/// to the ordinary tool row — never a wrong or empty diff.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ToolEdit {
    /// Target file path as the base reported it.
    pub path: String,
    /// File region BEFORE the edit (empty for a fresh `Write`).
    pub before: String,
    /// File region AFTER the edit.
    pub after: String,
}

impl ToolEdit {
    /// Pull a structured edit off a Claude-shaped tool-call input (the JSON the
    /// base reports for `Edit` / `MultiEdit` / `Write`), so the TUI can render a
    /// diff card. Shared by every driver/loop that sees that input shape (the
    /// `umadev-host` claude driver AND the continuous-session director loop,
    /// whose `SessionEvent::ToolCall.input` is the same JSON).
    ///
    /// - `Edit` → `before = old_string`, `after = new_string`.
    /// - `MultiEdit` → the first edit of the batch (a representative card).
    /// - `Write` → `before = ""`, `after = content` (all additions).
    /// - any other tool / unreadable fields → `None`.
    ///
    /// **Fail-open by construction:** every field is fetched with `?`, so a
    /// missing/malformed input yields `None` — never a panic, never a
    /// fabricated diff.
    #[must_use]
    pub fn from_claude_tool_input(name: &str, input: &serde_json::Value) -> Option<Self> {
        let str_field = |k: &str| input.get(k).and_then(|s| s.as_str());
        match name {
            "Edit" => Some(Self {
                path: str_field("file_path")?.to_string(),
                before: str_field("old_string")?.to_string(),
                after: str_field("new_string")?.to_string(),
            }),
            "MultiEdit" => {
                let path = str_field("file_path")?.to_string();
                let first = input
                    .get("edits")
                    .and_then(|e| e.as_array())
                    .and_then(|arr| arr.first())?;
                Some(Self {
                    path,
                    before: first
                        .get("old_string")
                        .and_then(|s| s.as_str())?
                        .to_string(),
                    after: first
                        .get("new_string")
                        .and_then(|s| s.as_str())?
                        .to_string(),
                })
            }
            "Write" => Some(Self {
                path: str_field("file_path")?.to_string(),
                before: String::new(),
                after: str_field("content")?.to_string(),
            }),
            _ => None,
        }
    }
}

/// Extract the **full** proposed content a mutating base tool would write, so the
/// governance content scan (secret floor / craft rules) sees every byte that
/// would land — never an empty string for a tool it "supports".
///
/// This is deliberately **distinct** from [`ToolEdit::from_claude_tool_input`],
/// which lifts only a *representative* first hunk for the TUI diff card. A
/// content scan must see EVERYTHING: a secret inlined into a later `MultiEdit`
/// hunk, or into a notebook cell, must not hide behind a first-hunk-only read.
///
/// Shapes handled (mutually exclusive in a real payload, tried in this order):
/// - `MultiEdit` = `{file_path, edits: [{old_string, new_string}, …]}` →
///   concatenation of **all** `edits[].new_string`, newline-joined.
/// - `Write` / `create_file` = `{file_path, content}` → `content`.
/// - `NotebookEdit` = `{notebook_path, new_source, …}` → `new_source`.
/// - `Edit` = `{file_path, old_string, new_string}` → `new_string`.
/// - codex / opencode alt → `new_str`.
///
/// **Fail-open:** a malformed / absent payload yields `String::new()`, so the
/// caller scans `""` (today's no-op) — never a panic, never a fabricated body.
#[must_use]
pub fn write_scan_content(input: &serde_json::Value) -> String {
    // MultiEdit: the batch of hunks lives in `edits[]` with NO top-level content.
    // Join every hunk's `new_string` so a secret in `edits[1..]` can't slip past
    // a first-hunk-only read.
    if let Some(edits) = input.get("edits").and_then(serde_json::Value::as_array) {
        let joined: Vec<&str> = edits
            .iter()
            .filter_map(|e| e.get("new_string").and_then(serde_json::Value::as_str))
            .collect();
        if !joined.is_empty() {
            return joined.join("\n");
        }
    }
    // Single-body shapes: Write (`content`), NotebookEdit (`new_source`),
    // Edit (`new_string`), codex/opencode (`new_str`). First present wins.
    for key in ["content", "new_source", "new_string", "new_str"] {
        if let Some(s) = input.get(key).and_then(serde_json::Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

/// The target path a mutating base tool would write, for governance scoping.
/// `Write` / `Edit` / `MultiEdit` carry `file_path`; `NotebookEdit` carries
/// `notebook_path`; codex / opencode `update` / `create` carry `path`. First
/// present wins; **fail-open** to `""` when none is present.
#[must_use]
pub fn write_scan_path(input: &serde_json::Value) -> String {
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(s) = input.get(key).and_then(serde_json::Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

/// One labeled option the base offered inside an `AskUserQuestion` call.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AskOption {
    /// The short label the user picks — what the base expects back as the answer.
    pub label: String,
    /// Optional one-line description of the option (may be empty).
    pub description: String,
}

/// One question inside a base `AskUserQuestion` call: the prompt + its options.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AskQuestion {
    /// Short header (e.g. "Database"). May be empty.
    pub header: String,
    /// The full question text the base wants answered. May be empty if only a
    /// header was supplied.
    pub question: String,
    /// Whether the base allows more than one option to be chosen.
    pub multi_select: bool,
    /// The labeled options (typically 2–4).
    pub options: Vec<AskOption>,
}

/// The parsed input of a base's **`AskUserQuestion`** tool call — the
/// interactive multiple-choice question(s) the base asks the user.
///
/// UmaDev drives the base **non-interactively** (claude `--print` / the
/// continuous stream-json session), so the base cannot render its own
/// interactive picker and its `AskUserQuestion` auto-cancels mid-turn. UmaDev
/// only observes the tool-call event — so without this parser the question +
/// its options are never shown and the turn silently reads as "cancelled". This
/// type lifts the question + options out of the raw tool input so the surface
/// layers can render them and relay the user's choice back as the next turn.
///
/// **Fail-open by construction:** [`from_tool_input`](Self::from_tool_input)
/// tolerates both the `{"questions":[…]}` shape and a single top-level question,
/// tolerates string-or-object options, and returns `None` on any shape it can't
/// read — never a panic, never a fabricated question.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AskUserQuestion {
    /// The question(s) the base asked (usually one).
    pub questions: Vec<AskQuestion>,
}

impl AskUserQuestion {
    /// Whether `name` is the base's interactive question tool. Case-insensitive
    /// so a normalized (`ask_user_question`) or canonical (`AskUserQuestion`)
    /// name both match.
    #[must_use]
    pub fn is_tool_name(name: &str) -> bool {
        let n = name.replace(['_', '-', ' '], "").to_ascii_lowercase();
        n == "askuserquestion"
    }

    /// Parse a base `AskUserQuestion` tool-call input. Returns `None` when the
    /// call is not an `AskUserQuestion` or the input has no readable question, so
    /// the caller keeps its existing (non-question) tool-row rendering.
    #[must_use]
    pub fn from_tool_input(name: &str, input: &serde_json::Value) -> Option<Self> {
        if !Self::is_tool_name(name) {
            return None;
        }
        Self::parse_value(input)
    }

    /// Parse just the input value (no tool-name guard) — the shared body of
    /// [`from_tool_input`], also handy when the name was already matched upstream.
    #[must_use]
    pub fn parse_value(input: &serde_json::Value) -> Option<Self> {
        let questions: Vec<AskQuestion> =
            if let Some(arr) = input.get("questions").and_then(|q| q.as_array()) {
                arr.iter().filter_map(parse_ask_question).collect()
            } else {
                // Older / flattened single-question shape at the top level.
                parse_ask_question(input).into_iter().collect()
            };
        if questions.is_empty() {
            return None;
        }
        Some(Self { questions })
    }

    /// A compact, single-line summary for the tool-row `detail` arg (the row is
    /// one line, so this never contains a newline). Prefers the first question's
    /// header, then its text, and appends a `(+N)` when more questions follow.
    #[must_use]
    pub fn summary(&self) -> String {
        let Some(first) = self.questions.first() else {
            return String::new();
        };
        let head = if first.header.is_empty() {
            first.question.as_str()
        } else {
            first.header.as_str()
        };
        let head = one_line_clip(head, 72);
        let extra = self.questions.len().saturating_sub(1);
        if extra > 0 {
            format!("{head} (+{extra})")
        } else {
            head
        }
    }

    /// A readable MULTI-LINE block: each question followed by its numbered
    /// options (`1. label — description`). Neutral structural text only (no UI
    /// verbs, no localized words) so a localized framing can wrap it.
    #[must_use]
    pub fn prompt_block(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let multi = self.questions.len() > 1;
        for (qi, q) in self.questions.iter().enumerate() {
            if !out.is_empty() {
                out.push('\n');
            }
            let title = match (q.header.is_empty(), q.question.is_empty()) {
                (false, false) => format!("{}: {}", q.header, q.question),
                (true, false) => q.question.clone(),
                (false, true) => q.header.clone(),
                (true, true) => String::new(),
            };
            // `write!` to a String is infallible — the `let _ =` keeps clippy's
            // `format_push_string` happy without an extra allocation.
            if multi {
                let _ = write!(out, "Q{}. {title}", qi + 1);
            } else {
                out.push_str(&title);
            }
            for (oi, opt) in q.options.iter().enumerate() {
                out.push('\n');
                if opt.description.is_empty() {
                    let _ = write!(out, "  {}. {}", oi + 1, opt.label);
                } else {
                    let _ = write!(out, "  {}. {} — {}", oi + 1, opt.label, opt.description);
                }
            }
        }
        out
    }

    /// Resolve a user's free-text reply to the answer to relay back to the base.
    ///
    /// - A bare option **number** (1-based, against the FIRST question's options
    ///   — the common single-question case) resolves to that option's label.
    /// - A reply that case-insensitively equals an option label resolves to that
    ///   option's canonical label.
    /// - Anything else passes through trimmed (free-text is always honored).
    ///
    /// Pure + fail-open: an out-of-range number or an empty option list just
    /// returns the trimmed reply.
    #[must_use]
    pub fn resolve_reply(&self, reply: &str) -> String {
        let trimmed = reply.trim();
        let opts = self.questions.first().map(|q| &q.options);
        if let Some(opts) = opts.filter(|o| !o.is_empty()) {
            if let Ok(n) = trimmed.parse::<usize>() {
                if n >= 1 && n <= opts.len() {
                    return opts[n - 1].label.clone();
                }
            }
            if let Some(hit) = opts.iter().find(|o| o.label.eq_ignore_ascii_case(trimmed)) {
                return hit.label.clone();
            }
        }
        trimmed.to_string()
    }
}

/// Parse one question object into an [`AskQuestion`], or `None` when it carries
/// neither a question text nor any option (nothing to ask).
fn parse_ask_question(v: &serde_json::Value) -> Option<AskQuestion> {
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(|s| s.as_str()).unwrap_or("").trim();
    let question = {
        let q = get("question");
        if q.is_empty() {
            get("prompt")
        } else {
            q
        }
    }
    .to_string();
    let header = get("header").to_string();
    let multi_select = obj
        .get("multiSelect")
        .or_else(|| obj.get("multi_select"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let options = obj
        .get("options")
        .and_then(|o| o.as_array())
        .map(|arr| arr.iter().filter_map(parse_ask_option).collect::<Vec<_>>())
        .unwrap_or_default();
    if question.is_empty() && header.is_empty() && options.is_empty() {
        return None;
    }
    Some(AskQuestion {
        header,
        question,
        multi_select,
        options,
    })
}

/// Parse one option — a bare string OR a `{label, description}` object — into an
/// [`AskOption`], or `None` when it carries no usable label.
fn parse_ask_option(v: &serde_json::Value) -> Option<AskOption> {
    match v {
        serde_json::Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| AskOption {
                label: s.to_string(),
                description: String::new(),
            })
        }
        serde_json::Value::Object(_) => {
            let pick = |k: &str| v.get(k).and_then(|s| s.as_str()).map(str::trim);
            let label = pick("label")
                .or_else(|| pick("value"))
                .or_else(|| pick("text"))
                .unwrap_or("");
            if label.is_empty() {
                return None;
            }
            Some(AskOption {
                label: label.to_string(),
                description: pick("description").unwrap_or("").to_string(),
            })
        }
        _ => None,
    }
}

/// Collapse whitespace + clip a string to `max` chars (char-boundary safe), for
/// a single-line summary that never wraps or splits a multibyte codepoint.
fn one_line_clip(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &one[..idx]),
        None => one,
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
    /// `detail` is a human-readable summary (file path, command, query);
    /// `edit` carries the structured before/after for a `Write`/`Edit` whose
    /// content the base actually exposed (so the TUI can draw a diff card),
    /// and is `None` for every other tool / when the content wasn't available.
    ToolUse {
        /// Tool name (e.g. "Read", "Write", "Bash").
        name: String,
        /// Human-readable description (file path, command).
        detail: String,
        /// Structured edit for a `Write`/`Edit` with content; `None` otherwise.
        /// **Fail-open:** `None` simply degrades to the plain tool row.
        edit: Option<ToolEdit>,
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
    /// The worker is in extended thinking mode (Claude's `thinking` blocks) but
    /// exposed NO reasoning text — a content-less pulse so the TUI can open a
    /// `[thinking]` indicator + spinner. Bases that DO expose the reasoning text
    /// emit [`StreamEvent::ThinkingDelta`] instead (which also opens the block).
    Thinking,
    /// A chunk of the worker's REASONING text (Claude's extended-thinking
    /// `thinking` content — partial; concatenate for the full reasoning). Parallels
    /// [`StreamEvent::Text`] but routes to a COLLAPSED `[thinking]` transcript block
    /// the user can expand (the global Ctrl+O verbose toggle / Ctrl+R), so the
    /// base's private chain of thought is visible for transparency without flooding
    /// the answer stream. The first delta opens the block; later deltas accumulate
    /// into the SAME one foldable block (never a row per delta).
    ThinkingDelta(String),
}

impl StreamEvent {
    /// Build a [`StreamEvent::ToolUse`] with no structured edit (`edit: None`) —
    /// the common case (Read / Bash / Grep / a path-only write). Keeps the many
    /// call sites that don't have before/after content from repeating
    /// `edit: None`.
    #[must_use]
    pub fn tool_use(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::ToolUse {
            name: name.into(),
            detail: detail.into(),
            edit: None,
        }
    }
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

/// Build a **context-aware, non-silent** offline chat reply from a request.
///
/// The offline runtime owns no model, so [`OfflineRuntime::complete`] returns an
/// empty body on purpose — the *pipeline* relies on that empty body to fall back
/// to its deterministic artifact templates (see the crate doc + the `is_empty()`
/// checks in `umadev-agent::phases`). But a *chat* turn driven offline must NOT
/// read as silence: an empty reply leaves the user staring at a dead prompt
/// (Wave 5 / gap G11, "offline chat returns empty").
///
/// This is the chat-side counterpart: given the same [`CompletionRequest`], it
/// returns a short, deterministic, **context-aware** acknowledgement that names
/// the user's last ask back to them and points at the real fix (select a base
/// CLI), so the offline chat surface stays honest and responsive without
/// pretending to think. The caller (the TUI chat path) uses it ONLY when the
/// brain `is_offline()` and the streamed body came back empty — so the pipeline's
/// empty-body template contract is untouched.
///
/// Fail-open by construction: it never errors and never returns an empty string
/// (an empty / whitespace-only task still yields the base-less guidance line).
#[must_use]
pub fn offline_chat_reply(req: &CompletionRequest) -> String {
    // The user's last turn is the most-recent `user` message (the actual ask);
    // fall back to the final message of any role, then to empty.
    let last_user = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .or_else(|| req.messages.last())
        .map_or("", |m| m.content.trim());
    // A compact, single-line echo of the ask so the reply is visibly ABOUT what
    // the user said (context-aware), capped so a long paste doesn't flood the
    // transcript. Char-boundary safe (`char_indices`), so multibyte CJK is never
    // split mid-codepoint.
    let echo: String = {
        let one_line: String = last_user.split_whitespace().collect::<Vec<_>>().join(" ");
        match one_line.char_indices().nth(120) {
            Some((idx, _)) => format!("{}…", &one_line[..idx]),
            None => one_line,
        }
    };
    if echo.is_empty() {
        // No ask to echo — still never silent: the base-less guidance line.
        "[offline] No base CLI is connected, so I can't think this through yet. \
         Pick a base with /claude, /codex or /opencode (or run `umadev` again to \
         re-open the picker), and I'll pick up the conversation with full context. \
         離線:尚未连接底座 CLI / 離線:尚未連接底座 CLI。"
            .to_string()
    } else {
        // Echo the ask + the concrete next step. Bilingual tail mirrors the rest
        // of the offline surfaces (the catalogs gate full i18n; this is the
        // runtime-crate floor, which has no i18n dep on purpose).
        format!(
            "[offline] I heard: \u{201c}{echo}\u{201d} — but no base CLI is connected, \
             so I can't actually work on it yet. Connect a base with /claude, /codex \
             or /opencode and ask again; I'll keep this conversation's context. \
             離線:已收到你的需求,但尚未连接底座 / 已收到你的需求,但尚未連接底座。"
        )
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
        // Intentionally empty: the PIPELINE path keys off an empty offline body to
        // fall back to its deterministic artifact templates. The CHAT path never
        // ships this silence — it calls [`offline_chat_reply`] when the brain is
        // offline and the body is empty (Wave 5 / G11). Do not return text here.
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
    /// A chunk of the base's REASONING text (Claude extended-thinking
    /// `thinking_delta` blocks — partial; concatenate for the full reasoning).
    /// Parallels [`SessionEvent::TextDelta`] but is the base's private chain of
    /// thought, surfaced to the TUI as a COLLAPSED `[thinking]` block (transparency
    /// without polluting the answer). **Fail-open:** a base that exposes no thinking
    /// simply never emits this; an unparseable thinking frame is skipped, never a
    /// panic.
    ThinkingDelta(String),
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
        /// REAL token usage reported by the base for this turn, when the base's
        /// live protocol carries it (claude's stream-json `result` line; codex's
        /// `turn/completed` / `thread/tokenUsage/updated` notification). `None`
        /// when the base does not report per-turn usage on its live stream
        /// (opencode's SSE carries none) — the consumer then falls back to a
        /// deterministic `chars/4` estimate so `/usage` stays non-empty but
        /// honest. **Fail-open:** an unparseable usage payload yields `None`,
        /// never a wrong number and never a panic.
        usage: Option<Usage>,
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

    /// A bounded tail of the base's STDERR — the diagnostic the TUI surfaces to
    /// tell the user *why* a base went idle (a bad model id, "not logged in", a
    /// config error the base prints to stderr before falling silent). Without
    /// this the user only ever sees "base session idle." with no cause.
    ///
    /// Returns the last few captured stderr lines (driver-bounded, e.g. ~20
    /// lines / ~4KB), or `None` when nothing was captured. **Fail-open:** the
    /// default returns `None`; capturing must never block the stdout reader or
    /// the host, so a contended / empty buffer just yields `None`.
    fn stderr_tail(&self) -> Option<String> {
        None
    }

    /// The base child's exit status if it has already exited, else `None` (still
    /// alive, or no child / unknown). Lets a caller distinguish "the base
    /// process died" from "alive but silent" when a session goes idle.
    ///
    /// **Fail-open:** the default returns `None`; an implementation does a
    /// non-blocking `try_wait()` and maps any error to `None` — it never blocks
    /// and never reports a false exit.
    fn try_exit_status(&self) -> Option<std::process::ExitStatus> {
        None
    }

    /// The base's OWN persisted conversation id, if this session has one — the
    /// load-bearing pointer for **full-context cross-session resume**. Every
    /// first-class base persists its transcript under this id (claude's pinned
    /// `--session-id`, codex's `thread.id`, opencode's server-side `ses_…`), so
    /// persisting just this id lets a later `/continue` re-open the SAME base
    /// conversation via `--resume` / `thread/resume` instead of cold-priming a
    /// fresh brain that "forgot the task." Near-zero extra storage: a ~36-byte id.
    ///
    /// **Fail-open:** the default returns `None`; a session that cannot expose a
    /// resumable id (or hasn't captured one yet) simply yields `None`, and the
    /// caller degrades to a fresh session — a resume is best-effort, never
    /// required.
    fn session_id(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(messages: Vec<(&str, &str)>) -> CompletionRequest {
        CompletionRequest {
            model: "offline".into(),
            messages: messages
                .into_iter()
                .map(|(role, content)| Message {
                    role: role.into(),
                    content: content.into(),
                })
                .collect(),
            max_tokens: None,
            temperature: None,
            system: None,
        }
    }

    #[test]
    fn offline_chat_reply_echoes_the_last_user_ask_and_is_never_empty() {
        // Wave 5 / G11: offline chat must NOT return silence — the reply names the
        // user's ask back to them and points at the fix (connect a base).
        let r = offline_chat_reply(&req(vec![
            ("user", "build me a todo app"),
            ("assistant", "ok"),
            ("user", "actually make it a kanban board"),
        ]));
        assert!(!r.trim().is_empty());
        assert!(
            r.contains("kanban board"),
            "should echo the LAST user ask: {r}"
        );
        assert!(
            r.contains("/claude"),
            "should point at connecting a base: {r}"
        );
    }

    #[test]
    fn offline_chat_reply_with_no_ask_still_guides_to_a_base() {
        // An empty / whitespace-only ask still yields the base-less guidance line,
        // never an empty string.
        let r = offline_chat_reply(&req(vec![("user", "   ")]));
        assert!(!r.trim().is_empty());
        assert!(r.contains("/codex") || r.contains("base CLI"));
    }

    #[test]
    fn offline_chat_reply_caps_a_long_ask_on_a_char_boundary() {
        // A long multibyte ask is truncated with an ellipsis on a char boundary
        // (no panic from slicing mid-codepoint).
        let long = "用".repeat(300);
        let r = offline_chat_reply(&req(vec![("user", &long)]));
        assert!(!r.is_empty());
        assert!(r.contains('\u{2026}'), "long ask should be elided: {r}");
    }

    #[test]
    fn tool_use_helper_defaults_edit_to_none() {
        // The convenience constructor is the common (no-content) path: a Read /
        // Bash row carries no diff payload.
        let ev = StreamEvent::tool_use("Read", "src/app.rs");
        assert_eq!(
            ev,
            StreamEvent::ToolUse {
                name: "Read".into(),
                detail: "src/app.rs".into(),
                edit: None,
            }
        );
    }

    #[test]
    fn tool_edit_carries_before_and_after() {
        // A structured edit round-trips its before/after so the TUI can diff it.
        let ev = StreamEvent::ToolUse {
            name: "Edit".into(),
            detail: "src/lib.rs".into(),
            edit: Some(ToolEdit {
                path: "src/lib.rs".into(),
                before: "let x = 1;".into(),
                after: "let x = 2;".into(),
            }),
        };
        let StreamEvent::ToolUse { edit: Some(e), .. } = ev else {
            panic!("expected an edit-bearing ToolUse");
        };
        assert_eq!(e.path, "src/lib.rs");
        assert_eq!(e.before, "let x = 1;");
        assert_eq!(e.after, "let x = 2;");
    }

    // ── write_scan_content / write_scan_path (governance content extraction) ──

    #[test]
    fn write_scan_content_concatenates_all_multiedit_hunks() {
        // A real MultiEdit carries NO top-level content; every hunk's `new_string`
        // must be scanned so a secret in a later hunk can't hide behind the first.
        let input = serde_json::json!({
            "file_path": "src/cfg.js",
            "edits": [
                { "old_string": "a", "new_string": "let a = 1;" },
                { "old_string": "b", "new_string": "let token = SECRET;" }
            ]
        });
        let content = write_scan_content(&input);
        assert_eq!(content, "let a = 1;\nlet token = SECRET;");
        assert_eq!(write_scan_path(&input), "src/cfg.js");
    }

    #[test]
    fn write_scan_content_reads_notebook_new_source() {
        // A real NotebookEdit carries its cell body in `new_source` (NOT `content`)
        // and its path in `notebook_path` (NOT `file_path`).
        let input = serde_json::json!({
            "notebook_path": "analysis.ipynb",
            "new_source": "key = SECRET"
        });
        assert_eq!(write_scan_content(&input), "key = SECRET");
        assert_eq!(write_scan_path(&input), "analysis.ipynb");
    }

    #[test]
    fn write_scan_content_preserves_write_and_edit_shapes() {
        // Write → `content`; Edit → `new_string`; codex/opencode alt → `new_str`.
        let write = serde_json::json!({ "file_path": "a.ts", "content": "W" });
        assert_eq!(write_scan_content(&write), "W");
        let edit = serde_json::json!({ "file_path": "a.ts", "new_string": "E" });
        assert_eq!(write_scan_content(&edit), "E");
        let alt = serde_json::json!({ "path": "a.ts", "new_str": "A" });
        assert_eq!(write_scan_content(&alt), "A");
        assert_eq!(write_scan_path(&alt), "a.ts");
    }

    #[test]
    fn write_scan_content_fails_open_on_malformed_payload() {
        // Absent / wrong-typed fields yield "" (today's no-op scan), never a panic.
        assert_eq!(write_scan_content(&serde_json::json!({})), "");
        assert_eq!(write_scan_content(&serde_json::json!({ "edits": [] })), "");
        assert_eq!(
            write_scan_content(&serde_json::json!({ "edits": [{ "old_string": "x" }] })),
            ""
        );
        assert_eq!(
            write_scan_content(&serde_json::json!({ "content": 42 })),
            ""
        );
        assert_eq!(write_scan_path(&serde_json::json!({})), "");
    }

    #[tokio::test]
    async fn offline_complete_stays_empty_for_the_pipeline_template_contract() {
        // The pipeline keys off an EMPTY offline body to fall back to templates;
        // `complete` must keep returning empty even though chat has its own reply.
        let rt = OfflineRuntime::default();
        let resp = rt.complete(req(vec![("user", "hi")])).await.unwrap();
        assert!(resp.text.is_empty());
    }

    // ── AskUserQuestion parsing / rendering / reply-resolution ──────────────

    #[test]
    fn ask_user_question_parses_questions_and_options() {
        let input = serde_json::json!({
            "questions": [{
                "header": "Database",
                "question": "Which database should the API use?",
                "multiSelect": false,
                "options": [
                    {"label": "Postgres", "description": "Relational, strong consistency"},
                    {"label": "MongoDB", "description": "Document store"},
                    {"label": "SQLite"}
                ]
            }]
        });
        let q = AskUserQuestion::from_tool_input("AskUserQuestion", &input)
            .expect("AskUserQuestion input must parse");
        assert_eq!(q.questions.len(), 1);
        let only = &q.questions[0];
        assert_eq!(only.header, "Database");
        assert_eq!(only.question, "Which database should the API use?");
        assert_eq!(only.options.len(), 3);
        assert_eq!(only.options[0].label, "Postgres");
        assert_eq!(
            only.options[0].description,
            "Relational, strong consistency"
        );
        assert_eq!(only.options[2].label, "SQLite");
        assert!(only.options[2].description.is_empty());

        // The multi-line block shows the question AND every numbered option — the
        // fix for the bare "no options visible" stub.
        let block = q.prompt_block();
        assert!(block.contains("Which database"), "block: {block}");
        assert!(block.contains("1. Postgres"), "numbered options: {block}");
        assert!(block.contains("3. SQLite"), "every option present: {block}");
        // The one-line tool-row summary is never empty and never multi-line.
        let summary = q.summary();
        assert!(!summary.is_empty());
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn ask_user_question_tolerates_string_options_and_flat_shape() {
        // String options + a single top-level question (no `questions` array).
        let input = serde_json::json!({
            "question": "Pick a framework",
            "options": ["Next.js", "Remix"]
        });
        let q = AskUserQuestion::parse_value(&input).expect("flat shape parses");
        assert_eq!(q.questions[0].options.len(), 2);
        assert_eq!(q.questions[0].options[1].label, "Remix");
    }

    #[test]
    fn ask_user_question_name_match_and_fail_open() {
        assert!(AskUserQuestion::is_tool_name("AskUserQuestion"));
        assert!(AskUserQuestion::is_tool_name("ask_user_question"));
        assert!(!AskUserQuestion::is_tool_name("Write"));
        // A non-question tool / unreadable input fails open to None (callers keep
        // their existing tool-row detail; never a panic, never a fake question).
        assert!(
            AskUserQuestion::from_tool_input("Write", &serde_json::json!({"file_path":"a"}))
                .is_none()
        );
        assert!(AskUserQuestion::from_tool_input(
            "AskUserQuestion",
            &serde_json::json!({"questions":[]})
        )
        .is_none());
        assert!(
            AskUserQuestion::from_tool_input("AskUserQuestion", &serde_json::Value::Null).is_none()
        );
    }

    #[test]
    fn ask_user_question_resolve_reply_maps_number_and_label() {
        let q = AskUserQuestion {
            questions: vec![AskQuestion {
                header: "DB".into(),
                question: "Which?".into(),
                multi_select: false,
                options: vec![
                    AskOption {
                        label: "Postgres".into(),
                        description: String::new(),
                    },
                    AskOption {
                        label: "MongoDB".into(),
                        description: String::new(),
                    },
                ],
            }],
        };
        // A bare option number resolves to the option label (the picker shortcut).
        assert_eq!(q.resolve_reply("1"), "Postgres");
        assert_eq!(q.resolve_reply(" 2 "), "MongoDB");
        // An exact (case-insensitive) label resolves to the canonical label.
        assert_eq!(q.resolve_reply("postgres"), "Postgres");
        // Out-of-range / free-text passes through trimmed (free-text always honored).
        assert_eq!(q.resolve_reply("9"), "9");
        assert_eq!(
            q.resolve_reply("  use whichever is cheaper "),
            "use whichever is cheaper"
        );
    }
}
