//! `umadev-runtime` — the `Runtime` trait that every "brain" of the
//! pipeline implements.
//!
//! UmaDev is the **project-director Agent**. The actual coding work happens in one
//! of five first-class host CLI "brain" implementations behind this trait:
//!
//! - a logged-in host CLI driven as a subprocess by the `umadev-host` crate; or
//! - [`OfflineRuntime`], which returns empty bodies so the pipeline falls back
//!   to deterministic templates when no brain is selected.

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
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
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
    /// Whole-prompt usage. The default is explicitly incomplete/unknown when a
    /// backend does not report it; it is never interpreted as a free turn.
    #[serde(default)]
    pub usage: Usage,
}

/// The accounting scope of a reported usage snapshot.
///
/// A [`Usage`] is deliberately never a last-model-call sample. Host adapters may
/// observe such samples internally, but must not surface them as a completed
/// turn's usage because doing so silently under-counts multi-call prompts.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    /// Aggregate for the complete logical prompt/turn.
    #[default]
    WholePrompt,
}

/// Base-reported usage for one complete logical prompt/turn.
///
/// Token totals are `u64` because vendor ledgers use `u64`; narrowing them to
/// `u32` can turn a valid long-lived session into a fabricated smaller number.
/// Cache reads/writes are subsets of input and reasoning is a subset of output,
/// so none is added again when computing [`Self::total_tokens`].
///
/// `usage_incomplete` means the counters are a lower bound. A missing cost is
/// unknown, never free. Cost is trustworthy only when present and both
/// `usage_incomplete` and `cost_partial` are false.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Full input token count, including cache reads/writes after normalization.
    #[serde(default)]
    pub input_tokens: u64,
    /// Full output token count, including reasoning when the base reports it.
    #[serde(default)]
    pub output_tokens: u64,
    /// `input_tokens + output_tokens`, saturating at `u64::MAX`.
    #[serde(default)]
    pub total_tokens: u64,
    /// Cache-read subset of [`Self::input_tokens`].
    #[serde(default)]
    pub cached_read_tokens: u64,
    /// Cache-write/creation subset of [`Self::input_tokens`].
    #[serde(default)]
    pub cached_write_tokens: u64,
    /// Reasoning subset of [`Self::output_tokens`].
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Number of model calls folded into this whole-prompt report.
    #[serde(default)]
    pub model_calls: u64,
    /// Number of main-agent loop turns folded into this report.
    #[serde(default)]
    pub num_turns: u64,
    /// Exact USD cost ticks (`10^10` ticks = USD 1), when trustworthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_ticks: Option<i64>,
    /// The token counters may under-count work still in flight or not applied.
    #[serde(default)]
    pub usage_incomplete: bool,
    /// At least one folded model call omitted cost information.
    #[serde(default)]
    pub cost_partial: bool,
    /// Explicitly prevents a last-call sample from masquerading as a turn total.
    #[serde(default)]
    pub scope: UsageScope,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cached_read_tokens: 0,
            cached_write_tokens: 0,
            reasoning_tokens: 0,
            model_calls: 0,
            num_turns: 0,
            cost_usd_ticks: None,
            // `Usage::default()` is used by completion surfaces whose backend did
            // not report usage. Preserve that uncertainty instead of inventing an
            // exact free, zero-token turn.
            usage_incomplete: true,
            cost_partial: false,
            scope: UsageScope::WholePrompt,
        }
    }
}

impl Usage {
    /// Construct an exact whole-prompt token report without a cost claim.
    #[must_use]
    pub const fn exact(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens.saturating_add(output_tokens),
            cached_read_tokens: 0,
            cached_write_tokens: 0,
            reasoning_tokens: 0,
            model_calls: 0,
            num_turns: 0,
            cost_usd_ticks: None,
            usage_incomplete: false,
            cost_partial: false,
            scope: UsageScope::WholePrompt,
        }
    }

    /// Return the exact cost only when every quality flag permits trusting it.
    #[must_use]
    pub const fn trusted_cost_usd_ticks(self) -> Option<i64> {
        if self.usage_incomplete || self.cost_partial {
            return None;
        }
        match self.cost_usd_ticks {
            // Zero is an exact, meaningful value for a free/local model. Missing
            // cost is represented by `None`; conflating the two makes one free
            // call erase otherwise exact cost when whole-prompt reports merge.
            Some(ticks) if ticks >= 0 => Some(ticks),
            _ => None,
        }
    }

    /// Whether this value carries no known token lower bound.
    #[must_use]
    pub const fn has_empty_lower_bound(self) -> bool {
        self.total_tokens == 0
    }

    /// Fold two distinct whole-prompt reports without overstating quality.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        let usage_incomplete = self.usage_incomplete || other.usage_incomplete;
        let cost_partial = self.cost_partial || other.cost_partial;
        let cost_usd_ticks = if usage_incomplete || cost_partial {
            None
        } else {
            self.trusted_cost_usd_ticks()
                .zip(other.trusted_cost_usd_ticks())
                .and_then(|(left, right)| left.checked_add(right))
        };
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_add(other.cached_read_tokens),
            cached_write_tokens: self
                .cached_write_tokens
                .saturating_add(other.cached_write_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            model_calls: self.model_calls.saturating_add(other.model_calls),
            num_turns: self.num_turns.saturating_add(other.num_turns),
            cost_usd_ticks,
            usage_incomplete,
            cost_partial,
            scope: UsageScope::WholePrompt,
        }
    }

    /// Mark an otherwise known lower bound as incomplete and scrub its cost.
    #[must_use]
    pub const fn into_incomplete(mut self) -> Self {
        self.usage_incomplete = true;
        self.cost_usd_ticks = None;
        self
    }
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
/// - the five first-class subprocess drivers in `umadev-host` — drive a
///   logged-in host CLI base without owning a model credential or provider SDK.
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
    /// [`Self::from_tool_input`], also handy when the name was already matched upstream.
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

    /// Like [`prompt_block`](Self::prompt_block) but WITHOUT the numbered
    /// "reply with a number" framing: each question's options are listed as plain
    /// bullets. A text-question surface (the user set `question_form = "text"`)
    /// uses this so it can still show what the base is weighing while inviting a
    /// free-text answer instead of a numeric pick. Neutral structural text only
    /// (no localized words) so a localized framing can wrap it.
    #[must_use]
    pub fn prose_block(&self) -> String {
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
            if multi {
                let _ = write!(out, "Q{}. {title}", qi + 1);
            } else {
                out.push_str(&title);
            }
            for opt in &q.options {
                out.push('\n');
                if opt.description.is_empty() {
                    let _ = write!(out, "  - {}", opt.label);
                } else {
                    let _ = write!(out, "  - {} — {}", opt.label, opt.description);
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

/// The parsed input of a base's **`ExitPlanMode`** tool call — the plan the base
/// proposes and asks the user to approve before it LEAVES its OWN plan mode and
/// starts executing.
///
/// This is the **base CLI's** plan mode (claude-code's `ExitPlanMode`), which is
/// **distinct** from UmaDev's own `TrustMode::Guarded` / `TrustMode::Plan` tiers:
/// the base decided, inside its turn, to draft a plan and pause for approval.
/// UmaDev drives the base non-interactively, so — exactly like [`AskUserQuestion`]
/// — the base cannot pop up its own approval UI and the call auto-cancels
/// mid-turn. UmaDev only observes the tool-call event, so without this parser the
/// `plan` markdown is never shown (the tool row read as a bare "ExitPlanMode"
/// stub). This type lifts the `plan` text out of the raw tool input so a surface
/// layer can render it under a note that clearly labels it as the base's plan
/// mode — never conflated with UmaDev's own guarded banner.
///
/// **Fail-open by construction:** [`from_tool_input`](Self::from_tool_input)
/// returns `None` for a non-`ExitPlanMode` call or an input with no `plan` text —
/// never a panic, never a fabricated plan.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ExitPlanMode {
    /// The plan markdown the base wants approved before it starts executing.
    pub plan: String,
}

impl ExitPlanMode {
    /// Whether `name` is the base's exit-plan-mode tool. Case-insensitive and
    /// separator-insensitive so a normalized (`exit_plan_mode`) or canonical
    /// (`ExitPlanMode`) name both match — mirrors [`AskUserQuestion::is_tool_name`].
    #[must_use]
    pub fn is_tool_name(name: &str) -> bool {
        let n = name.replace(['_', '-', ' '], "").to_ascii_lowercase();
        n == "exitplanmode"
    }

    /// Parse a base `ExitPlanMode` tool-call input. Returns `None` when the call
    /// is not an `ExitPlanMode` or carries no readable `plan` text, so the caller
    /// keeps its existing (non-plan) tool-row rendering.
    #[must_use]
    pub fn from_tool_input(name: &str, input: &serde_json::Value) -> Option<Self> {
        if !Self::is_tool_name(name) {
            return None;
        }
        Self::parse_value(input)
    }

    /// Parse just the input value (no tool-name guard) — the shared body of
    /// [`Self::from_tool_input`], handy when the name was already matched upstream.
    #[must_use]
    pub fn parse_value(input: &serde_json::Value) -> Option<Self> {
        let plan = input
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if plan.is_empty() {
            return None;
        }
        Some(Self {
            plan: plan.to_string(),
        })
    }

    /// A compact, single-line summary for the tool-row `detail` (never multi-line):
    /// the first non-empty line of the plan, whitespace-collapsed and clipped, so
    /// the tool row shows what's being approved instead of a bare "ExitPlanMode".
    #[must_use]
    pub fn summary(&self) -> String {
        let first = self
            .plan
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        one_line_clip(first, 72)
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
/// `[tool] Reading src/app.tsx...` and `[write] Writing...` instead of a blank spinner.
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
    /// A tool invocation whose base protocol supplies a stable call id.
    ///
    /// Keeping the id through the presentation boundary is what lets a client
    /// attach interleaved progress, output, and terminal results to the exact
    /// row that owns them instead of guessing from FIFO order.
    ToolUseCorrelated {
        /// Stable, base-supplied tool-call id.
        call_id: String,
        /// Tool name (e.g. "Read", "Write", "Bash").
        name: String,
        /// Human-readable description (file path, command).
        detail: String,
        /// Structured edit exposed by the base, when available.
        edit: Option<ToolEdit>,
    },
    /// A non-terminal status-title replacement for one running tool card.
    ///
    /// This is neither stdout nor a completion verdict. Kimi Code uses it for
    /// `tool.progress` status updates such as "Reading dependencies".
    ToolProgressCorrelated {
        /// Stable id of the running tool call.
        call_id: String,
        /// Complete replacement for the tool's current status title.
        title: String,
    },
    /// An incremental output chunk from a tool that is **still running**.
    ///
    /// This is display-only progress: consumers must not treat it as completion,
    /// advance a tool-call FIFO, or use it as verification evidence. A later
    /// [`StreamEvent::ToolResult`] is the sole terminal verdict for the call.
    ToolOutputDelta {
        /// UTF-8 output emitted since the previous progress event.
        delta: String,
    },
    /// Incremental output for one identified, still-running tool call.
    ToolOutputDeltaCorrelated {
        /// Stable id of the tool call producing this output.
        call_id: String,
        /// UTF-8 output emitted since the previous progress event.
        delta: String,
    },
    /// A complete replacement snapshot for the currently running tool output.
    ///
    /// Some machine protocols periodically send the whole terminal buffer, or
    /// explicitly reset it, instead of an append-only byte stream. Consumers
    /// must replace the visible log with `output`; they must not append it.
    ToolOutputSnapshot {
        /// Complete safe UTF-8 terminal buffer; an empty string clears it.
        output: String,
    },
    /// A complete output-buffer replacement for one identified tool call.
    ToolOutputSnapshotCorrelated {
        /// Stable id of the tool call producing this snapshot.
        call_id: String,
        /// Complete safe UTF-8 terminal buffer; an empty string clears it.
        output: String,
    },
    /// The worker received the **terminal** result from a tool call.
    /// `ok` = success/failure; `summary` is a truncated result preview. Exactly
    /// this event, never [`StreamEvent::ToolOutputDelta`], settles the call.
    ToolResult {
        /// Whether the tool call succeeded.
        ok: bool,
        /// Truncated result preview (first ~200 chars).
        summary: String,
    },
    /// The terminal verdict for one identified tool call.
    ToolResultCorrelated {
        /// Stable id of the tool call this result settles.
        call_id: String,
        /// Whether the tool call succeeded.
        ok: bool,
        /// Truncated result preview.
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
         Pick a base with /claude, /codex, /opencode, /grok or /kimi (or run `umadev` again to \
         re-open the picker), and I'll pick up the conversation with full context. \
         離線:尚未连接底座 CLI / 離線:尚未連接底座 CLI。"
            .to_string()
    } else {
        // Echo the ask + the concrete next step. Bilingual tail mirrors the rest
        // of the offline surfaces (the catalogs gate full i18n; this is the
        // runtime-crate floor, which has no i18n dep on purpose).
        format!(
            "[offline] I heard: \u{201c}{echo}\u{201d} — but no base CLI is connected, \
             so I can't actually work on it yet. Connect a base with /claude, /codex, \
             /opencode, /grok or /kimi and ask again; I'll keep this conversation's context. \
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

/// A binary decision handed back to a base for an approval-style request.
///
/// This remains the payload of the legacy [`BaseSession::respond`] method and
/// is also embedded in the richer [`HostResponse`] contract. Keeping this type
/// unchanged preserves every existing approval caller while newer drivers can
/// retain a vendor option id, feedback, or a structured answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow the action.
    Allow,
    /// Deny the action (the base gets an error result and continues / stops).
    Deny,
}

/// The semantic meaning of one approval option offered by a base.
///
/// ACP bases commonly distinguish one-turn and persistent choices. The raw
/// option id remains authoritative; this classification lets a surface render
/// and policy-check it without comparing vendor labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostApprovalOptionKind {
    /// Permit this occurrence only.
    AllowOnce,
    /// Permit this class of action for subsequent occurrences too.
    AllowAlways,
    /// Reject this occurrence only.
    RejectOnce,
    /// Reject this class of action for subsequent occurrences too.
    RejectAlways,
    /// A vendor-specific option whose semantic kind is not standardised.
    Other(String),
}

/// One selectable option in a typed host approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostApprovalOption {
    /// Stable option id that must be echoed to protocols such as ACP.
    pub id: String,
    /// Human-readable label supplied by the base.
    pub label: String,
    /// Policy-relevant meaning of this option.
    pub kind: HostApprovalOptionKind,
}

/// The answer shape expected by a structured host question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostQuestionKind {
    /// Arbitrary visible text.
    Text,
    /// Sensitive text that a UI should mask while it is entered.
    Secret,
    /// Exactly one offered option.
    SingleChoice,
    /// Zero or more offered options.
    MultiChoice,
    /// A yes/no-style confirmation.
    Confirmation,
    /// A vendor-specific question type.
    Other(String),
}

/// One selectable value in a structured host question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostQuestionOption {
    /// Protocol value returned when this option is selected.
    pub value: String,
    /// Human-readable option label.
    pub label: String,
    /// Optional explanatory text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional content shown while this option is focused. Hosts such as
    /// Grok Build use this for code, mock-up, or diff previews; keeping it
    /// typed avoids burying interactive content in opaque metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// One typed question a host asks the user during an in-flight turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostQuestion {
    /// Stable question id used to correlate its answer.
    pub id: String,
    /// Optional compact heading for picker-style surfaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// Full prompt shown to the user.
    pub prompt: String,
    /// Expected answer shape.
    pub kind: HostQuestionKind,
    /// Whether the base requires a non-empty answer.
    #[serde(default)]
    pub required: bool,
    /// Offered choices; empty for free-text questions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<HostQuestionOption>,
}

/// One answer to a [`HostQuestion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAnswer {
    /// The question id being answered.
    pub question_id: String,
    /// Selected option values or one free-text value.
    #[serde(default)]
    pub values: Vec<String>,
}

