//! `ClaudeCodeDriver` — drives the `claude` CLI in non-interactive
//! print mode.
//!
//! The driver shells out to `claude --print "<prompt>"`. Because the
//! user has already authenticated `claude` (subscription / OAuth), no
//! API key is needed — the host CLI bills the user's existing session.
//!
//! The program name and print flag are overridable for forward
//! compatibility if the CLI's flags change:
//!
//! - `UMADEV_CLAUDE_BIN`  — program name (default `claude`)
//! - `UMADEV_CLAUDE_PRINT_FLAG` — print flag (default `--print`)

use std::time::Duration;

use async_trait::async_trait;
use umadev_runtime::{
    CompletionRequest, CompletionResponse, Runtime, RuntimeError, RuntimeKind, Usage,
};

use crate::{
    default_workspace, merge_prompt, model_args, run_subprocess, run_subprocess_streaming,
    HostDriver, ProbeResult, PromptChannel, SubprocessCall,
};

/// Drives the `claude` CLI as a subprocess.
#[derive(Debug, Clone)]
pub struct ClaudeCodeDriver {
    program: String,
    print_flag: String,
    timeout: Duration,
    /// When `true`, the next `complete` resumes the `claude` conversation
    /// instead of starting cold. Set per-call by the TUI for chat turns 2+.
    continue_session: bool,
    /// An explicit conversation id (UUID) the TUI pins for its chat session.
    /// When set, turn 1 creates the session with `--session-id <uuid>` and
    /// later turns resume it with `--resume <uuid>` — deterministic, so we
    /// never accidentally continue the user's *other* `claude` conversation
    /// in the same directory. When `None`, falls back to `--continue`
    /// ("most recent in this dir").
    session_id: Option<String>,
    /// The cwd the `claude` subprocess runs in (the pipeline project root).
    /// `None` → the launching process's cwd.
    workspace: Option<std::path::PathBuf>,
}

impl Default for ClaudeCodeDriver {
    fn default() -> Self {
        Self {
            program: std::env::var("UMADEV_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string()),
            print_flag: std::env::var("UMADEV_CLAUDE_PRINT_FLAG")
                .unwrap_or_else(|_| "--print".to_string()),
            timeout: crate::worker_timeout_from_env(),
            continue_session: false,
            session_id: None,
            workspace: None,
        }
    }
}

impl ClaudeCodeDriver {
    /// Build a driver with explicit settings (mainly for tests).
    #[must_use]
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    /// Override the per-call timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builder form of [`HostDriver::set_continue_session`] (mainly for tests).
    #[must_use]
    pub fn with_continue_session(mut self, continue_session: bool) -> Self {
        self.continue_session = continue_session;
        self
    }

    /// Builder form of [`HostDriver::set_session_id`] (mainly for tests).
    #[must_use]
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// The full argument vector for a `complete` call, resolving the session
    /// strategy. Exposed for tests. The prompt is appended by the subprocess
    /// layer as the last positional argument.
    ///
    /// - explicit id + resume  → `--resume <uuid>`   (continue our own session)
    /// - explicit id + fresh   → `--session-id <uuid>` (create it with our id)
    /// - no id + resume        → `--continue`         (most recent in this dir)
    /// - no id + fresh         → (nothing)            (brand-new conversation)
    #[must_use]
    pub fn call_args(&self) -> Vec<String> {
        self.call_args_with_format("text")
    }

    /// [`Self::call_args`] but with the `--output-format` value spelled out so
    /// the non-streaming `complete` can request `"json"` (single `result`
    /// envelope) and read real token `usage`. Session handling is identical to
    /// [`Self::call_args`]; only the output format differs.
    #[must_use]
    pub fn call_args_with_format(&self, output_format: &str) -> Vec<String> {
        let mut args = self.base_args_with_format(output_format);
        match (&self.session_id, self.continue_session) {
            (Some(id), true) => {
                args.push("--resume".to_string());
                args.push(id.clone());
            }
            (Some(id), false) => {
                args.push("--session-id".to_string());
                args.push(id.clone());
            }
            (None, true) => args.push("--continue".to_string()),
            (None, false) => {}
        }
        args
    }

    /// The argument vector preceding the prompt, with the output format made
    /// explicit. Exposed for tests.
    ///
    /// `output_format` is `"text"` for the non-streaming path's plain-markdown
    /// answer, or `"json"` when the caller wants the single `result` envelope
    /// (so it can read real token `usage`). Both share the same permission and
    /// session handling; only the `--output-format` value differs.
    ///
    /// Flag rationale:
    /// - `--print` (or `-p`): non-interactive single-shot mode.
    /// - `--dangerously-skip-permissions`: bypass all tool permission prompts
    ///   so the pipeline runs fully autonomously — Claude can read/write
    ///   files and run bash without waiting for per-call approval. This is
    ///   essential because UmaDev drives the host as an unattended
    ///   subprocess; without it, every `Write` / `Bash` call would hang
    ///   waiting for a y/n that never comes. UmaDev's own governance
    ///   layer (112 rules, `PreToolUse` hook, quality gate) is the safety net.
    /// - `--output-format text`: explicit text output — no JSON envelope
    ///   so the existing `clean_output` pipeline gets plain markdown.
    ///
    /// Deliberately **does NOT** pass `--bare`. Anthropic's headless
    /// docs recommend `--bare` for CI, but bare mode skips OAuth and
    /// keychain reads, requiring `ANTHROPIC_API_KEY`. UmaDev's whole
    /// pitch is "drive your already-logged-in subscription", so the
    /// keychain MUST be reachable — `--bare` would break the very
    /// users we exist to serve.
    ///
    /// Permission bypass can be disabled by setting
    /// `UMADEV_NO_SKIP_PERMS=1` (e.g. if a corporate policy blocks it).
    #[must_use]
    pub fn base_args(&self) -> Vec<String> {
        self.base_args_with_format("text")
    }

    /// [`Self::base_args`] but with the `--output-format` value spelled out so
    /// the non-streaming `complete` can request `"json"` and read real `usage`
    /// off the single `result` envelope. See [`Self::base_args`] for the flag
    /// rationale.
    #[must_use]
    pub fn base_args_with_format(&self, output_format: &str) -> Vec<String> {
        let mut args = vec![
            self.print_flag.clone(),
            "--output-format".to_string(),
            output_format.to_string(),
        ];
        // Auto-skip permission prompts so the pipeline is fully autonomous.
        // UmaDev's governance layer replaces the host's permission system.
        if std::env::var("UMADEV_NO_SKIP_PERMS").as_deref() != Ok("1") {
            args.push("--dangerously-skip-permissions".to_string());
        }
        args
    }
}

#[async_trait]
impl Runtime for ClaudeCodeDriver {
    /// Concurrent-safe fork: clone with a FRESH session (no resume) so
    /// parallel pipeline steps don't collide on one Claude session.
    fn fork(&self) -> Option<Box<dyn Runtime>> {
        Some(Box::new(
            self.clone()
                .with_continue_session(false)
                .with_session_id(None),
        ))
    }

    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Anthropic
    }