/// Optional presentation and free-form context attached to one answer.
///
/// `question_id` is the host-local correlation key. Protocol drivers translate
/// it back to the vendor's required key (for Grok Build, the original question
/// text) and never expose it as an answer value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostQuestionAnnotation {
    /// Local question id associated with this annotation.
    pub question_id: String,
    /// Verbatim preview for the selected single-choice option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Optional free-form notes entered alongside the selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Rich outcome of a blocking questionnaire.
///
/// The ordinary [`HostResponse::UserInput`] shape remains available for the
/// other bases. This outcome preserves the four-path interview contract used
/// by Grok Build without coercing “chat about this” or “skip interview” into a
/// completed answer submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HostUserInputOutcome {
    /// Submit the current answers and optional annotations.
    Accepted {
        /// Answers correlated by local question id.
        answers: Vec<HostAnswer>,
        /// Preview/notes annotations correlated by local question id.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<HostQuestionAnnotation>,
    },
    /// Return partial answers so the agent can discuss/reformulate them.
    ChatAboutThis {
        /// Answered questions only.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        partial_answers: Vec<HostAnswer>,
    },
    /// End the interview and let the agent plan from the available answers.
    SkipInterview {
        /// Answered questions only.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        partial_answers: Vec<HostAnswer>,
    },
    /// Dismiss the questionnaire without treating it as an infrastructure error.
    Cancelled,
}

/// Three-way outcome of a base plan-approval interaction.
///
/// `Abandoned` is deliberately distinct from `Cancelled`: the former exits
/// plan mode without starting implementation, while the latter keeps planning
/// and may carry revision feedback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HostPlanOutcome {
    /// Approve the plan and begin implementation.
    Approved,
    /// Keep planning, optionally incorporating the user's feedback.
    Cancelled {
        /// Revision feedback supplied by the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    /// Leave plan mode without implementing the proposed plan.
    Abandoned,
}

/// One permission or scope a base wants to add for the current operation.
///
/// `kind` and `target` cover the portable policy surface; `metadata` preserves
/// non-secret vendor fields needed to formulate an exact protocol response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPermission {
    /// Permission class, for example `filesystem_write` or `network`.
    pub kind: String,
    /// Optional path, host, process, or other concrete scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Vendor-specific, non-secret permission details.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

/// The outcome defined by the MCP elicitation protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostElicitationAction {
    /// Accept the request and return structured content.
    Accept,
    /// Explicitly decline the request.
    Decline,
    /// Cancel without an affirmative or negative answer.
    Cancel,
}

/// Human-only outcome for a base's folder-trust gate.
///
/// This is intentionally not [`ApprovalDecision`]: a generic auto-approval or
/// remembered tool permission must never become authority to trust a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostFolderTrustDecision {
    /// The live user explicitly trusted the validated folder scope.
    Trust,
    /// Reject, cancel, time out, or otherwise leave the base gated.
    KeepGated,
}

/// A typed request from a live base session to its UmaDev host.
///
/// These variants deliberately separate permission, question, and elicitation
/// protocols. Treating every server request as binary approval loses user
/// answers and can accidentally authorise an unknown request. [`Unknown`](Self::Unknown)
/// is therefore first-class and must be rejected unless a newer, understood
/// contract handles it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostRequest {
    /// A tool/action approval request (legacy `NeedApproval` plus ACP options).
    Approval {
        /// Tool id or action class.
        action: String,
        /// File, command, resource, or other target.
        target: String,
        /// Optional explanation supplied by the base.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// Selectable protocol options. Empty means binary allow/deny.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<HostApprovalOption>,
        /// Vendor-specific, non-secret request fields.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    /// One or more structured questions that require actual user answers.
    UserInput {
        /// Questions in presentation order.
        questions: Vec<HostQuestion>,
        /// Vendor-specific, non-secret request fields.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    /// A request to expand the session's current permissions or sandbox scope.
    PermissionExpansion {
        /// Concrete permissions the base wants to add.
        permissions: Vec<HostPermission>,
        /// Optional reason shown to the user or policy layer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Vendor-specific, non-secret request fields.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    /// An MCP server asks the user for structured information.
    McpElicitation {
        /// MCP server name when the base reports it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        /// Prompt supplied by the MCP server.
        message: String,
        /// JSON Schema for the expected response content.
        requested_schema: serde_json::Value,
        /// Vendor-specific, non-secret request fields.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    /// The base drafted a plan and asks before beginning execution.
    PlanConfirmation {
        /// Full plan text or markdown.
        plan: String,
        /// Optional confirmation prompt supplied by the base.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// Vendor-specific, non-secret request fields.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        metadata: serde_json::Value,
    },
    /// Grok Build asks the live user whether source-computed configuration in a
    /// validated folder may be loaded.
    FolderTrust {
        /// Exact validated session cwd, for display only.
        cwd: PathBuf,
        /// Grok-computed git/worktree-aware trust key, for display only.
        workspace: PathBuf,
        /// Bounded configuration categories that caused the gate.
        config_kinds: Vec<String>,
    },
    /// A request method the current UmaDev version does not understand.
    Unknown {
        /// Vendor method name, retained for diagnostics and capability gating.
        method: String,
        /// Original request payload after secret redaction.
        payload: serde_json::Value,
    },
}

impl HostRequest {
    /// Build the safest protocol-shaped response for this request.
    ///
    /// Approval, permission, and plan requests are denied; MCP elicitation is
    /// declined; questions and unknown methods receive an explicit rejection.
    /// This is the host-interaction security floor, distinct from governance's
    /// fail-open content scanning: an unrecognised permission request must never
    /// become authority by accident.
    #[must_use]
    pub fn safe_rejection(&self, reason: impl Into<String>) -> HostResponse {
        let reason = reason.into();
        match self {
            Self::Approval { .. } => HostResponse::Approval {
                decision: ApprovalDecision::Deny,
                selected_option_id: None,
                message: Some(reason),
            },
            Self::PermissionExpansion { .. } => HostResponse::PermissionExpansion {
                decision: ApprovalDecision::Deny,
                granted: Vec::new(),
                message: Some(reason),
            },
            Self::McpElicitation { .. } => HostResponse::McpElicitation {
                action: HostElicitationAction::Decline,
                content: None,
            },
            Self::PlanConfirmation { metadata, .. }
                if metadata
                    .get("responseContract")
                    .and_then(serde_json::Value::as_str)
                    == Some("grok_exit_plan_mode_v1") =>
            {
                HostResponse::PlanOutcome {
                    outcome: HostPlanOutcome::Cancelled {
                        feedback: Some(reason),
                    },
                }
            }
            Self::PlanConfirmation { .. } => HostResponse::PlanConfirmation {
                decision: ApprovalDecision::Deny,
                feedback: Some(reason),
            },
            Self::FolderTrust { .. } => HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::KeepGated,
            },
            Self::UserInput { metadata, .. }
                if metadata
                    .get("responseContract")
                    .and_then(serde_json::Value::as_str)
                    == Some("grok_ask_user_question_v1") =>
            {
                HostResponse::UserInputOutcome {
                    outcome: HostUserInputOutcome::Cancelled,
                }
            }
            Self::UserInput { metadata, .. }
                if matches!(
                    metadata
                        .get("responseContract")
                        .and_then(serde_json::Value::as_str),
                    Some("kimi_plan_review_permission_v1" | "kimi_permission_question_v1")
                ) =>
            {
                // Both kimi ACP interactive contracts settle a refusal as a
                // protocol Cancelled (→ `write_host_response`'s Cancelled arm →
                // `cancelled_host_response_frame`), never the generic Rejected
                // that the kimi driver has no write-back arm for (→ -32602).
                HostResponse::Cancelled {
                    reason: Some(reason),
                }
            }
            Self::UserInput { .. } | Self::Unknown { .. } => HostResponse::Rejected { reason },
        }
    }
}

/// A typed reply to a [`HostRequest`].
///
/// Drivers must validate that the response variant matches the pending request
/// before writing it to the base. A mismatch or unknown request is rejected,
/// never coerced into an affirmative response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostResponse {
    /// Reply to [`HostRequest::Approval`].
    Approval {
        /// Binary policy decision.
        decision: ApprovalDecision,
        /// Exact vendor option id to echo, when one was selected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_option_id: Option<String>,
        /// Optional denial explanation or audit note.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Reply to [`HostRequest::UserInput`].
    UserInput {
        /// Answers correlated by question id.
        answers: Vec<HostAnswer>,
    },
    /// Rich four-path reply to [`HostRequest::UserInput`].
    UserInputOutcome {
        /// Accepted/chat/skip/cancel outcome without semantic coercion.
        outcome: HostUserInputOutcome,
    },
    /// Reply to [`HostRequest::PermissionExpansion`].
    PermissionExpansion {
        /// Whether any requested scope may be granted.
        decision: ApprovalDecision,
        /// Exact granted subset; empty when denied.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        granted: Vec<HostPermission>,
        /// Optional explanation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Reply to [`HostRequest::McpElicitation`].
    McpElicitation {
        /// Accept, decline, or cancel outcome.
        action: HostElicitationAction,
        /// Schema-conforming content when accepted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<serde_json::Value>,
    },
    /// Reply to [`HostRequest::PlanConfirmation`].
    PlanConfirmation {
        /// Whether execution may begin.
        decision: ApprovalDecision,
        /// Optional user feedback for a rejected or revised plan.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    /// Rich three-path reply to [`HostRequest::PlanConfirmation`].
    PlanOutcome {
        /// Approved/cancelled/abandoned outcome.
        outcome: HostPlanOutcome,
    },
    /// Reply to [`HostRequest::FolderTrust`]. Only an explicit live-user action
    /// may construct `Trust`; every fallback uses `KeepGated`.
    FolderTrust {
        /// Trust or remain gated.
        decision: HostFolderTrustDecision,
    },
    /// Explicit cancellation of an interaction.
    Cancelled {
        /// Optional cancellation reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Explicit rejection used for unknown or unsupported request types.
    Rejected {
        /// Human-readable, non-secret reason.
        reason: String,
    },
}

/// Canonical reasoning effort reported by a live session.
///
/// This is deliberately a closed wire-value set. A driver must ignore an
/// unknown future value instead of guessing that it is equivalent to one of
/// today's levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionReasoningEffort {
    /// Disable explicit reasoning effort.
    None,
    /// Minimal reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort. The canonical wire spelling is `xhigh`.
    Xhigh,
}

impl SessionReasoningEffort {
    /// Exact canonical wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

impl TryFrom<&str> for SessionReasoningEffort {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            _ => Err("unknown session reasoning effort"),
        }
    }
}

impl fmt::Display for SessionReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One selectable reasoning-effort entry advertised for a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReasoningEffortOption {
    /// Stable user-selectable option id.
    pub id: String,
    /// Canonical value sent to the base.
    pub value: SessionReasoningEffort,
    /// Human-readable label.
    pub label: String,
    /// Optional explanatory text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this is the model's advertised default.
    #[serde(default)]
    pub default: bool,
}

/// One model in a session's complete replacement catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModelInfo {
    /// Stable catalog key used by [`BaseSession::set_model`].
    pub model_id: String,
    /// Human-readable model name.
    pub name: String,
    /// Optional model description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Advertised total context window, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_context_tokens: Option<u64>,
    /// Vendor agent implementation selected by this model, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Whether the model accepts a reasoning-effort selection.
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// Current/default effort included in this catalog snapshot, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<SessionReasoningEffort>,
    /// Model-specific selectable effort menu.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<SessionReasoningEffortOption>,
}

/// Closed session interaction mode.
///
/// Unknown values are never coerced to `Default`; a future base mode remains
/// unsupported until its semantics are explicitly modeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// Normal interactive work.
    Default,
    /// Read-only planning work.
    Plan,
    /// Ask-first interaction mode.
    Ask,
}

impl SessionMode {
    /// Exact canonical wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::Ask => "ask",
        }
    }
}