    fn capabilities(&self) -> umadev_runtime::BrainCapabilities {
        // Claude Code is the most capable base: persistent `/goal` mode,
        // stream-json streaming, real usage on the result line, and the
        // PreToolUse real-time governance hook.
        umadev_runtime::BrainCapabilities {
            persistent_goal: true,
            streaming: true,
            reports_usage: true,
            realtime_governance: true,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, RuntimeError> {
        let prompt = merge_prompt(&req);
        // Request `--output-format json` (not `text`) so claude emits a single
        // `result` envelope carrying real token `usage` — without it the
        // non-streaming path had NO usage data and `/usage` always read zero for
        // it. The envelope is the same shape the streaming `result` line uses, so
        // the existing `extract_*` helpers parse it unchanged.
        let mut args = self.call_args_with_format("json");
        args.extend(model_args(&req.model));
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);
        let out = run_subprocess(SubprocessCall {
            program: &self.program,
            args: &args,
            prompt: &prompt,
            channel: PromptChannel::Arg,
            workspace: &ws,
            timeout: self.timeout,
            env: &[],
        })
        .await
        .map_err(crate::map_subprocess_error)?;

        // Parse the `result` envelope for the answer + usage. Fall back to raw
        // stdout if extraction yields nothing (an error envelope or an
        // unexpected format must never silently empty the run — fail-open).
        let usage = extract_usage(&out.stdout);
        let text = extract_result_text(&out.stdout).unwrap_or_else(|| {
            let assistant = extract_all_assistant_text(&out.stdout);
            if assistant.trim().is_empty() {
                out.stdout.clone()
            } else {
                assistant
            }
        });
        Ok(CompletionResponse {
            text,
            id: "claude-code-cli".to_string(),
            model: req.model,
            usage,
        })
    }

    /// Streaming completion via `claude --output-format stream-json --verbose`.
    ///
    /// Each newline-delimited JSON line is parsed in real time:
    /// - `{"type":"assistant","message":{"content":[{"type":"text","text":"…"}]}}`
    ///   → [`StreamEvent::Text`] with the delta.
    /// - `{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{...}}]}}`
    ///   → [`StreamEvent::ToolUse`] with the tool name + a human summary.
    /// - `{"type":"result","result":"…"}` → final assembled text.
    ///
    /// Non-JSON lines (rare stray output) are silently skipped. If parsing
    /// fails entirely, falls back to the non-streaming `complete` path so a
    /// format change never breaks the pipeline.
    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
    ) -> Result<CompletionResponse, RuntimeError> {
        let prompt = merge_prompt(&req);
        // Streaming args: same base + stream-json + verbose instead of text.
        let mut args = vec![
            self.print_flag.clone(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];
        if std::env::var("UMADEV_NO_SKIP_PERMS").as_deref() != Ok("1") {
            args.push("--dangerously-skip-permissions".to_string());
        }
        // Honor the selected model (`/model` / RunOptions.model) — `claude
        // --model <alias|full-id>`. Without this the host silently runs its own
        // default and the user's model choice is ignored.
        args.extend(model_args(&req.model));

        let model = req.model.clone();
        let timeout = self.timeout;
        let program = self.program.clone();
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);

        // Accumulate the raw stream so a mid-stream failure can salvage whatever
        // the base already produced instead of cold-restarting a whole new run.
        let stream_buf = std::sync::Mutex::new(String::new());
        let result = run_subprocess_streaming(
            SubprocessCall {
                program: &program,
                args: &args,
                prompt: &prompt,
                channel: PromptChannel::Arg,
                workspace: &ws,
                timeout,
                env: &[],
            },
            &|line: &str| {
                if let Ok(mut b) = stream_buf.lock() {
                    b.push_str(line);
                    b.push('\n');
                }
                if let Some(ev) = parse_claude_stream_line(line) {
                    on_event(ev);
                }
            },
        )
        .await;

        match result {
            Ok(out) => {
                // The streaming stdout is all JSON lines. Extract the final
                // result text from the `{"type":"result","result":"…"}` line.
                let mut final_text = extract_result_text(&out.stdout).unwrap_or_else(|| {
                    // Fallback: concatenate all assistant text blocks.
                    extract_all_assistant_text(&out.stdout)
                });
                // Detect a max-turns / execution abort BEFORE the fallback below
                // may move `out.stdout`, then surface it so it isn't silently
                // treated as a complete success (the result envelope was an
                // error message, so `final_text` is the real partial output).
                let abort = result_error(&out.stdout);
                // Capture real token usage from the result line (also before the
                // move below) so `/usage` shows true spend instead of zeros.
                let usage = extract_usage(&out.stdout);
                if final_text.trim().is_empty() && !out.stdout.trim().is_empty() {
                    final_text = out.stdout;
                }
                if let Some(msg) = abort {
                    on_event(umadev_runtime::StreamEvent::Warning { message: msg });
                }
                Ok(CompletionResponse {
                    text: final_text,
                    id: "claude-code-cli".to_string(),
                    model,
                    usage,
                })
            }
            Err(e) => {
                // Streaming broke mid-flight (commonly the base subprocess being
                // SIGTERM/SIGALRM'd — exit 143/142 — by its own environment).
                // This is routine self-healing, so it's `debug!`, not a scary
                // user-facing warning. First try to SALVAGE what already streamed
                // (it was shown to the user live) before paying for a full
                // cold-restart `complete` (worst case another whole timeout).
                tracing::debug!(error = %e, "streaming failed, falling back to non-streaming");
                let partial = stream_buf.into_inner().unwrap_or_default();
                if let Some(text) = salvage_partial_stream(&partial) {
                    let usage = extract_usage(&partial);
                    return Ok(CompletionResponse {
                        text,
                        id: "claude-code-cli".to_string(),
                        model,
                        usage,
                    });
                }
                drop(args);
                drop(prompt);
                self.complete(req).await
            }
        }
    }
}

/// Recover usable assistant text from a partial stream-json buffer captured
/// before a mid-stream failure. Returns `None` when nothing usable is present
/// (so the caller cold-restarts via `complete`), `Some(text)` when there is
/// real output worth keeping instead of re-running the whole turn.
fn salvage_partial_stream(stdout: &str) -> Option<String> {
    let text = extract_result_text(stdout).unwrap_or_else(|| extract_all_assistant_text(stdout));
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Parse one line of `claude --output-format stream-json` output into a
/// [`StreamEvent`]. Returns `None` for lines that aren't JSON or don't
/// carry displayable content (system init, rate-limit events, etc.).
fn parse_claude_stream_line(line: &str) -> Option<umadev_runtime::StreamEvent> {
    let line = line.trim();
    if line.is_empty() || !line.starts_with('{') {
        return None;
    }
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "assistant" => {
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())?;
            for block in content {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "thinking" => {
                        return Some(umadev_runtime::StreamEvent::Thinking);
                    }
                    "text" => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                return Some(umadev_runtime::StreamEvent::Text {
                                    delta: text.to_string(),
                                });
                            }
                        }
                    }
                    "tool_use" => {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        let detail = summarize_tool_input(name, block.get("input"));
                        return Some(umadev_runtime::StreamEvent::ToolUse {
                            name: name.to_string(),
                            detail,
                        });
                    }
                    _ => {}
                }
            }
            None
        }
        "user" => {
            // tool_result comes back as a "user" message.
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())?;
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    let ok = block
                        .get("is_error")
                        .and_then(serde_json::Value::as_bool)
                        .is_none_or(|e| !e);
                    let content_str = block.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let summary: String = content_str.chars().take(200).collect();
                    return Some(umadev_runtime::StreamEvent::ToolResult { ok, summary });
                }
            }
            None
        }
        _ => None, // "result", "system", "rate_limit_event" — no display needed
    }
}

/// Extract the final result text from a stream-json stdout.
/// Looks for the `{"type":"result","result":"…"}` line.
///
/// When the result line is an ERROR terminal (`is_error: true` or a `subtype`
/// like `error_max_turns` / `error_during_execution`), its `result` string is
/// an error message, NOT the answer — we return `None` so the caller falls back
/// to the real assistant text accumulated before the abort (and surfaces the
/// error separately via [`result_error`]). Otherwise a max-turns abort would
/// masquerade as a short successful reply.
fn extract_result_text(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                let is_error = v.get("is_error").and_then(serde_json::Value::as_bool) == Some(true);
                let subtype_error = v
                    .get("subtype")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s.starts_with("error"));
                if is_error || subtype_error {
                    return None;
                }
                if let Some(result) = v.get("result").and_then(|r| r.as_str()) {
                    return Some(result.to_string());
                }
            }
        }
    }
    None
}

/// Detect an ERROR terminal on the `result` line and return a human message.
/// `None` when the run ended cleanly (`subtype: "success"`). Used to surface a
/// [`umadev_runtime::StreamEvent::Warning`] so a max-turns / execution abort
/// is visible instead of silently truncating the phase output.
fn result_error(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                let is_error = v.get("is_error").and_then(serde_json::Value::as_bool) == Some(true);
                let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
                if is_error || subtype.starts_with("error") {
                    return Some(match subtype {
                        "error_max_turns" => {
                            "底座达到最大轮次上限,本阶段输出可能不完整".to_string()
                        }
                        "error_during_execution" => {
                            "底座执行中出错,本阶段输出可能不完整".to_string()
                        }
                        other if !other.is_empty() => {
                            format!("底座异常终止 ({other}),输出可能不完整")
                        }
                        _ => "底座异常终止,输出可能不完整".to_string(),
                    });
                }
            }
        }
    }
    None
}