impl TryFrom<&str> for SessionMode {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "default" => Ok(Self::Default),
            "plan" => Ok(Self::Plan),
            "ask" => Ok(Self::Ask),
            _ => Err("unknown session mode"),
        }
    }
}

impl fmt::Display for SessionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One slash command in a session's complete replacement catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCommandInfo {
    /// Command name without a leading slash.
    pub name: String,
    /// Human-readable command description.
    pub description: String,
    /// Optional argument/input hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
    /// Optional source-defined command scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Optional source path for a skill-backed command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Closed status vocabulary for one base-owned ACP plan entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlanEntryStatus {
    /// The base has not started this item.
    Pending,
    /// The base is actively working on this item.
    InProgress,
    /// The base completed this item.
    Completed,
}

/// Closed priority vocabulary from the stable ACP plan update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlanEntryPriority {
    /// Highest relative priority.
    High,
    /// Normal relative priority.
    Medium,
    /// Lowest relative priority.
    Low,
}

/// One item in a base-owned, whole-snapshot ACP plan update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlanEntry {
    /// User-facing task text supplied by the base.
    pub content: String,
    /// Relative priority supplied by the base.
    pub priority: SessionPlanEntryPriority,
    /// Current execution status supplied by the base.
    pub status: SessionPlanEntryStatus,
}

/// Typed dynamic state replacement or transition from a live base session.
///
/// Catalog variants are snapshots, not deltas: consumers must replace their
/// local list even when the new list is empty. Duplicate transition events are
/// idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionStateUpdate {
    /// Replace the entire model catalog and its advertised current model.
    ModelCatalogReplaced {
        /// Current model id for this snapshot.
        current_model_id: String,
        /// Complete replacement model list.
        available_models: Vec<SessionModelInfo>,
    },
    /// The active session model changed explicitly.
    ModelChanged {
        /// New model catalog id.
        model_id: String,
        /// Effective reasoning effort, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<SessionReasoningEffort>,
    },
    /// The base replaced an unavailable persisted model automatically.
    ModelAutoSwitched {
        /// Previously persisted model id.
        previous_model_id: String,
        /// Replacement model id; it may be empty when no fallback exists.
        new_model_id: String,
        /// Human-readable reason supplied by the base.
        reason: String,
    },
    /// The active interaction mode changed.
    ModeChanged {
        /// Exact closed mode.
        mode: SessionMode,
    },
    /// Replace the base's independent thinking configuration.
    ///
    /// This is deliberately not a [`SessionReasoningEffort`]: bases such as
    /// Kimi expose a distinct on/off axis whose enabled effort remains
    /// model-owned. `None` means the complete configuration snapshot omitted
    /// the control for the current model. The two capability bits preserve
    /// locked-on models instead of presenting a switch that cannot work.
    ThinkingChanged {
        /// Whether native thinking is enabled, or unavailable for this model.
        enabled: Option<bool>,
        /// Whether the current model advertises a selectable on state.
        can_enable: bool,
        /// Whether the current model advertises a selectable off state.
        can_disable: bool,
    },
    /// Replace the entire slash-command catalog and tool-name snapshot.
    CommandCatalogReplaced {
        /// Complete replacement command list.
        commands: Vec<SessionCommandInfo>,
        /// Complete tool-name snapshot attached to the command update.
        tools: Vec<String>,
    },
    /// Replace the base's entire native plan snapshot.
    ///
    /// This remains separate from UmaDev's director plan: a base may publish a
    /// short local todo list while executing one director-owned step.
    PlanReplaced {
        /// Complete replacement plan; an empty list explicitly clears it.
        entries: Vec<SessionPlanEntry>,
    },
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
    /// The base reported the EXACT model it resolved for this session, read from
    /// the base's own session metadata (for example claude's stream-json
    /// `system/init` `model` field or opencode's `session.updated.info.model`).
    /// Consumers may display this model id, but must not treat it as proof of a
    /// context-window size: that requires explicit base configuration/provider
    /// metadata. Consumers should treat duplicate reports as idempotent.
    /// **Fail-open:** a base whose init frame carries no model id simply never
    /// emits this; an unparseable frame is skipped, never a panic.
    SessionModel(String),
    /// A typed state snapshot or transition. This is state-only and should not
    /// create a transcript row or toast by itself.
    StateUpdate(SessionStateUpdate),
    /// The base invoked a tool — `name` is the tool id (`Write`/`Edit`/`Bash`/
    /// `Read`/…), `input` the raw tool input (e.g. `{"file_path": "..."}`).
    /// This is where a real file write shows up.
    ToolCall {
        /// Tool id (`Write`, `Edit`, `Bash`, `Read`, …).
        name: String,
        /// Raw tool input as the base reported it.
        input: serde_json::Value,
    },
    /// A tool invocation with the base's stable call id.
    ///
    /// New multi-tool protocols should emit this variant so interleaved updates
    /// and results never depend on FIFO ordering. Legacy drivers keep emitting
    /// [`SessionEvent::ToolCall`] unchanged when their stream exposes no id.
    ToolCallCorrelated {
        /// Stable, base-supplied tool-call id.
        call_id: String,
        /// Tool id (`Write`, `Edit`, `Bash`, `Read`, …).
        name: String,
        /// Raw tool input as the base reported it.
        input: serde_json::Value,
    },
    /// A non-terminal status-title replacement for one identified tool call.
    ///
    /// The title is presentation state, not tool output. Consumers must update
    /// the matching running card in place and must not settle the call.
    ToolProgressCorrelated {
        /// Stable id of the running tool call.
        call_id: String,
        /// Complete replacement for the tool's current status title.
        title: String,
    },
    /// An incremental output chunk from a tool that is **still running**.
    ///
    /// This event is non-terminal and display-only. Consumers must not pop a
    /// pending tool call, disarm an in-tool timeout, or count verification as
    /// passed until the matching terminal [`SessionEvent::ToolResult`] arrives.
    ToolOutputDelta(String),
    /// Complete replacement for an uncorrelated running tool's visible output.
    /// An empty string clears the buffer without settling the tool call.
    ToolOutputSnapshot(String),
    /// Incremental output for one identified, still-running tool call.
    ToolOutputDeltaCorrelated {
        /// Stable id of the tool call producing this output.
        call_id: String,
        /// UTF-8 output emitted since the previous progress event.
        delta: String,
    },
    /// A complete output-buffer replacement for one identified tool call.
    ToolOutputSnapshotCorrelated {
        /// Stable id of the tool call producing this snapshot.
        call_id: String,
        /// Complete safe UTF-8 terminal buffer; an empty string clears it.
        output: String,
    },
    /// A tool returned its **terminal** verdict. `ok` = success/failure,
    /// `summary` a truncated preview. This is the sole event that settles the
    /// pending tool call.
    ToolResult {
        /// Whether the tool call succeeded.
        ok: bool,
        /// Truncated result preview.
        summary: String,
    },
    /// The terminal verdict for one identified tool call.
    ToolResultCorrelated {
        /// Stable id of the tool call this result settles.
        call_id: String,
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
    /// A typed in-flight request that needs a host response.
    ///
    /// `req_id` is the stable protocol correlation id and must be passed back to
    /// [`BaseSession::respond_host`]. Unlike [`SessionEvent::NeedApproval`], the
    /// request preserves questions, permission expansion, MCP elicitation, plan
    /// confirmation, and unknown methods instead of flattening all of them into
    /// an unsafe binary approval.
    HostRequest {
        /// Stable, base-supplied request id.
        req_id: String,
        /// Typed request payload.
        request: HostRequest,
    },
    /// A previously surfaced [`SessionEvent::HostRequest`] was WITHDRAWN by the
    /// base before the host answered — the base cancelled its own request, the
    /// sub-agent that raised it finished, its bounded timeout elapsed, or the
    /// session/turn tore down. The surface must retract the now-stale picker for
    /// this `req_id` (a late answer to a withdrawn request is a benign no-op the
    /// driver's `respond_host` already tolerates). State-only: like
    /// [`SessionEvent::StateUpdate`], it creates no transcript row by itself and
    /// never counts as turn content.
    HostRequestSettled {
        /// The `req_id` of the withdrawn [`SessionEvent::HostRequest`].
        req_id: String,
        /// Short, non-secret reason for display (e.g. "base cancelled",
        /// "sub-agent finished", "timed out").
        reason: String,
    },
    /// A lifecycle signal for one of the base's OWN background **sub-agents**.
    /// Claude emits these as stream-json `system` frames: `task_started` (edge),
    /// `task_notification` (terminal edge: completed / failed / stopped) and
    /// `background_tasks_changed` (level: the full live set). The orchestrator
    /// also normalizes OpenCode child-session SSE/status reconciliation into the
    /// same edges/level. Codex maps app-server `collabAgentToolCall` items and
    /// their `agentsStates` onto the same level signal while filtering child
    /// `threadId` output from the main transcript.
    /// The orchestrator uses these signals to refuse to settle a turn as "done"
    /// while the base's own
    /// background agents are still outstanding (the premature-final-report
    /// fix) — background SHELLS (a dev server) are deliberately NOT surfaced,
    /// so a long-running server can never wedge a settle. **Fail-open:** a
    /// base that emits no such frames simply never produces this event; an
    /// unparseable frame is skipped, never a panic.
    BackgroundTask(BackgroundTaskSignal),
    /// Lifecycle state for an ordinary background shell process or monitor.
    ///
    /// This is deliberately separate from [`SessionEvent::BackgroundTask`]: a
    /// dev server may remain alive after its creating turn completes, and must
    /// never make the orchestrator wait for a sub-agent or redrive that turn.
    BackgroundProcess(BackgroundProcessSignal),
    /// Complete server-authoritative replacement of the live prompt queue.
    ///
    /// This event is state-only: consumers replace their queue mirror without
    /// creating a transcript row. A mutation request never edits the mirror
    /// optimistically; only this event may do so.
    PromptQueueChanged(PromptQueueSnapshot),
    /// The current turn ended — see [`TurnStatus`]. After this the orchestrator
    /// either sends the next phase's directive (same session, context retained)
    /// or stops at a gate.
    TurnDone {
        /// How the turn ended.
        status: TurnStatus,
        /// REAL token usage reported by the base for this turn, when the base's
        /// live protocol carries it (Claude's stream-json `result` line, Codex's
        /// `turn/completed` / `thread/tokenUsage/updated` notification, or
        /// OpenCode's assistant `message.updated` token totals). `None` when the
        /// selected base or installed version does not report exact per-turn usage;
        /// the consumer then falls back to a deterministic `chars/4` estimate so
        /// `/usage` stays non-empty but honest. **Fail-open:** an unparseable usage
        /// payload yields `None`, never a wrong number and never a panic.
        usage: Option<Usage>,
    },
}

/// Bounded tracker for every tool invocation that is still in flight.
///
/// Modern ACP/app-server streams can interleave several calls, so a single
/// `in_tool_call` boolean is incorrect: the first completed result would disarm
/// the long-tool watchdog while sibling calls are still running. Correlated
/// calls are tracked by their stable id; legacy id-less calls use a conservative
/// counter. The set is bounded against an adversarial base, and any overflow
/// remains active until a terminal turn/reset rather than falsely declaring the
/// tools finished.
#[derive(Debug, Default, Clone)]
pub struct ToolActivity {
    legacy_open: usize,
    correlated_open: HashSet<String>,
    overflowed: bool,
}

impl ToolActivity {
    const MAX_CORRELATED: usize = 1_024;

    /// Fold one session event into the tracker and return whether any tool is
    /// still active afterwards. Duplicate correlated starts are idempotent;
    /// unmatched results are ignored.
    pub fn observe(&mut self, event: &SessionEvent) -> bool {
        match event {
            SessionEvent::ToolCall { .. } => {
                self.legacy_open = self.legacy_open.saturating_add(1);
            }
            SessionEvent::ToolCallCorrelated { call_id, .. } => {
                if self.correlated_open.len() < Self::MAX_CORRELATED {
                    self.correlated_open.insert(call_id.clone());
                } else if !self.correlated_open.contains(call_id) {
                    self.overflowed = true;
                }
            }
            SessionEvent::ToolResult { .. } => {
                self.legacy_open = self.legacy_open.saturating_sub(1);
            }
            SessionEvent::ToolResultCorrelated { call_id, .. } => {
                self.correlated_open.remove(call_id);
            }
            SessionEvent::TurnDone { .. } => self.clear(),
            _ => {}
        }
        self.is_active()
    }

    /// Whether at least one observed call has not produced its matching terminal
    /// result yet.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.overflowed || self.legacy_open > 0 || !self.correlated_open.is_empty()
    }

    /// Forget all calls at a known turn/retry boundary.
    pub fn clear(&mut self) {
        self.legacy_open = 0;
        self.correlated_open.clear();
        self.overflowed = false;
    }
}

impl SessionEvent {
    /// Return the stable tool-call id carried by a correlated tool event.
    ///
    /// Legacy FIFO-only tool events return `None`; callers must retain their
    /// existing ordering fallback for those variants.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolCallCorrelated { call_id, .. }
            | Self::ToolProgressCorrelated { call_id, .. }
            | Self::ToolOutputDeltaCorrelated { call_id, .. }
            | Self::ToolOutputSnapshotCorrelated { call_id, .. }
            | Self::ToolResultCorrelated { call_id, .. } => Some(call_id),
            _ => None,
        }
    }
}