/// Parse token usage from the stream-json `result` line.
///
/// The final `{"type":"result", "usage":{"input_tokens":…,"output_tokens":…},
/// "total_cost_usd":…,"num_turns":…}` line carries real usage. We surface the
/// headline input/output token counts (cache tokens folded into input) so
/// `/usage` reflects true spend instead of zeros. Returns [`Usage::default`]
/// (zeros) when no usable result line is present.
fn extract_usage(stdout: &str) -> Usage {
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                if let Some(u) = v.get("usage") {
                    let field = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
                    // Fold cache reads/writes into input — they ARE consumed input.
                    let input = field("input_tokens")
                        + field("cache_read_input_tokens")
                        + field("cache_creation_input_tokens");
                    let output = field("output_tokens");
                    return Usage {
                        input_tokens: u32::try_from(input).unwrap_or(u32::MAX),
                        output_tokens: u32::try_from(output).unwrap_or(u32::MAX),
                    };
                }
            }
        }
    }
    Usage::default()
}

/// Concatenate all assistant text blocks from stream-json lines.
/// Used as a fallback when no `result` line is found.
fn extract_all_assistant_text(stdout: &str) -> String {
    let mut texts = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                if let Some(content) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                texts.push(text.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    texts.join("\n")
}

/// Build a human-readable summary of a `tool_use` input (file path, command,
/// search query) so the TUI can show "[tool] Read src/app.tsx" instead of a
/// raw JSON blob.
fn summarize_tool_input(name: &str, input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    // Common tool patterns: Read/Write/Edit → `file_path`; Bash → command;
    // Grep/Glob → pattern; WebSearch → query.
    let get_str = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "Read" | "Write" | "Edit" | "NotebookEdit" => {
            let p = get_str("file_path");
            if p.is_empty() {
                get_str("path").to_string()
            } else {
                p.to_string()
            }
        }
        "Bash" => get_str("command").to_string(),
        "Grep" | "Glob" => get_str("pattern").to_string(),
        "WebSearch" | "WebFetch" => get_str("query").to_string(),
        "Task" | "Agent" => get_str("description").to_string(),
        _ => {
            // Generic: show first string value.
            input
                .as_object()
                .and_then(|o| o.values().find_map(|v| v.as_str()))
                .unwrap_or_default()
                .to_string()
        }
    }
}