/// Permissions granted to a long-lived host session.
///
/// Filesystem/network/process access and approval automation are separate axes:
/// Guarded can run a normal development environment while still surfacing
/// consequential actions for review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BasePermissionProfile {
    /// Request planning/research behavior and the base's strongest available
    /// read-only boundary. Effective enforcement remains vendor- and
    /// platform-specific and requires separate attestation.
    Plan,
    /// Request full development access while keeping host approval events active.
    #[default]
    Guarded,
    /// Request full development access with ordinary host approvals pre-authorized.
    Auto,
}

impl BasePermissionProfile {
    /// Whether UmaDev requests unrestricted filesystem, process, network, and
    /// local-port access from the host.
    ///
    /// This is an **intent**, not evidence that the base actually obtained those
    /// capabilities. Enterprise policy, an inherited parent sandbox, or a
    /// platform-specific vendor downgrade may still restrict the live process.
    #[must_use]
    pub const fn full_access(self) -> bool {
        !matches!(self, Self::Plan)
    }

    /// Whether UmaDev requests full development access from the base.
    ///
    /// Prefer this name in new code: unlike the compatibility
    /// [`full_access`](Self::full_access) accessor, it cannot be mistaken for an
    /// attestation of the effective process boundary.
    #[must_use]
    pub const fn requests_full_access(self) -> bool {
        self.full_access()
    }

    /// Whether ordinary host approval requests should be pre-authorized.
    #[must_use]
    pub const fn auto_approve(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Sandbox profile UmaDev asked a base to use at process launch.
///
/// A requested profile is never itself proof of enforcement. Use
/// [`EffectiveSandboxEvidence`] before making a user-facing safety or capability
/// claim.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseSandboxRequest {
    /// Explicitly request no vendor sandbox.
    Off,
    /// Request a filesystem read-only sandbox.
    ReadOnly,
    /// Request writes inside the selected workspace only.
    WorkspaceWrite,
    /// Request the vendor's unrestricted/danger-full-access profile.
    DangerFullAccess,
    /// Vendor-specific profile not covered by the portable variants.
    Vendor(String),
}

impl BaseSandboxRequest {
    /// Stable launch request currently used for one first-class base/profile.
    ///
    /// This records UmaDev's intent only. It deliberately does not model the
    /// request as effective state; use [`EffectiveSandboxEvidence`] for claims.
    #[must_use]
    pub fn for_base_launch(backend: &str, permissions: BasePermissionProfile) -> Self {
        match (backend, permissions) {
            ("grok-build", BasePermissionProfile::Plan) => Self::ReadOnly,
            ("grok-build", BasePermissionProfile::Guarded | BasePermissionProfile::Auto) => {
                Self::Off
            }
            (_, BasePermissionProfile::Plan) => Self::Vendor("umadev-plan".to_string()),
            (_, BasePermissionProfile::Guarded) => Self::Vendor("umadev-guarded".to_string()),
            (_, BasePermissionProfile::Auto) => Self::Vendor("umadev-auto".to_string()),
        }
    }
}

/// Whether the vendor's requested sandbox was actually applied.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEffectiveStatus {
    /// No trustworthy effective-state report was received.
    #[default]
    Unknown,
    /// The reported sandbox profile is actively enforced.
    Enforced,
    /// A trustworthy report says sandboxing is disabled.
    Disabled,
    /// The requested boundary could not be fully applied or was overridden.
    Degraded,
}

/// Provenance of an effective sandbox report.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEvidenceSource {
    /// No evidence beyond UmaDev's own launch request.
    #[default]
    None,
    /// UmaDev knows only the argv/config value it requested.
    LaunchRequestOnly,
    /// The base reported its post-policy, post-platform effective state over its
    /// machine protocol.
    VendorProtocol,
    /// The vendor's native resume preflight verified the persisted profile before
    /// the irreversible process sandbox was applied. This proves only profile
    /// consistency; it is not evidence that the OS sandbox was applied.
    NativeResumePreflight,
}

impl SandboxEvidenceSource {
    /// Whether this source attests the post-policy, post-application effective
    /// process boundary. A native resume preflight is intentionally excluded: it
    /// compares configurations but cannot prove the later OS operation succeeded.
    #[must_use]
    pub const fn attests_effective_state(self) -> bool {
        matches!(self, Self::VendorProtocol)
    }
}

/// Effective filesystem write boundary reported for a live base process.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveFilesystemAccess {
    /// The live boundary is not known.
    #[default]
    Unknown,
    /// Project files are read-only.
    ReadOnly,
    /// Writes are limited to the selected workspace (plus vendor runtime paths).
    WorkspaceWrite,
    /// No vendor filesystem restriction was reported.
    Unrestricted,
}

/// Effective network or local-listener capability reported for a live process.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveIoAccess {
    /// The live capability is not known.
    #[default]
    Unknown,
    /// The capability is available to spawned development commands.
    Allowed,
    /// The capability is blocked for spawned development commands.
    Blocked,
}

/// Post-policy, post-platform evidence for a base process's real sandbox.
///
/// `status=unknown` with `source=launch_request_only` is the normal value when a
/// base accepts a flag but does not expose any machine-readable attestation. It
/// must never be projected as either "hard read-only" or "full access".
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
pub struct EffectiveSandboxEvidence {
    /// Whether the requested sandbox was applied.
    pub status: SandboxEffectiveStatus,
    /// Resolved profile after vendor configuration/enterprise policy, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_profile: Option<BaseSandboxRequest>,
    /// Trustworthy origin of this report.
    pub source: SandboxEvidenceSource,
    /// Effective filesystem write boundary.
    pub filesystem: EffectiveFilesystemAccess,
    /// Effective outbound network capability for spawned commands.
    pub network: EffectiveIoAccess,
    /// Effective bind/listen capability for local development servers.
    pub local_ports: EffectiveIoAccess,
    /// Bounded, non-secret diagnostic supplied by the reporting boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl EffectiveSandboxEvidence {
    /// Record that only a launch request is known; no effective claim is safe.
    #[must_use]
    pub fn requested_only(requested: BaseSandboxRequest) -> Self {
        Self {
            status: SandboxEffectiveStatus::Unknown,
            resolved_profile: Some(requested),
            source: SandboxEvidenceSource::LaunchRequestOnly,
            filesystem: EffectiveFilesystemAccess::Unknown,
            network: EffectiveIoAccess::Unknown,
            local_ports: EffectiveIoAccess::Unknown,
            detail: Some("vendor effective sandbox state was not reported".to_string()),
        }
    }

    /// Whether the evidence proves an unrestricted development process.
    #[must_use]
    pub fn proves_full_access(&self) -> bool {
        self.status == SandboxEffectiveStatus::Disabled
            && self.source.attests_effective_state()
            && self.filesystem == EffectiveFilesystemAccess::Unrestricted
            && self.network == EffectiveIoAccess::Allowed
            && self.local_ports == EffectiveIoAccess::Allowed
    }

    /// Whether the evidence proves a hard read-only project filesystem.
    #[must_use]
    pub fn proves_read_only_filesystem(&self) -> bool {
        self.status == SandboxEffectiveStatus::Enforced
            && self.source.attests_effective_state()
            && self.filesystem == EffectiveFilesystemAccess::ReadOnly
    }

    /// Whether this is trustworthy evidence for the exact requested profile.
    #[must_use]
    pub fn verifies_profile(&self, requested: &BaseSandboxRequest) -> bool {
        self.source.attests_effective_state()
            && matches!(
                self.status,
                SandboxEffectiveStatus::Enforced | SandboxEffectiveStatus::Disabled
            )
            && self.resolved_profile.as_ref() == Some(requested)
    }
}

/// Immutable authority identity attached to a resumable vendor session id.
///
/// A vendor id may be loaded only under the same base, canonical workspace,
/// permission profile, and sandbox request. Grok Build additionally requires a
/// trustworthy effective sandbox report and its native pre-start resume
/// preflight; ACP `session/load` alone runs too late to enforce that invariant.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct BaseResumeIdentity {
    /// Stable UmaDev backend id that minted the vendor session.
    pub backend: String,
    /// Canonical absolute workspace used by the session.
    pub canonical_workspace: PathBuf,
    /// Immutable UmaDev permission profile used at launch.
    pub permission_profile: BasePermissionProfile,
    /// Sandbox profile requested at launch.
    pub requested_sandbox: BaseSandboxRequest,
    /// Effective sandbox report captured for that process.
    pub effective_sandbox: EffectiveSandboxEvidence,
    /// Whether the vendor must run its native pre-start resume/profile check before
    /// any protocol-level load is permitted.
    #[serde(default)]
    pub native_resume_preflight_required: bool,
}

impl BaseResumeIdentity {
    /// Resolve a requested-only launch identity for a real workspace.
    ///
    /// Canonicalization is fail-closed: if the root cannot be resolved, no
    /// authority-bearing resume identity is minted.
    #[must_use]
    pub fn requested_for_launch(
        backend: &str,
        workspace: &Path,
        permission_profile: BasePermissionProfile,
    ) -> Option<Self> {
        let canonical_workspace = std::fs::canonicalize(workspace).ok()?;
        Some(Self::requested_only(
            backend,
            canonical_workspace,
            permission_profile,
            BaseSandboxRequest::for_base_launch(backend, permission_profile),
            backend == "grok-build",
        ))
    }

    /// Construct a launch identity when UmaDev has no effective vendor report.
    #[must_use]
    pub fn requested_only(
        backend: impl Into<String>,
        canonical_workspace: PathBuf,
        permission_profile: BasePermissionProfile,
        requested_sandbox: BaseSandboxRequest,
        native_resume_preflight_required: bool,
    ) -> Self {
        let effective_sandbox = EffectiveSandboxEvidence::requested_only(requested_sandbox.clone());
        Self {
            backend: backend.into(),
            canonical_workspace,
            permission_profile,
            requested_sandbox,
            effective_sandbox,
            native_resume_preflight_required,
        }
    }

    /// Whether a persisted vendor id may be loaded for this requested launch.
    ///
    /// `native_resume_preflight_satisfied` must come from the launch path, never a
    /// persisted project file. Grok identities fail closed until both the saved
    /// effective state is attested and the native preflight is wired.
    #[must_use]
    pub fn permits_resume_as(
        &self,
        requested: &Self,
        native_resume_preflight_satisfied: bool,
    ) -> bool {
        let immutable_match = self.backend == requested.backend
            && self.canonical_workspace == requested.canonical_workspace
            && self.permission_profile == requested.permission_profile
            && self.requested_sandbox == requested.requested_sandbox;
        if !immutable_match {
            return false;
        }
        if self.backend != "grok-build" {
            return true;
        }
        self.native_resume_preflight_required
            && requested.native_resume_preflight_required
            && native_resume_preflight_satisfied
            && self
                .effective_sandbox
                .verifies_profile(&self.requested_sandbox)
    }
}

/// One background sub-agent lifecycle signal (see
/// [`SessionEvent::BackgroundTask`]). The driver translates the base's own
/// frames and pre-filters to SUB-AGENT tasks only (a background shell — e.g. a
/// dev server the base deliberately leaves running — must never be waited on),
/// so consumers stay base-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundTaskSignal {
    /// A background sub-agent started (claude `system/task_started` with an
    /// agent-typed task). `id` is the base's task id.
    Started {
        /// The base's task id for this background sub-agent.
        id: String,
    },
    /// A background task reached a terminal state (claude
    /// `system/task_notification` with status completed / failed / stopped).
    /// Emitted for ANY task id — removing a non-agent id from an agents-only
    /// set is a harmless no-op, and this keeps the terminal edge lossless.
    Finished {
        /// The base's task id that finished.
        id: String,
    },
    /// The LEVEL signal (claude `system/background_tasks_changed`): the full
    /// set of currently-live background SUB-AGENT ids. Consumers must REPLACE
    /// their outstanding set with this payload rather than pairing edges, so a
    /// missed edge can never wedge a stale count (the base's own documented
    /// contract for this frame).
    Live {
        /// All currently-live background sub-agent task ids.
        agent_ids: Vec<String>,
    },
}

/// Kind of ordinary background process reported by a base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundProcessKind {
    /// A shell command running independently of the current turn.
    Bash,
    /// A long-lived monitor process.
    Monitor,
}

/// Safe, bounded identity for one live background process.
///
/// Commands, working directories, output paths, and captured output are not
/// carried here. They may contain secrets or host paths and are unnecessary
/// for lifecycle reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundProcessInfo {
    /// Base-assigned background task id.
    pub task_id: String,
    /// Tool call that transitioned into background execution.
    pub tool_call_id: String,
    /// Whether the process is an ordinary shell or monitor.
    pub kind: BackgroundProcessKind,
    /// Optional redacted human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Ordinary background process lifecycle.
///
/// Consumers may display this state or replace a task pane from [`Self::Live`],
/// but must not feed it into sub-agent completion waits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BackgroundProcessSignal {
    /// A process transitioned to background execution.
    Started {
        /// Safe process identity and display metadata.
        process: BackgroundProcessInfo,
    },
    /// A process reached a terminal state.
    Finished {
        /// Base-assigned task id.
        task_id: String,
        /// Process kind from the terminal snapshot.
        kind: BackgroundProcessKind,
        /// Exit status when the base observed one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Redacted termination signal, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
        /// Whether the base truncated captured output.
        truncated: bool,
        /// Whether the base queued its own model wake-up after completion.
        will_wake: bool,
    },
    /// Complete replacement set reconstructed during session replay.
    Live {
        /// All ordinary background processes still live after replay.
        processes: Vec<BackgroundProcessInfo>,
    },
}

/// One task in a base-owned, server-authoritative background-process snapshot.
///
/// The snapshot deliberately excludes commands, captured output, working
/// directories, and output-file paths. Those fields are present in some vendor
/// protocols but may contain secrets and are unnecessary for list/stop control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundProcessSnapshotEntry {
    /// Base-assigned task id used by the native stop operation.
    pub task_id: String,
    /// Whether the task is an ordinary shell or a monitor.
    pub kind: BackgroundProcessKind,
    /// Whether the base reports that the task has reached a terminal state.
    pub completed: bool,
    /// Exit status when the base has observed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Redacted termination signal, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// Whether the base truncated the task's captured output.
    pub truncated: bool,
}