#[async_trait]
impl HostDriver for ClaudeCodeDriver {
    fn backend_id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code CLI"
    }

    fn set_continue_session(&mut self, continue_session: bool) {
        self.continue_session = continue_session;
    }

    fn set_session_id(&mut self, session_id: Option<String>) {
        self.session_id = session_id;
    }

    fn set_workspace(&mut self, workspace: std::path::PathBuf) {
        self.workspace = Some(workspace);
    }

    async fn probe(&self) -> ProbeResult {
        let tmp = default_workspace();
        match run_subprocess(SubprocessCall {
            program: &self.program,
            args: &["--version".to_string()],
            prompt: "",
            channel: PromptChannel::Stdin,
            workspace: &tmp,
            timeout: Duration::from_secs(10),
            env: &[],
        })
        .await
        {
            Ok(out) => ProbeResult::Ready {
                version: out.stdout.lines().next().unwrap_or("unknown").to_string(),
            },
            Err(e) if e.contains("not found on PATH") => ProbeResult::NotInstalled {
                program: self.program.clone(),
            },
            Err(e) => ProbeResult::Unhealthy { detail: e },
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn fork_yields_a_concurrent_instance() {
        // A real logged-in base MUST fork so the pipeline's parallel fan-out
        // (architecture || UI/UX) triggers; only offline falls back to serial.
        use umadev_runtime::Runtime;
        let forked = ClaudeCodeDriver::default()
            .with_continue_session(true)
            .fork();
        assert!(forked.is_some(), "a real base must fork for parallel work");
    }
    use umadev_runtime::StreamEvent;

    #[test]
    fn salvage_partial_stream_recovers_text_or_none() {
        // A partial stream with an assistant text block → recoverable (so a
        // mid-stream failure reuses it instead of a full cold restart).
        let partial = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"half an answer"}]}}"#,
            "\n",
        );
        assert_eq!(
            salvage_partial_stream(partial).as_deref(),
            Some("half an answer")
        );
        // Nothing usable (no assistant text, just noise) → None → cold restart.
        assert_eq!(salvage_partial_stream(""), None);
        assert_eq!(
            salvage_partial_stream(r#"{"type":"system","subtype":"init"}"#),
            None
        );
    }

    #[test]
    fn extract_usage_reads_tokens_from_result_line() {
        let stdout = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":1200,"cache_read_input_tokens":300,"output_tokens":450},"total_cost_usd":0.02,"num_turns":4}"#,
        );
        let u = extract_usage(stdout);
        assert_eq!(u.input_tokens, 1500); // 1200 + 300 cache read
        assert_eq!(u.output_tokens, 450);
        // No result line → zeros (graceful).
        assert_eq!(extract_usage("plain text").input_tokens, 0);
    }

    #[test]
    fn result_error_detects_max_turns_and_extract_skips_envelope() {
        // A max-turns abort: the result line is an error envelope whose
        // `result` string is an error message, NOT the answer.
        let stdout = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial real answer"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"Reached max turns (50)"}"#,
        );
        // The error envelope's string is NOT returned as the answer…
        assert_eq!(extract_result_text(stdout), None);
        // …so the caller falls back to the real partial assistant text.
        assert_eq!(extract_all_assistant_text(stdout), "partial real answer");
        // …and the abort is surfaced.
        assert!(result_error(stdout).unwrap().contains("最大轮次"));

        // A clean success: result string IS the answer, no error surfaced.
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"the answer"}"#;
        assert_eq!(extract_result_text(ok).as_deref(), Some("the answer"));
        assert!(result_error(ok).is_none());
    }

    // ---- stream-json parsing (verified against real claude output) ----

    #[test]
    fn parse_skips_system_init_line() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/tmp","model":"claude-opus-4-8"}"#;
        assert!(parse_claude_stream_line(line).is_none());
    }

    #[test]
    fn parse_skips_rate_limit_line() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
        assert!(parse_claude_stream_line(line).is_none());
    }

    #[test]
    fn parse_emits_thinking_indicator() {
        // claude emits "thinking" content blocks — we emit a Thinking event
        // so the TUI can show a "[thinking] thinking..." indicator.
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."}]}}"#;
        let ev = parse_claude_stream_line(line).expect("thinking should emit Thinking event");
        assert!(matches!(ev, StreamEvent::Thinking));
    }

    #[test]
    fn parse_extracts_tool_use_read() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/Cargo.toml"}}]}}"#;
        let ev = parse_claude_stream_line(line).expect("should parse tool_use");
        match ev {
            StreamEvent::ToolUse { name, detail } => {
                assert_eq!(name, "Read");
                assert_eq!(detail, "/tmp/Cargo.toml");
            }
            _ => panic!("expected ToolUse, got {ev:?}"),
        }
    }

    #[test]
    fn parse_extracts_tool_use_bash() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"npm install"}}]}}"#;
        let ev = parse_claude_stream_line(line).expect("should parse");
        match ev {
            StreamEvent::ToolUse { name, detail } => {
                assert_eq!(name, "Bash");
                assert_eq!(detail, "npm install");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn parse_extracts_tool_result() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"version = 4.6.0\nedition = 2021"}]}}"#;
        let ev = parse_claude_stream_line(line).expect("should parse tool_result");
        match ev {
            StreamEvent::ToolResult { ok, summary } => {
                assert!(ok);
                assert!(summary.contains("4.6.0"));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn parse_extracts_tool_result_error() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"file not found"}]}}"#;
        let ev = parse_claude_stream_line(line).expect("should parse");
        match ev {
            StreamEvent::ToolResult { ok, summary } => {
                assert!(!ok, "is_error=true should give ok=false");
                assert!(summary.contains("file not found"));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn parse_extracts_text_delta() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"workspace version is 4.6.0"}]}}"#;
        let ev = parse_claude_stream_line(line).expect("should parse text");
        match ev {
            StreamEvent::Text { delta } => {
                assert!(delta.contains("4.6.0"));
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn extract_result_from_full_stream() {
        // Simulate a multi-line stream-json output (simplified from real capture).
        let stream = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"Cargo.toml"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"version = 4.6.0"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"The version is 4.6.0"}]}}
{"type":"result","subtype":"success","result":"The version is 4.6.0"}"#;
        let result = extract_result_text(stream).expect("should find result");
        assert_eq!(result, "The version is 4.6.0");
    }

    #[test]
    fn extract_assistant_text_fallback() {
        // When there's no "result" line, fall back to concatenating text blocks.
        let stream = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part 1"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"part 2"}]}}"#;
        let result = extract_all_assistant_text(stream);
        assert_eq!(result, "part 1\npart 2");
    }

    #[test]
    fn parse_skips_non_json_lines() {
        assert!(parse_claude_stream_line("").is_none());
        assert!(parse_claude_stream_line("   ").is_none());
        assert!(parse_claude_stream_line("not json at all").is_none());
        assert!(parse_claude_stream_line("{broken json").is_none());
    }

    #[test]
    fn summarize_tool_input_patterns() {
        let read_input = serde_json::json!({"file_path": "/tmp/app.tsx"});
        assert_eq!(
            summarize_tool_input("Read", Some(&read_input)),
            "/tmp/app.tsx"
        );

        let bash_input = serde_json::json!({"command": "npm test"});
        assert_eq!(summarize_tool_input("Bash", Some(&bash_input)), "npm test");

        let grep_input = serde_json::json!({"pattern": "TODO"});
        assert_eq!(summarize_tool_input("Grep", Some(&grep_input)), "TODO");

        assert_eq!(summarize_tool_input("Unknown", None), "");
    }

    #[test]
    fn defaults_are_sane() {
        let d = ClaudeCodeDriver::default();
        assert_eq!(d.backend_id(), "claude-code");
        assert_eq!(d.display_name(), "Claude Code CLI");
        assert_eq!(d.kind(), RuntimeKind::Anthropic);
        // base_args always starts with these stable flags; the permission
        // bypass flag is appended conditionally (tested below in the same
        // function to avoid env-var races between parallel tests).
        let args = d.base_args();
        assert_eq!(
            &args[..3],
            &[
                "--print".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
            ]
        );
        // By default (no UMADEV_NO_SKIP_PERMS) the bypass flag is present.
        // We check contains rather than exact equality because env state is
        // shared across parallel tests.
        assert!(
            args.contains(&"--dangerously-skip-permissions".to_string()),
            "base_args should include --dangerously-skip-permissions by default: {args:?}"
        );
    }

    #[tokio::test]
    async fn probe_reports_not_installed_for_missing_binary() {
        let d = ClaudeCodeDriver::with_program("umadev-fake-claude-xyz");
        let probe = d.probe().await;
        assert!(matches!(probe, ProbeResult::NotInstalled { .. }));
        assert!(!probe.is_ready());
    }

    #[test]
    fn continue_session_appends_resume_flag() {
        let fresh = ClaudeCodeDriver::default();
        assert!(
            !fresh.call_args().contains(&"--continue".to_string()),
            "a fresh session with no pinned id must NOT resume"
        );

        let mut resumed = ClaudeCodeDriver::default();
        resumed.set_continue_session(true);
        assert!(
            resumed.call_args().contains(&"--continue".to_string()),
            "a continued session (no pinned id) must pass --continue"
        );
        // The builder form mirrors the setter.
        assert!(ClaudeCodeDriver::default()
            .with_continue_session(true)
            .call_args()
            .contains(&"--continue".to_string()));
    }

    #[test]
    fn explicit_session_id_pins_the_conversation() {
        let uuid = "11111111-2222-4333-8444-555555555555".to_string();

        // Turn 1 (fresh) creates the session with OUR id.
        let create = ClaudeCodeDriver::default().with_session_id(Some(uuid.clone()));
        let args = create.call_args();
        assert!(args
            .windows(2)
            .any(|w| w == ["--session-id", uuid.as_str()]));
        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--continue".to_string()));

        // Turn 2+ resumes that exact id — never "the most recent in this dir",
        // so we can't accidentally continue the user's other conversation.
        let resume = ClaudeCodeDriver::default()
            .with_session_id(Some(uuid.clone()))
            .with_continue_session(true);
        let args = resume.call_args();
        assert!(args.windows(2).any(|w| w == ["--resume", uuid.as_str()]));
        assert!(!args.contains(&"--continue".to_string()));
        assert!(!args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn call_args_json_format_requests_usage_envelope() {
        // The non-streaming complete() asks for `--output-format json` so it can
        // read real token usage off the single result envelope; the default
        // text path is unchanged.
        let d = ClaudeCodeDriver::default();
        let json_args = d.call_args_with_format("json");
        assert!(
            json_args
                .windows(2)
                .any(|w| w == ["--output-format", "json"]),
            "json format must pass --output-format json: {json_args:?}"
        );
        let text_args = d.call_args();
        assert!(
            text_args
                .windows(2)
                .any(|w| w == ["--output-format", "text"]),
            "default call_args stays text: {text_args:?}"
        );
        // Session handling is identical across formats — a pinned resume id
        // still appears regardless of output format.
        let id = "11111111-2222-4333-8444-555555555555".to_string();
        let resume = ClaudeCodeDriver::default()
            .with_session_id(Some(id.clone()))
            .with_continue_session(true);
        assert!(resume
            .call_args_with_format("json")
            .windows(2)
            .any(|w| w == ["--resume", id.as_str()]));
    }

    // The fake claude is a `#!/bin/sh` script, which Windows cannot exec; the
    // JSON-envelope parsing it exercises is also covered by `extract_usage` /
    // `extract_result_text` unit tests above.
    #[cfg(unix)]
    #[tokio::test]
    async fn complete_parses_usage_from_json_result_envelope() {
        // Drive a fake `claude` that emits the real `--output-format json`
        // single-object envelope (captured from live claude). complete() must
        // extract BOTH the answer text and the real token usage, instead of the
        // old hard-coded zeros.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-claude");
        std::fs::write(
            &script,
            // Drain stdin (the Arg-channel path closes the write half, so this is
            // an immediate EOF) so the fake mirrors the codex fake's structure,
            // which runs cleanly under dash on Linux CI where the no-drain form flaked.
            "#!/bin/sh\ncat >/dev/null 2>&1\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"the real answer\",\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":300,\"cache_creation_input_tokens\":50,\"output_tokens\":42}}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let d = ClaudeCodeDriver::with_program(script.to_str().unwrap());
        let req = CompletionRequest {
            model: "claude-opus-4-8".into(),
            system: None,
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "x".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let resp = d.complete(req).await.unwrap();
        assert_eq!(resp.text, "the real answer");
        // input = 1200 + 300 cache read + 50 cache creation.
        assert_eq!(resp.usage.input_tokens, 1550);
        assert_eq!(resp.usage.output_tokens, 42);
    }

    #[tokio::test]
    async fn complete_drives_a_fake_claude_binary() {
        // Use `echo` as a stand-in: it ignores --print and echoes the
        // remaining args, proving the driver passes the merged prompt
        // as a positional argument.
        let d = ClaudeCodeDriver::with_program("echo");
        let req = CompletionRequest {
            model: "claude-sonnet-4-6".into(),
            system: Some("be terse".into()),
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "ping".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let resp = d.complete(req).await.unwrap();
        // echo prints "--print <prompt>"; the driver's clean_output trims it.
        assert!(resp.text.contains("be terse"));
        assert!(resp.text.contains("ping"));
        assert_eq!(resp.model, "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn complete_claude_response_contract_is_stable() {
        // Pin the claude bespoke driver's complete() contract: response.id is
        // "claude-code-cli", the model echoes the request model, and stdout
        // (via echo) lands in text. This is the claude-code subprocess
        // integration test (paired with codex's complete_drives_a_fake_codex_binary
        // (Claude Code + Codex are both bespoke drivers.)
        let d = ClaudeCodeDriver::with_program("echo");
        let req = CompletionRequest {
            model: "claude-opus-4-7".into(),
            system: None,
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "contract-probe".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let resp = d.complete(req).await.unwrap();
        assert_eq!(resp.id, "claude-code-cli");
        assert_eq!(resp.model, "claude-opus-4-7");
        assert!(resp.text.contains("contract-probe"));
    }

    #[tokio::test]
    async fn complete_surfaces_host_process_error() {
        let d = ClaudeCodeDriver::with_program("umadev-fake-claude-xyz");
        let req = CompletionRequest {
            model: "m".into(),
            system: None,
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "x".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let err = d.complete(req).await.unwrap_err();
        assert!(matches!(err, RuntimeError::HostProcess(_)));
    }
}