/// Complete background-process replacement returned by the owning base.
///
/// `session_id` is the exact live vendor session used for the native query.
/// Drivers must exclude foreign-session tasks before constructing this value;
/// callers can therefore render or stop only tasks belonging to their session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundProcessSnapshot {
    /// Exact live vendor session that owns every returned task.
    pub session_id: String,
    /// Complete bounded set of tasks currently retained by the base, including
    /// terminal tombstones when the base returns them.
    pub processes: Vec<BackgroundProcessSnapshotEntry>,
}

/// Native result of asking the base to stop one background process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundProcessStopOutcome {
    /// The base stopped a process that was live when it handled the request.
    Killed,
    /// The process had already reached a terminal state.
    AlreadyExited,
    /// No task owned by the live session was available under that id.
    ///
    /// Drivers intentionally use the same result for a missing id and a foreign
    /// session's id so the API cannot become a cross-session discovery oracle.
    NotFound,
}

/// Native list/stop semantics exposed by a live base session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundProcessControlCapability {
    /// No native background-process control surface is available.
    #[default]
    Unsupported,
    /// Each list is a complete server snapshot and every stop is preceded by an
    /// exact live-session ownership check against a fresh snapshot.
    ServerAuthoritativeOwned,
}

/// One row in a base-owned prompt queue.
///
/// The queue is a server-authoritative collaboration surface. Callers must
/// replace their entire visible mirror from each [`PromptQueueSnapshot`] and
/// must not treat a locally requested mutation as committed until a later
/// snapshot confirms it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptQueueEntry {
    /// Stable base-assigned prompt id.
    pub id: String,
    /// Monotonic edit version used by version-checked mutations.
    pub version: u64,
    /// Client that originally enqueued the prompt, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Client that most recently edited the prompt, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_editor: Option<String>,
    /// Base-defined display kind such as `prompt`, `bash`, or `command`.
    pub kind: String,
    /// Plain queue text supplied by the base.
    pub text: String,
    /// Zero-based position in the complete queued set.
    pub position: usize,
}

/// Complete server-authoritative prompt-queue replacement for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptQueueSnapshot {
    /// Exact live session id that owns the queue.
    pub session_id: String,
    /// Complete ordered set of prompts that have not begun running.
    pub entries: Vec<PromptQueueEntry>,
    /// Prompt currently draining, when the base reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_prompt_id: Option<String>,
}

/// A version-aware mutation of a base-owned prompt queue.
///
/// Mutation delivery is fire-and-forget. Success means only that the complete
/// protocol frame was written; the next [`PromptQueueSnapshot`] remains the
/// sole authority for whether the mutation applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PromptQueueMutation {
    /// Remove one queued prompt if its version still matches.
    Remove {
        /// Stable prompt id.
        id: String,
        /// Last server version the user acted on.
        expected_version: u64,
    },
    /// Replace the ordering with the complete visible id set.
    Reorder {
        /// Complete ordered prompt-id list.
        ordered_ids: Vec<String>,
    },
    /// Clear prompts owned by this client.
    Clear,
    /// Edit one queued prompt in place.
    Edit {
        /// Stable prompt id.
        id: String,
        /// Non-empty replacement text.
        new_text: String,
    },
    /// Atomically promote one queued prompt, optionally replacing its text.
    Interject {
        /// Stable prompt id.
        id: String,
        /// Last server version the user acted on.
        expected_version: u64,
        /// Optional non-empty replacement text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_text: Option<String>,
    },
}

/// How a newly submitted prompt should enter a live base-owned queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptQueuePlacement {
    /// Append behind prompts already waiting.
    Tail,
    /// Ask the base to promote this prompt according to its native send-now
    /// semantics. This can cancel the current turn; it is not a local reorder.
    SendNow,
}

/// Native prompt-queue semantics exposed by a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptQueueCapability {
    /// No native queue surface is available.
    #[default]
    Unsupported,
    /// Complete snapshots are server-authoritative and stale-sensitive
    /// mutations carry the last observed entry version.
    ServerAuthoritativeVersioned,
}

/// One optional behavior a long-lived [`BaseSession`] may implement.
///
/// Capabilities are intentionally typed rather than inferred from the selected
/// backend id. A caller can therefore choose an explicit fallback without
/// sending a vendor method to a session that never advertised it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCapability {
    /// Submit through a native live steering operation instead of having the
    /// caller open an unrelated turn.
    MidTurnSteer,
    /// Switch the current model through the base's native session protocol.
    SetModel,
    /// Switch the current interaction mode through the base's native protocol.
    SetMode,
    /// Switch the current model-owned thinking toggle through the base's native
    /// session protocol.
    SetThinking,
    /// Submit and mutate prompts through a server-authoritative queue.
    PromptQueue,
    /// List and stop base-owned background processes through native methods.
    BackgroundProcessControl,
}

/// Stable kind tag for one ordered [`TurnInputBlock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputBlockKind {
    /// UTF-8 text supplied directly by the user.
    Text,
    /// A local image attachment.
    Image,
    /// A local generic-file attachment.
    File,
}

impl fmt::Display for TurnInputBlockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text => f.write_str("text"),
            Self::Image => f.write_str("image"),
            Self::File => f.write_str("file"),
        }
    }
}

/// Caller policy for a generic file when a base has no native file part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileInputMode {
    /// Require a native file/resource part. No text fallback is permitted.
    #[default]
    NativeOnly,
    /// Permit the driver to read a bounded UTF-8 file and materialize it as an
    /// explicitly labelled text block.
    MaterializeText,
}

/// One block in a user turn. Vector order is wire order.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnInputBlock {
    /// Plain UTF-8 text.
    Text {
        /// Text body. Empty text is retained so ordering is never rewritten.
        text: String,
    },
    /// A local image. The driver validates and reads it immediately before the
    /// protocol write; callers never provide MIME or base64 claims.
    Image {
        /// Local filesystem path.
        path: PathBuf,
    },
    /// A local generic file.
    File {
        /// Local filesystem path.
        path: PathBuf,
        /// Whether an explicit bounded-text fallback is acceptable.
        mode: FileInputMode,
    },
}

impl fmt::Debug for TurnInputBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { text } => f
                .debug_struct("Text")
                .field("text", &format_args!("[redacted; {} bytes]", text.len()))
                .finish(),
            Self::Image { .. } => f
                .debug_struct("Image")
                .field("path", &"[local path redacted]")
                .finish(),
            Self::File { mode, .. } => f
                .debug_struct("File")
                .field("path", &"[local path redacted]")
                .field("mode", mode)
                .finish(),
        }
    }
}

impl TurnInputBlock {
    /// Stable kind tag without exposing an attachment path.
    #[must_use]
    pub const fn kind(&self) -> TurnInputBlockKind {
        match self {
            Self::Text { .. } => TurnInputBlockKind::Text,
            Self::Image { .. } => TurnInputBlockKind::Image,
            Self::File { .. } => TurnInputBlockKind::File,
        }
    }
}

/// Ordered input for one base turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TurnInput {
    /// Input blocks in exact user order.
    pub blocks: Vec<TurnInputBlock>,
}

impl TurnInput {
    /// Construct one plain-text turn.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![TurnInputBlock::Text { text: text.into() }],
        }
    }

    /// Construct an input from an already ordered block vector.
    #[must_use]
    pub const fn new(blocks: Vec<TurnInputBlock>) -> Self {
        Self { blocks }
    }

    /// Return the sole text body when this is exactly one text block.
    #[must_use]
    pub fn sole_text(&self) -> Option<&str> {
        match self.blocks.as_slice() {
            [TurnInputBlock::Text { text }] => Some(text),
            _ => None,
        }
    }
}

impl From<String> for TurnInput {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

impl From<&str> for TurnInput {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

/// How one input kind is delivered by a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    /// The live protocol has no safe implementation.
    #[default]
    Unsupported,
    /// The protocol carries this as a native structured part.
    Native,
    /// Only explicit bounded UTF-8 materialization is available.
    MaterializedText,
}

/// Live steering semantics of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SteerSemantics {
    /// The protocol has no distinct live steering operation.
    #[default]
    Unsupported,
    /// Input is appended to the currently active turn.
    SameTurn,
    /// Input is queued for the active turn's next model safe point, or for the
    /// head of the immediately following turn when that safe point was missed.
    ///
    /// A successful steering method proves only that the input was queued by
    /// the protocol. It does not prove that the model has observed the input.
    SameTurnOrImmediateNext,
}

/// Cross-process conversation recovery exposed by a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResumeCapability {
    /// No resumable conversation surface is available.
    #[default]
    Unsupported,
    /// Vendor-native resume outside ACP.
    Native,
    /// ACP `session/resume` was negotiated.
    AcpResume,
    /// ACP `session/load` was negotiated as the compatibility fallback.
    AcpLoad,
}

/// Visibility of native sub-agent work in the event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubagentVisibility {
    /// No native lifecycle signal is exposed.
    #[default]
    None,
    /// Start/finish events are visible but no authoritative level is promised.
    Lifecycle,
    /// The driver emits an authoritative live-set level signal.
    AuthoritativeLiveSet,
}

/// Delivery result for one ordered input block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDeliveryReport {
    /// Zero-based position in [`TurnInput::blocks`].
    pub index: usize,
    /// Stable block kind; attachment paths are deliberately absent.
    pub kind: TurnInputBlockKind,
    /// Actual delivery mode used for this block.
    pub delivery: InputDelivery,
    /// Validated raw bytes represented by this block.
    pub source_bytes: usize,
    /// Validated MIME type when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// Strongest receipt stage proven for one outbound input frame.
///
/// A receipt is deliberately narrower than model progress: even a
/// [`ProtocolAcknowledged`](Self::ProtocolAcknowledged) frame proves only that
/// the base protocol echoed/accepted the correlated input. It never means the
/// model started, processed, or completed the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryReceiptStage {
    /// UmaDev wrote and flushed one complete frame to the base transport.
    #[default]
    TransportWritten,
    /// The base emitted its documented, exactly correlated protocol ACK.
    ProtocolAcknowledged,
}

/// Receipt returned after a complete protocol frame was accepted for writing,
/// optionally upgraded when the base exposes an exact correlated ACK. It
/// contains no local attachment paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeliveryReport {
    /// Per-block results in exact input order.
    pub blocks: Vec<BlockDeliveryReport>,
    /// Encoded protocol-frame size checked before writing. `None` is reserved
    /// for legacy text-only implementations that cannot observe their framing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoded_bytes: Option<usize>,
    /// Strongest proven delivery boundary. This is not model-processing state.
    #[serde(default)]
    pub receipt: DeliveryReceiptStage,
}

impl fmt::Display for SessionCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MidTurnSteer => f.write_str("mid-turn steer"),
            Self::SetModel => f.write_str("set model"),
            Self::SetMode => f.write_str("set mode"),
            Self::SetThinking => f.write_str("set thinking"),
            Self::PromptQueue => f.write_str("server-authoritative prompt queue"),
            Self::BackgroundProcessControl => {
                f.write_str("server-authoritative background-process control")
            }
        }
    }
}

/// Optional behaviors exposed by a live [`BaseSession`].
///
/// The conservative default advertises only the required text turn. Drivers
/// opt into richer protocol operations explicitly; opening another turn is
/// never treated as steering.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCapabilities {
    /// The session implements [`BaseSession::steer`] as a native live operation.
    pub mid_turn_steer: bool,
    /// The session implements [`BaseSession::set_model`].
    pub set_model: bool,
    /// The session implements [`BaseSession::set_mode`].
    pub set_mode: bool,
    /// The session implements [`BaseSession::set_thinking`].
    pub set_thinking: bool,
    /// Writes are checked by a host-native pre-write governance hook.
    pub realtime_governance: bool,
    /// Text delivery mode.
    pub text_input: InputDelivery,
    /// Image delivery mode.
    pub image_input: InputDelivery,
    /// Generic-file delivery mode.
    pub file_input: InputDelivery,
    /// Exact live steering semantics.
    pub steer: SteerSemantics,
    /// Cross-process resume semantics.
    pub resume: ResumeCapability,
    /// Native sub-agent event visibility.
    pub subagents: SubagentVisibility,
    /// Native prompt-queue semantics.
    pub prompt_queue: PromptQueueCapability,
    /// Native background-process list/stop semantics.
    pub background_process_control: BackgroundProcessControlCapability,
}

impl Default for SessionCapabilities {
    fn default() -> Self {
        Self {
            mid_turn_steer: false,
            set_model: false,
            set_mode: false,
            set_thinking: false,
            realtime_governance: false,
            text_input: InputDelivery::Native,
            image_input: InputDelivery::Unsupported,
            file_input: InputDelivery::Unsupported,
            steer: SteerSemantics::Unsupported,
            resume: ResumeCapability::Unsupported,
            subagents: SubagentVisibility::None,
            prompt_queue: PromptQueueCapability::Unsupported,
            background_process_control: BackgroundProcessControlCapability::Unsupported,
        }
    }
}

impl SessionCapabilities {
    /// Delivery mode for one input block kind.
    #[must_use]
    pub const fn delivery_for(self, kind: TurnInputBlockKind) -> InputDelivery {
        match kind {
            TurnInputBlockKind::Text => self.text_input,
            TurnInputBlockKind::Image => self.image_input,
            TurnInputBlockKind::File => self.file_input,
        }
    }

    /// Whether this capability set includes `capability`.
    #[must_use]
    pub const fn supports(self, capability: SessionCapability) -> bool {
        match capability {
            SessionCapability::MidTurnSteer => {
                self.mid_turn_steer
                    && matches!(
                        self.steer,
                        SteerSemantics::SameTurn | SteerSemantics::SameTurnOrImmediateNext
                    )
            }
            SessionCapability::SetModel => self.set_model,
            SessionCapability::SetMode => self.set_mode,
            SessionCapability::SetThinking => self.set_thinking,
            SessionCapability::PromptQueue => matches!(
                self.prompt_queue,
                PromptQueueCapability::ServerAuthoritativeVersioned
            ),
            SessionCapability::BackgroundProcessControl => matches!(
                self.background_process_control,
                BackgroundProcessControlCapability::ServerAuthoritativeOwned
            ),
        }
    }
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
    /// A cancellation request was accepted by the transport but the active turn
    /// did not reach a terminal event within the driver's bounded wait. The
    /// session must not be reused until the caller tears it down.
    #[error("session interrupt did not settle: {0}")]
    InterruptPending(String),
    /// This base does not implement the fresh read-only child-session surface.
    /// **Fail-open signal:** callers choose their domain-safe fallback (for
    /// example, deterministic intent routing or an explicit unavailable review) and
    /// never block. A child that is implemented but fails to start may instead
    /// surface [`SessionError::Start`]. The string is a human-readable reason.
    #[error("session fork unsupported: {0}")]
    ForkUnsupported(String),
    /// The selected base has no native implementation for an optional session
    /// capability. Callers may match this variant and apply a visible, safe
    /// fallback instead of guessing from an error string.
    #[error("session capability unsupported: {0}")]
    CapabilityUnsupported(SessionCapability),
    /// A structured input kind cannot be delivered without changing meaning.
    #[error("turn input block {index} ({kind}) is unsupported: {reason}")]
    InputUnsupported {
        /// Zero-based block position; never an attachment path.
        index: usize,
        /// Stable block kind.
        kind: TurnInputBlockKind,
        /// Path-free diagnostic.
        reason: String,
    },
    /// An input or attachment failed bounded validation.
    #[error("turn input block {index} ({kind}) is invalid: {reason}")]
    InputInvalid {
        /// Zero-based block position; never an attachment path.
        index: usize,
        /// Stable block kind.
        kind: TurnInputBlockKind,
        /// Path-free diagnostic.
        reason: String,
    },
}

/// A long-lived base session that the 9-phase runner drives one phase at a
/// time. ONE session spans an entire run; context flows research → docs →
/// code without re-priming. See `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`.
///
/// Contract:
/// - [`capabilities`](Self::capabilities) reports optional native operations.
/// - [`send_turn`](Self::send_turn) injects a phase directive (imperative).
/// - [`steer`](Self::steer) submits live input with the advertised
///   [`SteerSemantics`]. UmaDev must not emulate it by opening a new turn;
///   vendor-native safe-point steering may itself place a missed input at the
///   head of the immediately following turn.
/// - [`next_event`](Self::next_event) is then polled until it yields a
///   [`SessionEvent::TurnDone`]; that marks the phase complete. `None` means
///   the session itself ended (process dead) — treat as a failed turn.
/// - [`respond`](Self::respond) answers a [`SessionEvent::NeedApproval`].
/// - [`respond_host`](Self::respond_host) answers a typed
///   [`SessionEvent::HostRequest`].
/// - [`interrupt`](Self::interrupt) aborts the in-flight turn (ESC / timeout).
/// - [`end`](Self::end) closes the session.
///
/// **Fail-open by contract:** a dead/garbled session surfaces a
/// [`TurnStatus::Failed`] (or `next_event` → `None`), never a panic — a driver
/// bug must never crash the host.
#[async_trait]
pub trait BaseSession: Send {
    /// Optional native operations supported by this live session.
    ///
    /// The default is conservative so existing and third-party implementations
    /// remain source compatible. A driver that opts into a capability must also
    /// override the corresponding method with the real protocol operation.
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::default()
    }

    /// Open a fresh, independent, READ-ONLY child session.
    ///
    /// Despite the historical method name, this MUST NOT resume or branch the
    /// writer transcript. It is a clean model context in the same workspace and
    /// with the same selected model/default, so it may inspect on-disk artifacts
    /// but can never mutate them or collide with the writer. The first-class
    /// drivers implement this as:
    ///
    /// - Claude: a new `--session-id`, `--permission-mode plan`, and only
    ///   `Read,Grep,Glob` tools;
    /// - Codex: a separate app-server with a new `thread/start` in a `read-only`
    ///   sandbox (never `thread/fork` or `thread/resume`);
    /// - OpenCode: a new `POST /session` with a deny-by-default ruleset that only
    ///   allows local source inspection.
    /// - Grok Build: a fresh ACP session launched in its read-only Plan profile.
    ///
    /// The unified surface serves both pre-action intent routing and independent
    /// role critics. A caller may run one strict-JSON turn and close it, or reuse
    /// the healthy child to execute a Chat/Explain turn so the semantic read-only
    /// decision is also enforced by the host permissions.
    ///
    /// **Fail-open by contract:** the default returns
    /// [`SessionError::ForkUnsupported`], and any setup failure is returned to the
    /// caller. The router falls back conservatively; critic callers retain an
    /// explicit unavailable result. A missing child surface therefore never
    /// wedges a host, grants write authority, or masquerades as review success.
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

    /// Switch the current model through the base's native session operation.
    ///
    /// The default is a typed unsupported result; it never restarts a session
    /// or pretends that changing local display state changed the base.
    async fn set_model(
        &mut self,
        _model_id: String,
        _reasoning_effort: Option<SessionReasoningEffort>,
    ) -> Result<(), SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::SetModel,
        ))
    }

    /// Switch the current interaction mode through the base's native session
    /// operation. The default is a typed unsupported result.
    async fn set_mode(&mut self, _mode: SessionMode) -> Result<(), SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::SetMode,
        ))
    }

    /// Switch the base's independent thinking toggle through its native session
    /// operation. The default is a typed unsupported result. A successful
    /// implementation returns the exact full-snapshot state confirmed by the
    /// base, including locked-model availability.
    async fn set_thinking(&mut self, _enabled: bool) -> Result<SessionStateUpdate, SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::SetThinking,
        ))
    }

    /// Inject one phase directive into the live session, starting a turn.
    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError>;

    /// Inject an ordered structured input, starting a turn.
    ///
    /// The compatibility default accepts exactly one text block and delegates
    /// to [`send_turn`](Self::send_turn). It rejects every richer shape rather
    /// than silently flattening it.
    async fn send_input(&mut self, input: TurnInput) -> Result<DeliveryReport, SessionError> {
        let Some(text) = input.sole_text() else {
            let (index, kind) = input
                .blocks
                .iter()
                .enumerate()
                .find(|(_, block)| !matches!(block, TurnInputBlock::Text { .. }))
                .map_or((0, TurnInputBlockKind::Text), |(index, block)| {
                    (index, block.kind())
                });
            return Err(SessionError::InputUnsupported {
                index,
                kind,
                reason: "this session accepts only one text block".to_string(),
            });
        };
        let text = text.to_string();
        let source_bytes = text.len();
        self.send_turn(text).await?;
        Ok(DeliveryReport {
            blocks: vec![BlockDeliveryReport {
                index: 0,
                kind: TurnInputBlockKind::Text,
                delivery: InputDelivery::Native,
                source_bytes,
                media_type: Some("text/plain; charset=utf-8".to_string()),
            }],
            encoded_bytes: None,
            receipt: DeliveryReceiptStage::TransportWritten,
        })
    }

    /// Submit user input through the session's native live steering operation.
    ///
    /// Drivers must override this only for a machine-protocol operation whose
    /// behavior is described exactly by [`SessionCapabilities::steer`]. Method
    /// success proves transport/protocol queuing only, never that the model has
    /// observed the input. The conservative default returns a typed capability
    /// error, allowing callers to queue the input visibly or choose another safe
    /// fallback. It deliberately does not call [`send_turn`](Self::send_turn).
    async fn steer(&mut self, _directive: String) -> Result<(), SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::MidTurnSteer,
        ))
    }

    /// Submit ordered structured input through native live steering.
    ///
    /// The compatibility default accepts exactly one text block and delegates
    /// to [`steer`](Self::steer). Rich input is rejected explicitly.
    async fn steer_input(&mut self, input: TurnInput) -> Result<DeliveryReport, SessionError> {
        let Some(text) = input.sole_text() else {
            let (index, kind) = input
                .blocks
                .iter()
                .enumerate()
                .find(|(_, block)| !matches!(block, TurnInputBlock::Text { .. }))
                .map_or((0, TurnInputBlockKind::Text), |(index, block)| {
                    (index, block.kind())
                });
            return Err(SessionError::InputUnsupported {
                index,
                kind,
                reason: "this session can steer only one text block".to_string(),
            });
        };
        let text = text.to_string();
        let source_bytes = text.len();
        self.steer(text).await?;
        Ok(DeliveryReport {
            blocks: vec![BlockDeliveryReport {
                index: 0,
                kind: TurnInputBlockKind::Text,
                delivery: InputDelivery::Native,
                source_bytes,
                media_type: Some("text/plain; charset=utf-8".to_string()),
            }],
            encoded_bytes: None,
            receipt: DeliveryReceiptStage::TransportWritten,
        })
    }

    /// Submit input to a native, server-authoritative prompt queue while a turn
    /// is already active.
    ///
    /// Implementations must preserve the base's prompt id and keep the protocol
    /// response alive until the queued prompt drains or is removed. Returning
    /// success proves only that the frame was written. The default is a typed
    /// unsupported result and never emulates a queue in local memory.
    async fn enqueue_input(
        &mut self,
        _input: TurnInput,
        _placement: PromptQueuePlacement,
    ) -> Result<DeliveryReport, SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::PromptQueue,
        ))
    }

    /// Send one native queue mutation.
    ///
    /// The server's next [`SessionEvent::PromptQueueChanged`] event is the only
    /// commit signal. Implementations must not report a fire-and-forget write as
    /// a successful queue-state change.
    async fn mutate_prompt_queue(
        &mut self,
        _mutation: PromptQueueMutation,
    ) -> Result<(), SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::PromptQueue,
        ))
    }

    /// Fetch the base's complete native background-process snapshot.
    ///
    /// Implementations must scope every returned task to the exact live session
    /// and must not synthesize task ids from transcript or tool-call text. The
    /// conservative default is a typed unsupported result.
    async fn list_background_processes(
        &mut self,
    ) -> Result<BackgroundProcessSnapshot, SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::BackgroundProcessControl,
        ))
    }

    /// Stop one background process through the base's native operation.
    ///
    /// A driver must perform its advertised ownership check before sending a
    /// destructive stop request. The default never attempts local PID killing or
    /// any other emulation.
    async fn stop_background_process(
        &mut self,
        _task_id: &str,
    ) -> Result<BackgroundProcessStopOutcome, SessionError> {
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::BackgroundProcessControl,
        ))
    }

    /// Pull the next event of the in-flight turn. Yields events until a
    /// [`SessionEvent::TurnDone`]; `None` once the underlying session ends.
    async fn next_event(&mut self) -> Option<SessionEvent>;

    /// Answer a [`SessionEvent::NeedApproval`] (governance / gate decision).
    async fn respond(
        &mut self,
        req_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError>;

    /// Answer a typed [`SessionEvent::HostRequest`].
    ///
    /// New protocol drivers override this method to encode structured answers,
    /// selected permission-option ids, MCP elicitation outcomes, and plan
    /// feedback. The default is intentionally backward compatible and safe:
    /// an [`HostResponse::Approval`] is forwarded to the legacy
    /// [`respond`](Self::respond) method, while every richer or mismatched reply
    /// is converted to `Deny`. Thus adding this method does not break any
    /// existing session implementation and an unsupported request can never be
    /// accidentally authorised.
    async fn respond_host(
        &mut self,
        req_id: &str,
        response: HostResponse,
    ) -> Result<(), SessionError> {
        let decision = match response {
            HostResponse::Approval { decision, .. } => decision,
            HostResponse::UserInput { .. }
            | HostResponse::UserInputOutcome { .. }
            | HostResponse::PermissionExpansion { .. }
            | HostResponse::McpElicitation { .. }
            | HostResponse::PlanConfirmation { .. }
            | HostResponse::PlanOutcome { .. }
            | HostResponse::FolderTrust { .. }
            | HostResponse::Cancelled { .. }
            | HostResponse::Rejected { .. } => ApprovalDecision::Deny,
        };
        self.respond(req_id, decision).await
    }

    /// Abort the in-flight turn (ESC / abort / timeout). `Ok(())` means the turn
    /// is terminal and the session can safely accept another turn; a driver must
    /// return an error instead of claiming success while cancellation is pending.
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

    /// Immutable authority/effective-sandbox identity for [`Self::session_id`].
    ///
    /// The default is deliberately absent: a caller may persist the id only with
    /// a separately constructed requested-only identity, which is insufficient
    /// to resume Grok Build. A driver that can report post-policy effective state
    /// and complete the vendor's native pre-start resume preflight can override
    /// this seam with attested evidence.
    fn resume_identity(&self) -> Option<&BaseResumeIdentity> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_default_is_unknown_and_whole_prompt_merge_preserves_quality() {
        let unknown = Usage::default();
        assert!(unknown.usage_incomplete);
        assert!(unknown.has_empty_lower_bound());
        assert_eq!(unknown.trusted_cost_usd_ticks(), None);
        assert_eq!(unknown.scope, UsageScope::WholePrompt);

        let exact = Usage {
            cached_read_tokens: 3,
            cached_write_tokens: 2,
            reasoning_tokens: 1,
            model_calls: 2,
            num_turns: 1,
            cost_usd_ticks: Some(10),
            ..Usage::exact(8, 2)
        };
        let partial = Usage {
            model_calls: 1,
            num_turns: 1,
            cost_usd_ticks: Some(20),
            cost_partial: true,
            ..Usage::exact(4, 1)
        };
        let merged = exact.merge(partial);
        assert_eq!(merged.total_tokens, 15);
        assert_eq!(merged.cached_read_tokens, 3);
        assert_eq!(merged.cached_write_tokens, 2);
        assert_eq!(merged.reasoning_tokens, 1);
        assert_eq!(merged.model_calls, 3);
        assert_eq!(merged.num_turns, 2);
        assert!(merged.cost_partial);
        assert_eq!(merged.cost_usd_ticks, None);
    }

    #[test]
    fn exact_usage_costs_merge_without_narrowing_or_rounding() {
        let above_u32 = u64::from(u32::MAX) + 1;
        let left = Usage {
            cost_usd_ticks: Some(7),
            ..Usage::exact(above_u32, 2)
        };
        let right = Usage {
            cost_usd_ticks: Some(11),
            ..Usage::exact(3, 4)
        };
        let merged = left.merge(right);
        assert_eq!(merged.input_tokens, above_u32 + 3);
        assert_eq!(merged.total_tokens, above_u32 + 9);
        assert_eq!(merged.trusted_cost_usd_ticks(), Some(18));
    }

    #[test]
    fn exact_zero_cost_is_trusted_and_merges_as_zero() {
        let free = Usage {
            cost_usd_ticks: Some(0),
            ..Usage::exact(4, 1)
        };
        let paid = Usage {
            cost_usd_ticks: Some(9),
            ..Usage::exact(2, 2)
        };
        assert_eq!(free.trusted_cost_usd_ticks(), Some(0));
        assert_eq!(free.merge(paid).trusted_cost_usd_ticks(), Some(9));

        let malformed = Usage {
            cost_usd_ticks: Some(-1),
            ..Usage::exact(1, 1)
        };
        assert_eq!(malformed.trusted_cost_usd_ticks(), None);
    }

    fn grok_resume_identity(effective_sandbox: EffectiveSandboxEvidence) -> BaseResumeIdentity {
        BaseResumeIdentity {
            backend: "grok-build".to_string(),
            canonical_workspace: PathBuf::from("/canonical/workspace"),
            permission_profile: BasePermissionProfile::Guarded,
            requested_sandbox: BaseSandboxRequest::Off,
            effective_sandbox,
            native_resume_preflight_required: true,
        }
    }

    #[test]
    fn requested_sandbox_is_not_effective_access_evidence() {
        for requested in [
            BaseSandboxRequest::Off,
            BaseSandboxRequest::ReadOnly,
            BaseSandboxRequest::DangerFullAccess,
        ] {
            let evidence = EffectiveSandboxEvidence::requested_only(requested);
            assert!(!evidence.proves_full_access());
            assert!(!evidence.proves_read_only_filesystem());
            assert_eq!(evidence.network, EffectiveIoAccess::Unknown);
            assert_eq!(evidence.local_ports, EffectiveIoAccess::Unknown);
        }
    }

    #[test]
    fn native_resume_preflight_never_attests_os_sandbox_application() {
        let claimed_full = EffectiveSandboxEvidence {
            status: SandboxEffectiveStatus::Disabled,
            resolved_profile: Some(BaseSandboxRequest::Off),
            source: SandboxEvidenceSource::NativeResumePreflight,
            filesystem: EffectiveFilesystemAccess::Unrestricted,
            network: EffectiveIoAccess::Allowed,
            local_ports: EffectiveIoAccess::Allowed,
            detail: None,
        };
        assert!(!claimed_full.proves_full_access());
        let saved = grok_resume_identity(claimed_full);
        let requested = BaseResumeIdentity::requested_only(
            "grok-build",
            PathBuf::from("/canonical/workspace"),
            BasePermissionProfile::Guarded,
            BaseSandboxRequest::Off,
            true,
        );
        assert!(
            !saved.permits_resume_as(&requested, true),
            "configuration preflight alone cannot authorize Grok session/load"
        );

        let claimed_read_only = EffectiveSandboxEvidence {
            status: SandboxEffectiveStatus::Enforced,
            resolved_profile: Some(BaseSandboxRequest::ReadOnly),
            source: SandboxEvidenceSource::NativeResumePreflight,
            filesystem: EffectiveFilesystemAccess::ReadOnly,
            network: EffectiveIoAccess::Blocked,
            local_ports: EffectiveIoAccess::Blocked,
            detail: None,
        };
        assert!(!claimed_read_only.proves_read_only_filesystem());
    }

    #[test]
    fn grok_resume_requires_exact_identity_effective_attestation_and_live_preflight() {
        let effective = EffectiveSandboxEvidence {
            status: SandboxEffectiveStatus::Disabled,
            resolved_profile: Some(BaseSandboxRequest::Off),
            source: SandboxEvidenceSource::VendorProtocol,
            filesystem: EffectiveFilesystemAccess::Unrestricted,
            network: EffectiveIoAccess::Allowed,
            local_ports: EffectiveIoAccess::Allowed,
            detail: Some("post-policy effective state".to_string()),
        };
        let saved = grok_resume_identity(effective);
        let requested = BaseResumeIdentity::requested_only(
            "grok-build",
            PathBuf::from("/canonical/workspace"),
            BasePermissionProfile::Guarded,
            BaseSandboxRequest::Off,
            true,
        );
        assert!(!saved.permits_resume_as(&requested, false));
        assert!(saved.permits_resume_as(&requested, true));

        let mut mismatch = requested.clone();
        mismatch.permission_profile = BasePermissionProfile::Auto;
        assert!(!saved.permits_resume_as(&mismatch, true));
        mismatch = requested.clone();
        mismatch.canonical_workspace = PathBuf::from("/other/workspace");
        assert!(!saved.permits_resume_as(&mismatch, true));
        mismatch = requested;
        mismatch.backend = "codex".to_string();
        assert!(!saved.permits_resume_as(&mismatch, true));
    }

    #[test]
    fn effective_io_is_modeled_separately_from_filesystem_and_degradation() {
        let mac_like = EffectiveSandboxEvidence {
            status: SandboxEffectiveStatus::Enforced,
            resolved_profile: Some(BaseSandboxRequest::ReadOnly),
            source: SandboxEvidenceSource::VendorProtocol,
            filesystem: EffectiveFilesystemAccess::ReadOnly,
            network: EffectiveIoAccess::Allowed,
            local_ports: EffectiveIoAccess::Allowed,
            detail: Some("platform permits network and listeners".to_string()),
        };
        let linux_like = EffectiveSandboxEvidence {
            network: EffectiveIoAccess::Blocked,
            local_ports: EffectiveIoAccess::Blocked,
            ..mac_like.clone()
        };
        assert!(mac_like.proves_read_only_filesystem());
        assert!(linux_like.proves_read_only_filesystem());
        assert_ne!(mac_like.network, linux_like.network);
        assert_ne!(mac_like.local_ports, linux_like.local_ports);

        for status in [
            SandboxEffectiveStatus::Unknown,
            SandboxEffectiveStatus::Degraded,
        ] {
            let unresolved = EffectiveSandboxEvidence {
                status,
                ..mac_like.clone()
            };
            assert!(!unresolved.proves_read_only_filesystem());
            assert!(!unresolved.proves_full_access());
        }
    }

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
        for alias in ["/codex", "/opencode", "/grok", "/kimi"] {
            assert!(
                r.contains(alias),
                "offline guidance must advertise every first-class base alias ({alias}): {r}"
            );
        }
    }

    #[test]
    fn offline_chat_reply_with_no_ask_still_guides_to_a_base() {
        // An empty / whitespace-only ask still yields the base-less guidance line,
        // never an empty string.
        let r = offline_chat_reply(&req(vec![("user", "   ")]));
        assert!(!r.trim().is_empty());
        for alias in ["/claude", "/codex", "/opencode", "/grok", "/kimi"] {
            assert!(
                r.contains(alias),
                "empty offline guidance must advertise every first-class base alias ({alias}): {r}"
            );
        }
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

    // ── typed host interaction + correlated tool events ───────────────────

    #[derive(Default)]
    struct LegacyApprovalSession {
        turns: Vec<String>,
        responses: Vec<(String, ApprovalDecision)>,
    }

    #[async_trait]
    impl BaseSession for LegacyApprovalSession {
        async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
            self.turns.push(directive);
            Ok(())
        }

        async fn next_event(&mut self) -> Option<SessionEvent> {
            None
        }

        async fn respond(
            &mut self,
            req_id: &str,
            decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            self.responses.push((req_id.to_string(), decision));
            Ok(())
        }

        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }

        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn base_session_defaults_to_detectable_unsupported_steer() {
        let mut session = LegacyApprovalSession::default();
        let capabilities = session.capabilities();
        assert!(!capabilities.mid_turn_steer);
        assert!(!capabilities.supports(SessionCapability::MidTurnSteer));
        assert!(!capabilities.supports(SessionCapability::SetModel));
        assert!(!capabilities.supports(SessionCapability::SetMode));
        assert!(!capabilities.supports(SessionCapability::PromptQueue));
        assert!(!capabilities.supports(SessionCapability::BackgroundProcessControl));
        assert_eq!(
            capabilities.delivery_for(TurnInputBlockKind::Text),
            InputDelivery::Native
        );
        assert_eq!(
            capabilities.delivery_for(TurnInputBlockKind::Image),
            InputDelivery::Unsupported
        );

        let error = session
            .steer("append this to the active turn".to_string())
            .await
            .expect_err("legacy sessions must not fake steer with a new turn");
        assert!(matches!(
            error,
            SessionError::CapabilityUnsupported(SessionCapability::MidTurnSteer)
        ));
        assert!(
            session.turns.is_empty(),
            "the default steer implementation must not delegate to send_turn"
        );

        let error = session
            .set_model(
                "catalog-model".to_string(),
                Some(SessionReasoningEffort::High),
            )
            .await
            .expect_err("legacy sessions must not fake a model switch");
        assert!(matches!(
            error,
            SessionError::CapabilityUnsupported(SessionCapability::SetModel)
        ));
        let error = session
            .set_mode(SessionMode::Plan)
            .await
            .expect_err("legacy sessions must not fake a mode switch");
        assert!(matches!(
            error,
            SessionError::CapabilityUnsupported(SessionCapability::SetMode)
        ));
        let error = session
            .enqueue_input(TurnInput::text("queued"), PromptQueuePlacement::Tail)
            .await
            .expect_err("legacy sessions must not emulate a native prompt queue");
        assert!(matches!(
            error,
            SessionError::CapabilityUnsupported(SessionCapability::PromptQueue)
        ));
        let error = session
            .list_background_processes()
            .await
            .expect_err("legacy sessions must not synthesize a process list");
        assert!(matches!(
            error,
            SessionError::CapabilityUnsupported(SessionCapability::BackgroundProcessControl)
        ));
        let error = session
            .stop_background_process("task-1")
            .await
            .expect_err("legacy sessions must not kill a local pid as an emulation");
        assert!(matches!(
            error,
            SessionError::CapabilityUnsupported(SessionCapability::BackgroundProcessControl)
        ));
        let error = session
            .mutate_prompt_queue(PromptQueueMutation::Clear)
            .await
            .expect_err("legacy sessions must not claim a local clear is server state");
        assert!(matches!(
            error,
            SessionError::CapabilityUnsupported(SessionCapability::PromptQueue)
        ));
    }

    #[test]
    fn session_mode_and_reasoning_effort_are_closed_exact_wire_sets() {
        assert_eq!(SessionMode::try_from("default"), Ok(SessionMode::Default));
        assert_eq!(SessionMode::try_from("plan"), Ok(SessionMode::Plan));
        assert_eq!(SessionMode::try_from("ask"), Ok(SessionMode::Ask));
        assert!(SessionMode::try_from("bypassPermissions").is_err());
        assert!(SessionMode::try_from("Plan").is_err());

        assert_eq!(
            SessionReasoningEffort::try_from("xhigh"),
            Ok(SessionReasoningEffort::Xhigh)
        );
        assert!(SessionReasoningEffort::try_from("max").is_err());
        assert!(SessionReasoningEffort::try_from("future-tier").is_err());
    }

    #[test]
    fn safe_point_steer_is_supported_but_not_strict_same_turn() {
        let safe_point = SessionCapabilities {
            mid_turn_steer: true,
            steer: SteerSemantics::SameTurnOrImmediateNext,
            ..SessionCapabilities::default()
        };
        assert!(safe_point.supports(SessionCapability::MidTurnSteer));
        assert_ne!(safe_point.steer, SteerSemantics::SameTurn);
        assert_eq!(
            serde_json::to_string(&safe_point.steer).unwrap(),
            "\"same_turn_or_immediate_next\""
        );

        let disabled = SessionCapabilities {
            mid_turn_steer: false,
            ..safe_point
        };
        assert!(!disabled.supports(SessionCapability::MidTurnSteer));
    }

    #[test]
    fn prompt_queue_contract_round_trips_versions_and_never_implies_a_commit() {
        let snapshot = PromptQueueSnapshot {
            session_id: "s1".to_string(),
            entries: vec![PromptQueueEntry {
                id: "p1".to_string(),
                version: 7,
                owner: Some("client-a".to_string()),
                last_editor: Some("client-b".to_string()),
                kind: "prompt".to_string(),
                text: "fix it".to_string(),
                position: 0,
            }],
            running_prompt_id: Some("p0".to_string()),
        };
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_str::<PromptQueueSnapshot>(&encoded).unwrap(),
            snapshot
        );
        let mutation = PromptQueueMutation::Interject {
            id: "p1".to_string(),
            expected_version: 7,
            new_text: Some("fix it now".to_string()),
        };
        let encoded = serde_json::to_string(&mutation).unwrap();
        assert_eq!(
            serde_json::from_str::<PromptQueueMutation>(&encoded).unwrap(),
            mutation
        );
        let capabilities = SessionCapabilities {
            prompt_queue: PromptQueueCapability::ServerAuthoritativeVersioned,
            ..SessionCapabilities::default()
        };
        assert!(capabilities.supports(SessionCapability::PromptQueue));
    }

    #[test]
    fn background_process_control_is_scoped_typed_and_opt_in() {
        let snapshot = BackgroundProcessSnapshot {
            session_id: "session-1".to_string(),
            processes: vec![BackgroundProcessSnapshotEntry {
                task_id: "task-1".to_string(),
                kind: BackgroundProcessKind::Monitor,
                completed: true,
                exit_code: None,
                signal: Some("killed".to_string()),
                truncated: false,
            }],
        };
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_str::<BackgroundProcessSnapshot>(&encoded).unwrap(),
            snapshot
        );
        assert!(!encoded.contains("command"));
        assert!(!encoded.contains("output"));

        let capabilities = SessionCapabilities {
            background_process_control:
                BackgroundProcessControlCapability::ServerAuthoritativeOwned,
            ..SessionCapabilities::default()
        };
        assert!(capabilities.supports(SessionCapability::BackgroundProcessControl));
        assert_eq!(
            serde_json::to_string(&BackgroundProcessStopOutcome::AlreadyExited).unwrap(),
            "\"already_exited\""
        );
    }

    #[tokio::test]
    async fn structured_input_default_is_text_compatible_and_never_flattens_files() {
        let mut session = LegacyApprovalSession::default();
        let report = session.send_input(TurnInput::text("hello")).await.unwrap();
        assert_eq!(session.turns, vec!["hello"]);
        assert_eq!(report.blocks[0].delivery, InputDelivery::Native);
        assert_eq!(report.receipt, DeliveryReceiptStage::TransportWritten);

        let error = session
            .send_input(TurnInput::new(vec![
                TurnInputBlock::Text {
                    text: "before".into(),
                },
                TurnInputBlock::File {
                    path: PathBuf::from("never-read.txt"),
                    mode: FileInputMode::MaterializeText,
                },
            ]))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SessionError::InputUnsupported {
                index: 1,
                kind: TurnInputBlockKind::File,
                ..
            }
        ));
        assert_eq!(session.turns, vec!["hello"]);
    }

    #[test]
    fn delivery_receipt_stage_is_backward_compatible_and_never_implies_processing() {
        let legacy = r#"{"blocks":[],"encoded_bytes":12}"#;
        let report: DeliveryReport = serde_json::from_str(legacy).unwrap();
        assert_eq!(report.receipt, DeliveryReceiptStage::TransportWritten);

        let acknowledged = DeliveryReport {
            blocks: Vec::new(),
            encoded_bytes: Some(12),
            receipt: DeliveryReceiptStage::ProtocolAcknowledged,
        };
        let encoded = serde_json::to_string(&acknowledged).unwrap();
        assert!(encoded.contains(r#""receipt":"protocol_acknowledged""#));
    }

    #[test]
    fn structured_input_roundtrip_preserves_block_order() {
        let input = TurnInput::new(vec![
            TurnInputBlock::Text { text: "甲".into() },
            TurnInputBlock::Image {
                path: PathBuf::from("图片 空格.png"),
            },
            TurnInputBlock::Text { text: "乙".into() },
        ]);
        let encoded = serde_json::to_string(&input).unwrap();
        let decoded: TurnInput = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, input);
        assert_eq!(decoded.blocks[1].kind(), TurnInputBlockKind::Image);
        assert!(!format!("{decoded:?}").contains("图片 空格.png"));
    }

    #[tokio::test]
    async fn respond_host_preserves_legacy_approval_and_rejects_richer_replies() {
        let mut session = LegacyApprovalSession::default();
        session
            .respond_host(
                "approval-1",
                HostResponse::Approval {
                    decision: ApprovalDecision::Allow,
                    selected_option_id: Some("allow_once".into()),
                    message: None,
                },
            )
            .await
            .unwrap();
        session
            .respond_host(
                "question-1",
                HostResponse::UserInput {
                    answers: vec![HostAnswer {
                        question_id: "database".into(),
                        values: vec!["Postgres".into()],
                    }],
                },
            )
            .await
            .unwrap();

        assert_eq!(
            session.responses,
            vec![
                ("approval-1".into(), ApprovalDecision::Allow),
                ("question-1".into(), ApprovalDecision::Deny),
            ]
        );
    }

    #[test]
    fn typed_host_request_round_trips_and_has_a_safe_protocol_rejection() {
        let request = HostRequest::PermissionExpansion {
            permissions: vec![HostPermission {
                kind: "filesystem_write".into(),
                target: Some("/outside/workspace".into()),
                metadata: serde_json::json!({"scope": "recursive"}),
            }],
            reason: Some("write generated output".into()),
            metadata: serde_json::Value::Null,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<HostRequest>(encoded).unwrap(),
            request
        );
        assert!(matches!(
            request.safe_rejection("not approved"),
            HostResponse::PermissionExpansion {
                decision: ApprovalDecision::Deny,
                granted,
                message: Some(reason),
            } if granted.is_empty() && reason == "not approved"
        ));

        let unknown = HostRequest::Unknown {
            method: "vendor/futureAction".into(),
            payload: serde_json::json!({"redacted": true}),
        };
        assert_eq!(
            unknown.safe_rejection("unsupported method"),
            HostResponse::Rejected {
                reason: "unsupported method".into()
            }
        );
    }

    #[test]
    fn grok_interaction_contract_round_trips_preview_notes_and_typed_rejections() {
        let question = HostQuestion {
            id: "local-q1".into(),
            header: Some("Database".into()),
            prompt: "Which database?".into(),
            kind: HostQuestionKind::SingleChoice,
            required: true,
            options: vec![HostQuestionOption {
                value: "option-1".into(),
                label: "Postgres".into(),
                description: Some("Relational".into()),
                preview: Some("CREATE TABLE users (...)".into()),
            }],
        };
        let response = HostResponse::UserInputOutcome {
            outcome: HostUserInputOutcome::Accepted {
                answers: vec![HostAnswer {
                    question_id: question.id.clone(),
                    values: vec!["option-1".into()],
                }],
                annotations: vec![HostQuestionAnnotation {
                    question_id: question.id.clone(),
                    preview: question.options[0].preview.clone(),
                    notes: Some("Keep migrations reversible".into()),
                }],
            },
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(
            serde_json::from_value::<HostResponse>(encoded).unwrap(),
            response
        );

        let grok_question = HostRequest::UserInput {
            questions: vec![question.clone()],
            metadata: serde_json::json!({
                "responseContract":"grok_ask_user_question_v1"
            }),
        };
        assert_eq!(
            grok_question.safe_rejection("closed"),
            HostResponse::UserInputOutcome {
                outcome: HostUserInputOutcome::Cancelled
            }
        );
        let generic_question = HostRequest::UserInput {
            questions: vec![question],
            metadata: serde_json::json!({
                "responseContract":"grok_ask_user_question_v2"
            }),
        };
        assert_eq!(
            generic_question.safe_rejection("closed"),
            HostResponse::Rejected {
                reason: "closed".into()
            }
        );

        let kimi_plan_review = HostRequest::UserInput {
            questions: Vec::new(),
            metadata: serde_json::json!({
                "responseContract":"kimi_plan_review_permission_v1"
            }),
        };
        assert_eq!(
            kimi_plan_review.safe_rejection("closed"),
            HostResponse::Cancelled {
                reason: Some("closed".into())
            }
        );

        // The kimi permission QUESTION contract must refuse as a protocol
        // Cancelled too — before it carried no responseContract and fell to the
        // generic Rejected, which the kimi driver has no write-back arm for
        // (every refusal path then wrote a -32602 error to a blocking request).
        let kimi_question = HostRequest::UserInput {
            questions: Vec::new(),
            metadata: serde_json::json!({
                "responseContract":"kimi_permission_question_v1"
            }),
        };
        assert_eq!(
            kimi_question.safe_rejection("closed"),
            HostResponse::Cancelled {
                reason: Some("closed".into())
            }
        );

        let grok_plan = HostRequest::PlanConfirmation {
            plan: "Deploy safely".into(),
            message: None,
            metadata: serde_json::json!({
                "responseContract":"grok_exit_plan_mode_v1"
            }),
        };
        assert!(matches!(
            grok_plan.safe_rejection("closed"),
            HostResponse::PlanOutcome {
                outcome: HostPlanOutcome::Cancelled {
                    feedback: Some(reason)
                }
            } if reason == "closed"
        ));
    }

    #[test]
    fn correlated_tool_events_keep_parallel_call_ids_without_changing_legacy_shapes() {
        let first = SessionEvent::ToolCallCorrelated {
            call_id: "tool-a".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "a.rs"}),
        };
        let second = SessionEvent::ToolResultCorrelated {
            call_id: "tool-b".into(),
            ok: true,
            summary: "done".into(),
        };
        assert_eq!(first.tool_call_id(), Some("tool-a"));
        assert_eq!(second.tool_call_id(), Some("tool-b"));

        // Existing constructors and pattern shapes remain exactly valid for
        // streams that expose no stable id.
        let legacy = SessionEvent::ToolCall {
            name: "Read".into(),
            input: serde_json::json!({"path": "legacy.rs"}),
        };
        assert_eq!(legacy.tool_call_id(), None);
    }

    #[test]
    fn tool_activity_keeps_parallel_calls_active_until_every_matching_result() {
        let mut activity = ToolActivity::default();
        assert!(activity.observe(&SessionEvent::ToolCallCorrelated {
            call_id: "tool-a".into(),
            name: "Read".into(),
            input: serde_json::Value::Null,
        }));
        assert!(activity.observe(&SessionEvent::ToolCallCorrelated {
            call_id: "tool-b".into(),
            name: "Bash".into(),
            input: serde_json::Value::Null,
        }));
        assert!(activity.observe(&SessionEvent::ToolResultCorrelated {
            call_id: "tool-a".into(),
            ok: true,
            summary: "first finished".into(),
        }));
        assert!(!activity.observe(&SessionEvent::ToolResultCorrelated {
            call_id: "tool-b".into(),
            ok: true,
            summary: "second finished".into(),
        }));
    }

    #[test]
    fn tool_activity_counts_legacy_calls_and_resets_at_turn_boundary() {
        let mut activity = ToolActivity::default();
        for name in ["Read", "Bash"] {
            assert!(activity.observe(&SessionEvent::ToolCall {
                name: name.into(),
                input: serde_json::Value::Null,
            }));
        }
        assert!(activity.observe(&SessionEvent::ToolResult {
            ok: true,
            summary: "one finished".into(),
        }));
        assert!(!activity.observe(&SessionEvent::TurnDone {
            status: TurnStatus::Completed,
            usage: None,
        }));
    }

    #[test]
    fn tool_activity_overflow_stays_conservatively_active_until_clear() {
        let mut activity = ToolActivity::default();
        for index in 0..=ToolActivity::MAX_CORRELATED {
            assert!(activity.observe(&SessionEvent::ToolCallCorrelated {
                call_id: format!("tool-{index}"),
                name: "Read".into(),
                input: serde_json::Value::Null,
            }));
        }
        for index in 0..ToolActivity::MAX_CORRELATED {
            assert!(activity.observe(&SessionEvent::ToolResultCorrelated {
                call_id: format!("tool-{index}"),
                ok: true,
                summary: String::new(),
            }));
        }
        activity.clear();
        assert!(!activity.is_active());
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

    #[test]
    fn ask_user_question_prose_block_drops_the_numbered_pick_framing() {
        let input = serde_json::json!({
            "question": "Which database should the API use?",
            "options": [
                {"label": "Postgres", "description": "Relational"},
                {"label": "MongoDB"}
            ]
        });
        let q = AskUserQuestion::parse_value(&input).expect("parses");
        let prose = q.prose_block();
        // Options are still listed (so the user knows what's being weighed) but as
        // plain bullets — NO "1. " / "2. " numbering that implies a numeric pick.
        assert!(prose.contains("Which database"), "prose: {prose}");
        assert!(prose.contains("- Postgres"), "bulleted option: {prose}");
        assert!(prose.contains("- MongoDB"), "bulleted option: {prose}");
        assert!(
            !prose.contains("1. Postgres") && !prose.contains("2. MongoDB"),
            "text mode must drop the numbered picker framing: {prose}"
        );
    }

    // ── ExitPlanMode (the BASE's plan mode — distinct from UmaDev Guarded) ───

    #[test]
    fn exit_plan_mode_parses_plan_text_and_summarizes() {
        let input = serde_json::json!({
            "plan": "## Plan\n1. Scaffold the API\n2. Add auth\n3. Wire the UI"
        });
        let p = ExitPlanMode::from_tool_input("ExitPlanMode", &input)
            .expect("ExitPlanMode input must parse");
        assert!(
            p.plan.contains("Scaffold the API"),
            "full plan kept: {}",
            p.plan
        );
        // The one-line detail is the first non-empty plan line, never multi-line.
        let summary = p.summary();
        assert!(!summary.is_empty());
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn exit_plan_mode_name_match_and_fail_open() {
        assert!(ExitPlanMode::is_tool_name("ExitPlanMode"));
        assert!(ExitPlanMode::is_tool_name("exit_plan_mode"));
        assert!(!ExitPlanMode::is_tool_name("AskUserQuestion"));
        assert!(!ExitPlanMode::is_tool_name("Write"));
        // A non-plan tool / an empty-or-absent plan fails open to None (the caller
        // keeps its plain tool row; never a panic, never a fabricated plan).
        assert!(
            ExitPlanMode::from_tool_input("Write", &serde_json::json!({"file_path": "a"}))
                .is_none()
        );
        assert!(
            ExitPlanMode::from_tool_input("ExitPlanMode", &serde_json::json!({"plan": "   "}))
                .is_none()
        );
        assert!(ExitPlanMode::from_tool_input("ExitPlanMode", &serde_json::json!({})).is_none());
    }
}
