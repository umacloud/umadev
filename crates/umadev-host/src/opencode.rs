//! `OpenCodeDriver` — drives the `opencode` CLI in non-interactive run mode.
//!
//! Shells out to:
//!
//! ```text
//! opencode run "<prompt>"
//! ```
//!
//! `OpenCode` owns provider authentication/configuration through
//! `opencode auth login` and its own config files. UmaDev treats it as a
//! first-class host base, just like Claude Code and Codex: we pass the prompt
//! to the already-configured CLI and capture the answer.
//!
//! Official CLI docs (and the live `opencode run --help` on the dev machine):
//! `opencode run [message..]` is the documented non-interactive form;
//! `--model provider/model` is accepted when the model id is already in
//! `OpenCode`'s provider/model shape; `-c/--continue` resumes the *most recent*
//! session in this directory; and `-s/--session <id>` resumes a *specific*
//! session id deterministically. When UmaDev has pinned a session id it uses
//! `-s <id>` (never colliding with the user's other `opencode` conversations in
//! the same dir). Fresh calls use `--format json`; the authoritative `sessionID`
//! repeated on each raw event is captured for the next turn. `--continue` is
//! used only when an older caller requested continuation before an id was known.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use semver::Version;
use tokio::sync::OnceCell;
use umadev_runtime::{
    BasePermissionProfile, CompletionRequest, CompletionResponse, Runtime, RuntimeError,
    RuntimeKind, Usage,
};

use crate::{
    default_workspace, merge_prompt, run_auth_status, run_subprocess, run_subprocess_streaming,
    AuthState, HostDriver, ProbeResult, PromptChannel, SubprocessCall,
};

/// Parse one exact semver token from OpenCode's `--version` output. Labels such
/// as `OpenCode CLI` and a leading `v` are accepted, as are semver prerelease and
/// build suffixes. We deliberately do not scrape an arbitrary digit substring:
/// an unrecognised format is unsafe to classify and must remain unknown.
fn parse_reported_opencode_version(raw: &str) -> Option<Version> {
    raw.lines()
        .flat_map(str::split_whitespace)
        .find_map(|token| {
            let token = token
                .trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'));
            let token = token
                .strip_prefix('v')
                .or_else(|| token.strip_prefix('V'))
                .unwrap_or(token);
            Version::parse(token).ok()
        })
}

fn version_output_excerpt(raw: &str) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        "<empty>".to_string()
    } else {
        one_line.chars().take(160).collect()
    }
}

/// Resolve the installed `opencode` binary by reading its `--version`, WITHOUT
/// enforcing a version floor or allowlist. This only proves the base is installed
/// and answering, so every permission posture runs ANY installed version. `Ok`
/// carries the raw `--version` output for diagnostics; `Err` distinguishes an absent binary
/// (`... not found on PATH`, surfaced as `NotInstalled`) from a `--version`
/// that genuinely failed (surfaced as `Unhealthy`).
pub(crate) async fn probe_opencode_version(
    program: &str,
    workspace: &Path,
) -> Result<String, String> {
    run_subprocess(SubprocessCall {
        program,
        args: &["--version".to_string()],
        prompt: "",
        channel: PromptChannel::Stdin,
        workspace,
        timeout: Duration::from_secs(10),
        env: &[],
    })
    .await
    .map(|out| out.stdout)
}

/// The version string the picker/doctor shows for a resolved binary: the exact
/// semver when the `--version` output is recognizable, otherwise a short,
/// single-line excerpt of what the CLI reported. An unfamiliar format is still
/// "installed" — never a refusal at discovery time.
fn reported_opencode_version(raw: &str) -> String {
    parse_reported_opencode_version(raw).map_or_else(
        || version_output_excerpt(raw),
        |version| version.to_string(),
    )
}

/// Resolve the installed `opencode` program before a run.
///
/// This checks availability only; all reported versions run. Plan safety comes
/// from the deny-by-default session ruleset (including delegation) plus the
/// read-only fork responder, rather than from assuming a semantic version proves
/// runtime behavior.
pub(crate) async fn ensure_opencode_available(
    program: &str,
    workspace: &Path,
) -> Result<(), String> {
    probe_opencode_version(program, workspace).await.map(|_| ())
}

/// Drives the `opencode` CLI as a subprocess.
#[derive(Debug, Clone)]
pub struct OpenCodeDriver {
    program: String,
    timeout: Duration,
    /// One `--version` probe per driver (shared by its concurrent forks),
    /// caching whether the binary is installed and answering. The returned
    /// version is diagnostic/display data only; it never authorizes execution.
    version_probe: Arc<OnceCell<Result<String, String>>>,
    /// Permission posture for this legacy one-shot driver. Defaults to Plan.
    permissions: BasePermissionProfile,
    /// When `true`, the next `complete` resumes a prior `opencode` session so
    /// the base keeps its own memory — deterministically via `-s <id>` when a
    /// [`Self::session_id`] is pinned, otherwise `--continue` (most recent).
    continue_session: bool,
    /// An explicit `opencode` session id to resume. When set AND
    /// [`Self::continue_session`] is true, the call uses `-s <id>` so UmaDev
    /// resumes *its own* session deterministically instead of grabbing
    /// "the most recent in this dir" (which could be the user's other
    /// conversation). When `None`, falls back to `--continue`.
    session_id: Arc<RwLock<Option<String>>>,
    /// The cwd the `opencode` subprocess runs in (the pipeline project root).
    workspace: Option<std::path::PathBuf>,
}

impl Default for OpenCodeDriver {
    fn default() -> Self {
        Self {
            program: std::env::var("UMADEV_OPENCODE_BIN")
                .unwrap_or_else(|_| "opencode".to_string()),
            timeout: crate::worker_timeout_from_env(),
            version_probe: Arc::new(OnceCell::new()),
            permissions: BasePermissionProfile::Plan,
            continue_session: false,
            session_id: Arc::new(RwLock::new(None)),
            workspace: None,
        }
    }
}

impl OpenCodeDriver {
    /// Build a driver with an explicit program name (mainly for tests).
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

    /// Resolve — once per driver — whether the binary is installed and answering
    /// `--version`. Version-NUMBER-agnostic: permission posture is applied by the
    /// runtime rules, never inferred from the version response.
    async fn require_resolved_version(&self, workspace: &Path) -> Result<String, String> {
        let program = self.program.clone();
        let workspace = workspace.to_path_buf();
        self.version_probe
            .get_or_init(|| async move { probe_opencode_version(&program, &workspace).await })
            .await
            .clone()
    }

    /// Confirm the program is installed, reusing the memoized probe. The version
    /// string is display/diagnostic data only for every permission posture.
    async fn ensure_program_available(&self, workspace: &Path) -> Result<(), String> {
        self.require_resolved_version(workspace).await.map(|_| ())
    }

    /// Inject a `--version` result into the memoized probe so tests exercise the
    /// version-agnostic execution path without a real binary.
    #[cfg(test)]
    fn with_version_output_for_test(mut self, raw: &str) -> Self {
        let cell: OnceCell<Result<String, String>> = OnceCell::new();
        assert!(
            cell.set(Ok(raw.to_string())).is_ok(),
            "fresh version cell must be empty"
        );
        self.version_probe = Arc::new(cell);
        self
    }

    /// Select the access/approval posture for this one-shot driver.
    #[must_use]
    pub fn with_permissions(mut self, permissions: BasePermissionProfile) -> Self {
        self.permissions = permissions;
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
        // Replace the cell instead of mutating a shared clone. In particular,
        // `fork()` must reset only the child and never erase its parent's live
        // session id.
        self.session_id = Arc::new(RwLock::new(session_id));
        self
    }

    fn pinned_session_id(&self) -> Option<String> {
        self.session_id
            .read()
            .ok()
            .and_then(|session_id| session_id.clone())
    }

    fn remember_session_id(&self, session_id: &str) {
        if !valid_opencode_session_id(session_id) {
            return;
        }
        if let Ok(mut slot) = self.session_id.write() {
            *slot = Some(session_id.to_string());
        }
    }

    /// The argument vector preceding the prompt. Exposed for tests.
    #[must_use]
    pub fn base_args(&self, model: &str) -> Vec<String> {
        self.base_args_for(
            model,
            std::env::var("UMADEV_NO_SKIP_PERMS").as_deref() == Ok("1"),
        )
    }

    fn base_args_for(&self, model: &str, no_skip: bool) -> Vec<String> {
        let mut args = vec!["run".to_string()];
        args.push("--agent".to_string());
        // Plan → read-only `plan` agent. Guarded ALSO uses `plan` on this ONE-SHOT
        // path: `opencode run` is non-interactive with no permission.asked round-trip
        // back to UmaDev, so a `build` agent here would collapse to opencode's own
        // allow/deny default and BYPASS Guarded's ask-before-write contract. The
        // writable Guarded work runs through OpenCodeSession (serve), which enforces
        // the ask-by-default ruleset with a real host-mediated approval channel. Only
        // Auto — explicit full autonomy, paired with `--auto` below — uses the writable
        // `build` agent here. (Same "can't ask → don't write" fail-closed principle as
        // the Kimi mode boundary.)
        args.push(
            if self.permissions.auto_approve() && !no_skip {
                "build"
            } else {
                "plan"
            }
            .to_string(),
        );
        // OpenCode model ids are provider/model. UmaDev's default launch
        // model (`claude-sonnet-4-6`) is not in that shape, so only pass a
        // model when the user explicitly selected an OpenCode-compatible id.
        if model.contains('/') {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
        // `--auto` is OpenCode's documented non-interactive auto-approval flag.
        // The environment latch only tightens Auto; Plan and Guarded never add it.
        if self.permissions.auto_approve() && !no_skip {
            args.push("--auto".to_string());
        }
        args
    }

    /// The full argument vector for a `complete` call, resolving the resume
    /// strategy. Exposed for tests. The prompt is appended by the subprocess
    /// layer as the last positional argument.
    ///
    /// - pinned id + resume → `-s <id>`     (resume OUR session deterministically)
    /// - no id + resume     → `--continue`  (most recent session in this dir)
    /// - fresh              → (nothing)     (brand-new session)
    ///
    /// Both `-s/--session <id>` and `-c/--continue` are confirmed against the
    /// live `opencode run --help`. Every call also uses the documented
    /// `--format json` raw-event stream. Each event repeats the authoritative
    /// `sessionID`; a fresh call captures it into this driver so its next turn
    /// can resume with `--session <id>` rather than accidentally selecting the
    /// user's most recent unrelated OpenCode conversation.
    #[must_use]
    pub fn call_args(&self, model: &str) -> Vec<String> {
        let mut args = self.base_args(model);
        if self.continue_session {
            match self.pinned_session_id() {
                Some(id) => {
                    // Resume OUR specific session — never "the most recent in
                    // this dir", so we can't continue the user's other chat.
                    args.push("--session".to_string());
                    args.push(id);
                }
                None => {
                    // `--continue` resumes the last session so `opencode` answers
                    // with its own prior context instead of starting cold.
                    args.push("--continue".to_string());
                }
            }
        }
        args.push("--format".to_string());
        args.push("json".to_string());
        args
    }
}

#[async_trait]
impl Runtime for OpenCodeDriver {
    /// Concurrent-safe fork: clone with a FRESH session (no resume, no pinned
    /// id) so parallel pipeline steps don't collide on one opencode session.
    fn fork(&self) -> Option<Box<dyn Runtime>> {
        Some(Box::new(
            self.clone()
                .with_continue_session(false)
                .with_session_id(None),
        ))
    }

    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Openai
    }

    fn capabilities(&self) -> umadev_runtime::BrainCapabilities {
        // `persistent_goal: true` means UmaDev FORWARDS a `/goal` directive to the
        // base when the user runs `/goal` (the intended interaction). OpenCode has no
        // native `/goal` slash command of its own (its CLI is `opencode run`; its
        // slash set is /editor /export /help /models /new /sessions /status /themes
        // /timeline /worktree), so it reads the directive as a strong "keep working
        // until done" instruction — only Claude Code has a native `/goal` mode. It
        // also has no usage on stdout and no PreToolUse hook.
        umadev_runtime::BrainCapabilities {
            persistent_goal: true,
            ..umadev_runtime::BrainCapabilities::default()
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, RuntimeError> {
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);
        self.ensure_program_available(&ws)
            .await
            .map_err(crate::map_subprocess_error)?;
        let prompt = merge_prompt(&req);
        let args = self.call_args(&req.model);
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

        if let Some(session_id) = extract_opencode_session_id(&out.stdout) {
            self.remember_session_id(&session_id);
        }
        // Robust to a structured stream with a differing text shape (would return
        // empty) AND an older/custom binary whose JSON lines lack the strict envelope
        // (would leak raw event JSON); a genuinely plain-text reply still passes
        // through verbatim.
        let text = resolve_opencode_answer(&out.stdout);

        Ok(crate::redaction::sanitize_completion_response(
            &CompletionResponse {
                text,
                id: "opencode-cli".to_string(),
                model: req.model,
                usage: Usage::default(),
            },
        ))
    }

    /// Streaming completion via `opencode run`, forwarding stdout **line by
    /// line** so the TUI shows live progress instead of a frozen spinner.
    ///
    /// `opencode run --format json` emits newline-delimited raw events with an
    /// authoritative `sessionID`, completed text parts, tool parts, and step
    /// lifecycle records. UmaDev parses those records into the same typed live
    /// events used by the other native bases and captures the assigned session
    /// id for deterministic resume. A custom/older binary that prints plain text
    /// still degrades to the conservative line forwarder.
    /// Timeout / empty-reply / error semantics are inherited unchanged from the
    /// shared subprocess layer and the `complete` fallback.
    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
    ) -> Result<CompletionResponse, RuntimeError> {
        let call_started = std::time::Instant::now();
        let prompt = merge_prompt(&req);
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);
        self.ensure_program_available(&ws)
            .await
            .map_err(crate::map_subprocess_error)?;
        let args = self.call_args(&req.model);
        let model = req.model.clone();
        let program = self.program.clone();
        let timeout = self.timeout;

        // Accumulate the raw stream so a mid-stream failure can salvage whatever
        // already arrived (opencode's answer IS its plain stdout) instead of
        // cold-restarting a whole new run.
        let stream_buf = crate::StreamingFallbackBuffer::new();
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
                stream_buf.push_line(line);
                if let Some(ev) = parse_opencode_stream_line(line) {
                    on_event(ev);
                }
                if let Some(session_id) = opencode_session_id_from_line(line) {
                    self.remember_session_id(&session_id);
                }
            },
        )
        .await;

        match result {
            Ok(out) => {
                if let Some(session_id) = extract_opencode_session_id(&out.stdout) {
                    self.remember_session_id(&session_id);
                }
                let text = resolve_opencode_answer(&out.stdout);
                Ok(crate::redaction::sanitize_completion_response(
                    &CompletionResponse {
                        text,
                        id: "opencode-cli".to_string(),
                        model,
                        usage: Usage::default(),
                    },
                ))
            }
            Err(e) => {
                // Fail-open: drop to the non-streaming path so a streaming-only
                // failure (no line-buffered stdout, format drift, or the base
                // being SIGTERM/SIGALRM'd — exit 143/142) never empties the
                // phase. Routine self-healing → `debug!`, not a scary warning.
                // Salvage what already streamed (opencode's text IS its stdout)
                // before paying for a full `complete` re-run.
                tracing::debug!(error = %e, "opencode streaming failed, falling back to non-streaming");
                let partial = stream_buf.into_string();
                let salvaged = resolve_opencode_answer(&partial);
                if !salvaged.trim().is_empty() {
                    return Ok(crate::redaction::sanitize_completion_response(
                        &CompletionResponse {
                            text: salvaged,
                            id: "opencode-cli".to_string(),
                            model,
                            usage: Usage::default(),
                        },
                    ));
                }
                let stream_error = crate::map_subprocess_error(&e);
                if matches!(stream_error, RuntimeError::Timeout(_, _)) {
                    return Err(stream_error);
                }
                let remaining = timeout.saturating_sub(call_started.elapsed());
                if remaining.is_zero() {
                    return Err(stream_error);
                }
                drop(args);
                drop(prompt);
                self.clone().with_timeout(remaining).complete(req).await
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OpenCodeExtraction {
    text: String,
    saw_structured_event: bool,
}

fn valid_opencode_session_id(session_id: &str) -> bool {
    (4..=160).contains(&session_id.len())
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn opencode_event(line: &str) -> Option<serde_json::Value> {
    let value = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    let kind = value.get("type").and_then(serde_json::Value::as_str)?;
    if !matches!(
        kind,
        "tool_use" | "step_start" | "step_finish" | "text" | "reasoning" | "error"
    ) {
        return None;
    }
    let session_id = value.get("sessionID").and_then(serde_json::Value::as_str)?;
    if !valid_opencode_session_id(session_id)
        || !value
            .get("timestamp")
            .is_some_and(serde_json::Value::is_number)
    {
        return None;
    }
    Some(value)
}

fn opencode_session_id_from_line(line: &str) -> Option<String> {
    opencode_event(line)?
        .get("sessionID")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn extract_opencode_session_id(stdout: &str) -> Option<String> {
    stdout.lines().find_map(opencode_session_id_from_line)
}

fn extract_opencode_output(stdout: &str) -> OpenCodeExtraction {
    let mut output = OpenCodeExtraction::default();
    for line in stdout.lines() {
        let Some(event) = opencode_event(line) else {
            continue;
        };
        output.saw_structured_event = true;
        if event.get("type").and_then(serde_json::Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = event
            .get("part")
            .filter(|part| part.get("type").and_then(serde_json::Value::as_str) == Some("text"))
            .and_then(|part| part.get("text"))
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        if !output.text.is_empty() && !output.text.ends_with('\n') {
            output.text.push('\n');
        }
        output.text.push_str(text);
    }
    output
}

/// Best-effort text salvage for output the STRICT structured parse could not use: an
/// older/custom opencode whose JSON lines omit the `timestamp`/`sessionID` envelope
/// (so [`opencode_event`] rejects every line), OR a structured stream whose text part
/// sits in a slightly different shape. Pulls the assistant words from any JSON line
/// that looks like a `text` event (`part.text`, else a top-level `text`). Returns
/// `None` when the input has no JSON lines or none carry text, so a genuinely
/// plain-text reply is left to pass through verbatim rather than being mangled.
fn salvage_opencode_text(stdout: &str) -> Option<String> {
    let mut out = String::new();
    let mut saw_json_line = false;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        saw_json_line = true;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("text") {
            continue;
        }
        let text = value
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("text").and_then(serde_json::Value::as_str))
            .filter(|text| !text.is_empty());
        if let Some(text) = text {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    (saw_json_line && !out.trim().is_empty()).then_some(out)
}

/// Resolve the final answer text from an `opencode run` stdout, robust to BOTH an
/// older/custom binary whose JSON lines lack the strict envelope (which would leak
/// raw event JSON to the user) AND a structured stream whose text shape differs
/// (which would return an empty answer). Order: the strict structured text when
/// present; else a best-effort salvage from any JSON lines; else the verbatim stdout
/// for a genuinely plain-text reply.
fn resolve_opencode_answer(stdout: &str) -> String {
    let extraction = extract_opencode_output(stdout);
    if extraction.saw_structured_event && !extraction.text.trim().is_empty() {
        return extraction.text;
    }
    salvage_opencode_text(stdout).unwrap_or_else(|| stdout.to_string())
}

fn opencode_tool_detail(part: &serde_json::Value) -> String {
    let state = part.get("state").unwrap_or(&serde_json::Value::Null);
    if let Some(title) = state
        .get("title")
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.trim().is_empty())
    {
        return title.chars().take(160).collect();
    }
    let input = state.get("input").unwrap_or(&serde_json::Value::Null);
    for key in [
        "filePath",
        "file_path",
        "path",
        "command",
        "pattern",
        "query",
        "url",
    ] {
        if let Some(value) = input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            return value.chars().take(160).collect();
        }
    }
    String::new()
}

/// Turn one line of `opencode run --format json` stdout into a typed live
/// event. The current official schema is parsed strictly enough that arbitrary
/// JSON in an assistant answer cannot impersonate a host event: an allow-listed
/// type, numeric timestamp, valid session id, and event-specific part shape are
/// all required. Plain text remains a compatibility fallback for custom builds.
fn parse_opencode_stream_line(line: &str) -> Option<umadev_runtime::StreamEvent> {
    parse_opencode_stream_line_raw(line).map(crate::redaction::sanitize_stream_event)
}

fn parse_opencode_stream_line_raw(line: &str) -> Option<umadev_runtime::StreamEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(event) = opencode_event(trimmed) {
        let kind = event.get("type").and_then(serde_json::Value::as_str)?;
        let part = event.get("part").unwrap_or(&serde_json::Value::Null);
        return match kind {
            "text" if part.get("type").and_then(serde_json::Value::as_str) == Some("text") => part
                .get("text")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| umadev_runtime::StreamEvent::Text {
                    delta: text.to_string(),
                }),
            "tool_use" if part.get("type").and_then(serde_json::Value::as_str) == Some("tool") => {
                let name = part
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .filter(|name| !name.trim().is_empty())?;
                Some(umadev_runtime::StreamEvent::tool_use(
                    name,
                    opencode_tool_detail(part),
                ))
            }
            "reasoning"
                if part.get("type").and_then(serde_json::Value::as_str) == Some("reasoning") =>
            {
                part.get("text")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| umadev_runtime::StreamEvent::ThinkingDelta(text.to_string()))
            }
            "error" => {
                let error = event.get("error").unwrap_or(&serde_json::Value::Null);
                let message = error
                    .get("data")
                    .and_then(|data| data.get("message"))
                    .or_else(|| error.get("message"))
                    .or_else(|| error.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("OpenCode reported an error")
                    .chars()
                    .take(240)
                    .collect();
                Some(umadev_runtime::StreamEvent::Warning { message })
            }
            // Step lifecycle is useful for completion accounting but carries no
            // user-facing delta on its own; unknown future records are likewise
            // ignored rather than guessed.
            _ => None,
        };
    }
    // Best-effort tool-step recognition. `opencode` decorates tool steps with a
    // leading box-drawing/pipe gutter in some terminals (e.g. "|  Read  src/x"
    // or "│ Bash npm test"); when we can confidently pull a known tool name out
    // of such a gutter line, surface it as ToolUse. Otherwise it's just text.
    if let Some(ev) = recognize_tool_step(trimmed) {
        return Some(ev);
    }
    // Plain text: restore the trailing newline so consecutive lines render as
    // separate lines in the typewriter view instead of being glued together.
    Some(umadev_runtime::StreamEvent::Text {
        delta: format!("{line}\n"),
    })
}

/// Recognise a known tool name in a gutter-decorated `opencode` step line,
/// returning a [`umadev_runtime::StreamEvent::ToolUse`] or `None`. Conservative: only fires when
/// the line starts with a box-drawing/pipe gutter AND the first token after it
/// is a known tool id — so ordinary prose that merely contains the word "Read"
/// is never mis-tagged.
fn recognize_tool_step(trimmed: &str) -> Option<umadev_runtime::StreamEvent> {
    // Strip a leading gutter of pipe / box-drawing chars + spaces.
    let after_gutter = trimmed.trim_start_matches(|c: char| {
        c == '|' || c == '│' || c == '├' || c == '└' || c == '─' || c == '*' || c.is_whitespace()
    });
    if std::ptr::eq(after_gutter, trimmed) {
        // No gutter was stripped → this is ordinary text, not a tool step.
        return None;
    }
    let mut parts = after_gutter.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let name = match head {
        "Read" => "Read",
        "Write" => "Write",
        "Edit" => "Edit",
        "Bash" | "Shell" | "Run" => "Bash",
        "Grep" | "Search" | "Glob" => "Grep",
        "Web" | "WebFetch" | "WebSearch" | "Fetch" => "WebFetch",
        _ => return None,
    };
    let detail: String = parts.next().unwrap_or("").trim().chars().take(80).collect();
    // opencode's step is scraped from a decorated gutter line — we only ever
    // recover the tool name + a short detail, never the file's before/after
    // content — so `edit` stays `None` and the TUI shows the plain tool row.
    Some(umadev_runtime::StreamEvent::tool_use(name, detail))
}

#[async_trait]
impl HostDriver for OpenCodeDriver {
    fn backend_id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode CLI"
    }

    fn permission_profile(&self) -> BasePermissionProfile {
        self.permissions
    }

    fn set_continue_session(&mut self, continue_session: bool) {
        self.continue_session = continue_session;
    }

    fn set_session_id(&mut self, session_id: Option<String>) {
        self.session_id = Arc::new(RwLock::new(session_id));
    }

    fn set_workspace(&mut self, workspace: std::path::PathBuf) {
        self.workspace = Some(workspace);
    }

    fn install_hint(&self) -> Option<&'static str> {
        Some("npm install -g opencode-ai")
    }

    fn login_hint(&self) -> Option<&'static str> {
        Some("opencode auth login")
    }

    async fn probe(&self) -> ProbeResult {
        let tmp = default_workspace();
        match self.require_resolved_version(&tmp).await {
            // Installed AND answering `--version` → Ready regardless of the
            // version NUMBER: an old build is installed, not "refused". The
            // read-only Plan permission rules are applied at run time, never at discovery,
            // so the picker treats OpenCode like every other base and can still
            // show the detected version. Resolve the honest auth state too
            // (gap G10).
            Ok(raw) => ProbeResult::Ready {
                version: reported_opencode_version(&raw),
                auth_state: self.probe_auth().await,
            },
            Err(e) if e.contains("not found on PATH") => ProbeResult::NotInstalled {
                program: self.program.clone(),
            },
            Err(e) => ProbeResult::Unhealthy { detail: e },
        }
    }

    /// Cheapest authenticated no-op for `OpenCode`, in cost order — NO real
    /// generation, NO tokens.
    ///
    /// `OpenCode` stores provider credentials in `auth.json` under its platform
    /// data dir (`$XDG_DATA_HOME/opencode/auth.json`, default
    /// `~/.local/share/opencode/auth.json` on Unix; `%LOCALAPPDATA%\opencode\…`
    /// on Windows). The file is written by `opencode auth login` and holds one
    /// entry per configured provider (verified live: `{"anthropic": {…}}`).
    ///
    /// 1. **`auth.json` exists AND is non-empty** (`{}` means "no providers")
    ///    under the data dir → at least one provider is configured → LoggedIn.
    /// 2. **Authoritative subcommand** `opencode auth list` — lists configured
    ///    providers/credentials; output mentioning a credential count / provider
    ///    means logged in, an explicit "no credentials" means not. Fallback when
    ///    the file can't be read (custom dir); bounded by the short timeout.
    ///
    /// Unlike claude/codex there is NO single env var that authenticates
    /// `OpenCode` globally (per-provider keys vary), so we don't guess from env.
    /// Fail-open: anything indeterminate → [`AuthState::Unknown`].
    async fn probe_auth(&self) -> AuthState {
        // 1. Credential file: present AND carrying at least one provider.
        if let Some(state) = opencode_auth_file().and_then(|p| classify_opencode_auth_file(&p)) {
            return state;
        }
        // 2. Authoritative subcommand: `opencode auth list`.
        match run_auth_status(
            &self.program,
            &["auth".to_string(), "list".to_string()],
            true,
        )
        .await
        {
            Some(out) => classify_opencode_auth_list(&out),
            None => AuthState::Unknown,
        }
    }
}

/// `OpenCode`'s credential file: `<data_dir>/opencode/auth.json`. Returns `None`
/// when no data dir can be derived (fail-open → the caller tries the subcommand).
fn opencode_auth_file() -> Option<std::path::PathBuf> {
    crate::data_dir().map(|d| d.join("opencode").join("auth.json"))
}

/// Classify `OpenCode`'s `auth.json`: present + a non-empty JSON object (at least
/// one configured provider) → [`AuthState::LoggedIn`]; present but `{}`/empty →
/// [`AuthState::NotLoggedIn`]; unreadable/absent → `None` (fall through to the
/// subcommand). Fail-open: a parse failure on a present file yields `None`, not a
/// guess.
fn classify_opencode_auth_file(path: &std::path::Path) -> Option<AuthState> {
    const MAX_AUTH_BYTES: u64 = 1024 * 1024;
    let body = crate::read_user_file_bounded(path, MAX_AUTH_BYTES).ok()?;
    let v = serde_json::from_slice::<serde_json::Value>(&body).ok()?;
    let obj = v.as_object()?;
    Some(if obj.is_empty() {
        AuthState::NotLoggedIn
    } else {
        AuthState::LoggedIn
    })
}

/// Classify the output of `opencode auth list`. Output naming a provider / a
/// non-zero credential count → [`AuthState::LoggedIn`]; an explicit "0
/// credentials" / "no credentials" → [`AuthState::NotLoggedIn`]; anything else →
/// [`AuthState::Unknown`] (fail-open — never a false positive).
fn classify_opencode_auth_list(out: &str) -> AuthState {
    let lower = out.to_ascii_lowercase();
    if lower.contains("0 credentials") || lower.contains("no credentials") {
        AuthState::NotLoggedIn
    } else if lower.contains("credential") {
        // "N credentials" / "1 credential" with N>=1 (the 0 case is handled
        // above), or a "Credentials" header followed by listed providers.
        AuthState::LoggedIn
    } else {
        AuthState::Unknown
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// A minimal one-turn request for the execution-surface tests.
    fn sample_completion_request() -> CompletionRequest {
        CompletionRequest {
            model: "m".into(),
            system: None,
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "go".into(),
            }],
            max_tokens: None,
            temperature: None,
        }
    }

    #[test]
    fn fork_yields_a_concurrent_instance() {
        // A real logged-in base MUST fork so the pipeline's parallel fan-out
        // (architecture || UI/UX) triggers; only offline falls back to serial.
        use umadev_runtime::Runtime;
        let forked = OpenCodeDriver::default().with_continue_session(true).fork();
        assert!(forked.is_some(), "a real base must fork for parallel work");
    }

    #[test]
    fn defaults_are_sane() {
        let d = OpenCodeDriver::default();
        assert_eq!(d.backend_id(), "opencode");
        assert_eq!(d.display_name(), "OpenCode CLI");
        assert_eq!(d.kind(), RuntimeKind::Openai);
        assert_eq!(d.permission_profile(), BasePermissionProfile::Plan);
    }

    #[test]
    fn version_parser_handles_labels_prefixes_and_semver_suffixes() {
        let parsed = parse_reported_opencode_version("OpenCode CLI v1.15.0+linux.x64 (stable)")
            .expect("a labelled version with build metadata is still semver");
        assert_eq!(parsed.to_string(), "1.15.0+linux.x64");
        assert_eq!(
            parse_reported_opencode_version("opencode V1.14.31-beta.1")
                .map(|version| version.to_string()),
            Some("1.14.31-beta.1".to_string())
        );
    }

    #[test]
    fn unparseable_version_remains_diagnostic_text() {
        assert_eq!(
            reported_opencode_version("OpenCode version unknown"),
            "OpenCode version unknown"
        );
        assert_eq!(reported_opencode_version(""), "<empty>");
    }

    #[tokio::test]
    async fn every_posture_adapts_to_every_reported_version() {
        for profile in [
            BasePermissionProfile::Plan,
            BasePermissionProfile::Guarded,
            BasePermissionProfile::Auto,
        ] {
            for raw in ["0.1.0", "1.14.30", "99.7.3+future", "version unknown", ""] {
                let d = OpenCodeDriver::with_program("echo")
                    .with_permissions(profile)
                    .with_version_output_for_test(raw);
                let resp = d
                    .complete(sample_completion_request())
                    .await
                    .expect("an installed OpenCode version must never be refused by version");
                assert_eq!(resp.id, "opencode-cli");
            }
        }
    }

    #[tokio::test]
    async fn probe_reports_ready_for_an_installed_old_version() {
        // An old installed version is still INSTALLED: the doctor/picker
        // shows Ready (with the detected version), like claude/codex/kimi/grok —
        // the read-only refusal is deferred to a Plan run, not shown as Unhealthy.
        let d = OpenCodeDriver::with_program("echo").with_version_output_for_test("1.14.30");
        match d.probe().await {
            ProbeResult::Ready { version, .. } => assert_eq!(version, "1.14.30"),
            other => panic!("an installed old version must probe Ready, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_reports_unhealthy_when_version_command_fails() {
        use std::os::unix::fs::PermissionsExt as _;
        // `--version` present but genuinely failing (non-zero exit) is Unhealthy,
        // distinct from an absent binary (NotInstalled).
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("broken-opencode");
        std::fs::write(&script, "#!/bin/sh\necho boom >&2\nexit 3\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let d = OpenCodeDriver::with_program(script.to_str().unwrap());
        assert!(matches!(d.probe().await, ProbeResult::Unhealthy { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn availability_probe_accepts_an_old_version() {
        use std::os::unix::fs::PermissionsExt as _;
        // The persistent start/resume path checks availability, never a version
        // allowlist. Plan authority is enforced by its runtime permission rules.
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("old-opencode");
        std::fs::write(&script, "#!/bin/sh\necho 1.14.30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = script.to_str().unwrap();
        let ws = std::env::temp_dir();

        ensure_opencode_available(program, &ws)
            .await
            .expect("an installed old version is available");
    }

    #[test]
    fn permission_profiles_shape_legacy_args_and_no_skip_only_tightens() {
        // One-shot `run` has no approval round-trip, so only EFFECTIVE Auto (writable
        // + `--auto`) gets the writable `build` agent; Plan AND Guarded stay read-only
        // `plan` so an unapproved write can never slip through the one-shot path.
        let cases = [
            (BasePermissionProfile::Plan, "plan", false),
            (BasePermissionProfile::Guarded, "plan", false),
            (BasePermissionProfile::Auto, "build", true),
        ];
        for (profile, agent, auto) in cases {
            let args = OpenCodeDriver::default()
                .with_permissions(profile)
                .base_args_for("anthropic/claude-sonnet", false);
            assert!(args.windows(2).any(|w| w[0] == "--agent" && w[1] == agent));
            assert_eq!(args.iter().any(|a| a == "--auto"), auto);
            assert!(args
                .windows(2)
                .any(|w| { w[0] == "--model" && w[1] == "anthropic/claude-sonnet" }));
        }

        // UMADEV_NO_SKIP_PERMS tightens Auto back to ordinary approvals — which the
        // one-shot cannot mediate — so it drops to the read-only `plan` agent AND emits
        // no `--auto`, never a writable-but-unapproved `build`.
        let tightened = OpenCodeDriver::default()
            .with_permissions(BasePermissionProfile::Auto)
            .base_args_for("m", true);
        assert!(tightened
            .windows(2)
            .any(|w| w[0] == "--agent" && w[1] == "plan"));
        assert!(!tightened.iter().any(|a| a == "--auto"));
    }

    #[test]
    fn continue_session_appends_resume_flag() {
        let fresh = OpenCodeDriver::default();
        let fresh_args = fresh.call_args("m");
        assert!(!fresh_args.contains(&"--continue".to_string()));
        assert!(fresh_args
            .windows(2)
            .any(|args| args == ["--format", "json"]));

        let mut resumed = OpenCodeDriver::default();
        resumed.set_continue_session(true);
        assert!(
            resumed.call_args("m").contains(&"--continue".to_string()),
            "a continued session with no pinned id must pass --continue so opencode uses its own memory"
        );
    }

    #[test]
    fn pinned_session_id_uses_deterministic_resume() {
        let id = "ses_01abcDEF".to_string();

        // Pinned id + continue → `--session <id>` (deterministic), NOT --continue.
        let mut resume = OpenCodeDriver::default().with_session_id(Some(id.clone()));
        resume.set_continue_session(true);
        let args = resume.call_args("m");
        assert!(
            args.windows(2).any(|w| w == ["--session", id.as_str()]),
            "pinned id must resume via --session <id>: {args:?}"
        );
        assert!(
            !args.contains(&"--continue".to_string()),
            "a pinned id must NOT fall back to --continue"
        );

        // The setter mirrors the builder.
        let mut via_setter = OpenCodeDriver::default();
        via_setter.set_session_id(Some(id.clone()));
        via_setter.set_continue_session(true);
        assert!(via_setter
            .call_args("m")
            .windows(2)
            .any(|w| w == ["--session", id.as_str()]));

        // A pinned id WITHOUT continue is still a fresh run (no resume flag) —
        // opencode has no "create with this id" flag.
        let fresh_pinned = OpenCodeDriver::default().with_session_id(Some(id.clone()));
        let args = fresh_pinned.call_args("m");
        assert!(!args.contains(&"--session".to_string()));
        assert!(!args.contains(&"--continue".to_string()));
    }

    #[test]
    fn a_fork_reset_does_not_erase_the_parent_session() {
        let parent = OpenCodeDriver::default().with_session_id(Some("ses_parent".to_string()));
        let child = parent.clone().with_session_id(None);
        assert_eq!(parent.pinned_session_id().as_deref(), Some("ses_parent"));
        assert!(child.pinned_session_id().is_none());
    }

    #[tokio::test]
    async fn probe_reports_not_installed_for_missing_binary() {
        let d = OpenCodeDriver::with_program("umadev-fake-opencode-xyz");
        let probe = d.probe().await;
        assert!(matches!(probe, ProbeResult::NotInstalled { .. }));
        assert!(!probe.is_ready());
        // NotInstalled auth state — distinct from NotLoggedIn, never LoggedIn.
        assert_eq!(probe.auth_state(), AuthState::NotInstalled);
        assert!(!probe.is_ready_and_authed());
    }

    #[test]
    fn install_and_login_hints_are_actionable() {
        let d = OpenCodeDriver::default();
        assert!(d.install_hint().unwrap().contains("opencode"));
        assert!(d.login_hint().unwrap().contains("auth login"));
    }

    #[test]
    fn classify_opencode_auth_file_three_states() {
        let dir = tempfile::TempDir::new().unwrap();

        // A non-empty provider object → at least one credential → LoggedIn.
        let logged_in = dir.path().join("auth_in.json");
        std::fs::write(&logged_in, r#"{"anthropic":{"type":"api","key":"x"}}"#).unwrap();
        assert_eq!(
            classify_opencode_auth_file(&logged_in),
            Some(AuthState::LoggedIn)
        );

        // An empty object `{}` → no providers configured → NotLoggedIn.
        let empty = dir.path().join("auth_empty.json");
        std::fs::write(&empty, "{}").unwrap();
        assert_eq!(
            classify_opencode_auth_file(&empty),
            Some(AuthState::NotLoggedIn)
        );

        // Absent file → None (caller falls through to the subcommand).
        let missing = dir.path().join("nope.json");
        assert_eq!(classify_opencode_auth_file(&missing), None);

        // Present but unparseable → None (fail-open, not a guess).
        let garbage = dir.path().join("garbage.json");
        std::fs::write(&garbage, "not json").unwrap();
        assert_eq!(classify_opencode_auth_file(&garbage), None);

        let oversized = dir.path().join("oversized.json");
        std::fs::write(&oversized, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert_eq!(classify_opencode_auth_file(&oversized), None);
    }

    #[cfg(unix)]
    #[test]
    fn opencode_auth_fifo_is_rejected_without_blocking() {
        let dir = tempfile::TempDir::new().unwrap();
        let fifo = dir.path().join("auth.json");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(classify_opencode_auth_file(&fifo), None);
    }

    #[test]
    fn classify_opencode_auth_list_three_states() {
        // Live `opencode auth list` output naming a provider + "1 credentials".
        let listed = "Credentials ~/.local/share/opencode/auth.json\nAnthropic api\n1 credentials";
        assert_eq!(classify_opencode_auth_list(listed), AuthState::LoggedIn);

        // No providers configured.
        assert_eq!(
            classify_opencode_auth_list("0 credentials"),
            AuthState::NotLoggedIn
        );
        assert_eq!(
            classify_opencode_auth_list("no credentials configured"),
            AuthState::NotLoggedIn
        );

        // Unrecognised → Unknown (fail-open, never a false positive).
        assert_eq!(classify_opencode_auth_list("???"), AuthState::Unknown);
        assert_eq!(classify_opencode_auth_list(""), AuthState::Unknown);
    }

    #[test]
    fn parse_line_forwards_plain_text_with_newline() {
        // A plain answer line becomes a Text delta (newline restored so lines
        // don't glue together in the typewriter view).
        let ev = parse_opencode_stream_line("Here is the analysis of the repo.")
            .expect("non-empty text line should emit an event");
        match ev {
            umadev_runtime::StreamEvent::Text { delta } => {
                assert_eq!(delta, "Here is the analysis of the repo.\n");
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // A blank line yields no event (no empty spam).
        assert!(parse_opencode_stream_line("").is_none());
        assert!(parse_opencode_stream_line("   ").is_none());
    }

    #[test]
    fn parse_line_recognizes_gutter_tool_step() {
        // A gutter-decorated tool step → ToolUse; ordinary prose containing a
        // tool word is NOT mis-tagged.
        let ev = parse_opencode_stream_line("│  Read  src/app.tsx")
            .expect("gutter tool line should emit an event");
        match ev {
            umadev_runtime::StreamEvent::ToolUse { name, detail, edit } => {
                assert_eq!(name, "Read");
                assert_eq!(detail, "src/app.tsx");
                // opencode's scraped gutter has no content → never a diff card.
                assert!(edit.is_none(), "opencode gutter scrape carries no edit");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        // Prose that merely mentions "Read" (no gutter) stays plain text.
        match parse_opencode_stream_line("I will Read the file next").unwrap() {
            umadev_runtime::StreamEvent::Text { .. } => {}
            other => panic!("plain prose must stay Text, got {other:?}"),
        }
    }

    #[test]
    fn structured_json_extracts_text_tool_and_authoritative_session() {
        let tool = r#"{"type":"tool_use","timestamp":123,"sessionID":"ses_abc","part":{"type":"tool","tool":"read","state":{"status":"completed","title":"Read src/lib.rs","input":{"filePath":"src/lib.rs"}}}}"#;
        let text = r#"{"type":"text","timestamp":124,"sessionID":"ses_abc","part":{"type":"text","text":"done","time":{"start":1,"end":2}}}"#;
        assert_eq!(
            opencode_session_id_from_line(tool).as_deref(),
            Some("ses_abc")
        );
        match parse_opencode_stream_line(tool).unwrap() {
            umadev_runtime::StreamEvent::ToolUse { name, detail, edit } => {
                assert_eq!(name, "read");
                assert_eq!(detail, "Read src/lib.rs");
                assert!(edit.is_none());
            }
            other => panic!("expected structured tool event, got {other:?}"),
        }
        assert_eq!(
            parse_opencode_stream_line(text),
            Some(umadev_runtime::StreamEvent::Text {
                delta: "done".to_string()
            })
        );
        let output = extract_opencode_output(&format!("{tool}\n{text}\n"));
        assert!(output.saw_structured_event);
        assert_eq!(output.text, "done");
        assert_eq!(
            extract_opencode_session_id(&format!("{tool}\n{text}\n")).as_deref(),
            Some("ses_abc")
        );
    }

    #[test]
    fn structured_parser_rejects_spoofed_or_malformed_json() {
        for line in [
            r#"{"type":"text","sessionID":"ses_abc","part":{"type":"text","text":"no timestamp"}}"#,
            r#"{"type":"text","timestamp":1,"sessionID":"../../escape","part":{"type":"text","text":"bad id"}}"#,
            r#"{"type":"unknown","timestamp":1,"sessionID":"ses_abc"}"#,
        ] {
            assert!(opencode_event(line).is_none(), "must reject: {line}");
            // Compatibility fallback treats an untrusted JSON-looking line as
            // plain text, never as a privileged tool/session event.
            assert!(matches!(
                parse_opencode_stream_line(line),
                Some(umadev_runtime::StreamEvent::Text { .. })
            ));
        }
    }

    #[test]
    fn resolve_opencode_answer_never_leaks_raw_json_or_empties() {
        // Strict structured stream → the parsed text is used verbatim.
        let strict = r#"{"type":"text","timestamp":124,"sessionID":"ses_abc","part":{"type":"text","text":"the answer"}}"#;
        assert_eq!(resolve_opencode_answer(strict), "the answer");

        // #12a: an older/custom opencode whose JSON lines OMIT the timestamp/sessionID
        // envelope — the strict parser rejects every line, so the old code returned the
        // raw JSONL verbatim (leaking event JSON). Now the words are salvaged instead.
        let no_envelope = r#"{"type":"text","part":{"type":"text","text":"salvaged words"}}"#;
        let resolved = resolve_opencode_answer(no_envelope);
        assert_eq!(resolved, "salvaged words");
        assert!(
            !resolved.contains('{'),
            "must not leak raw event JSON to the user: {resolved}"
        );

        // #12b: structured events present but the text sits in a shape the strict
        // extractor misses → best-effort salvage recovers it rather than an empty answer.
        let differing = r#"{"type":"text","sessionID":"ses_x","text":"top level text"}"#;
        assert_eq!(resolve_opencode_answer(differing), "top level text");

        // A genuinely plain-text reply (no JSON lines) passes through verbatim.
        let plain = "just a plain sentence from an old binary";
        assert_eq!(resolve_opencode_answer(plain), plain);

        // No texty JSON at all (only a tool event, no envelope) → salvage finds nothing,
        // so we do NOT fabricate; the verbatim stdout is returned (never an empty string
        // that silently drops the turn's output).
        let tool_only = r#"{"type":"tool_use","part":{"type":"tool","tool":"read"}}"#;
        assert_eq!(resolve_opencode_answer(tool_only), tool_only);
    }

    #[test]
    fn captured_fresh_session_becomes_a_pinned_resume() {
        let mut driver = OpenCodeDriver::default();
        driver.remember_session_id("ses_fresh");
        driver.set_continue_session(true);
        let args = driver.call_args("m");
        assert!(args
            .windows(2)
            .any(|args| args == ["--session", "ses_fresh"]));
        assert!(!args.contains(&"--continue".to_string()));
    }

    // The fake is a `#!/bin/sh` script Windows cannot exec; the per-line
    // forwarding logic is also covered by the `parse_opencode_stream_line` unit
    // tests above, which are platform-independent.
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_emits_one_event_per_line_not_a_single_dump() {
        // The whole point of the opencode streaming override: a multi-line
        // answer must arrive as SEVERAL incremental events (one per line), not
        // one buffer-then-dump event. Drive a fake binary that prints 3 lines.
        use std::sync::{Arc, Mutex};
        use umadev_runtime::StreamEvent;

        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-opencode");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null 2>&1\nprintf 'line one\\nline two\\nline three\\n'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let d = OpenCodeDriver::with_program(script.to_str().unwrap())
            .with_version_output_for_test("1.17.16");
        let req = CompletionRequest {
            model: "m".into(),
            system: None,
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "go".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let events: Arc<Mutex<Vec<StreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let on_event = move |ev: StreamEvent| {
            sink.lock().unwrap().push(ev);
        };
        let resp = d.complete_streaming(req, &on_event).await.unwrap();
        let got = events.lock().unwrap();
        let text_events = got
            .iter()
            .filter(|e| matches!(e, StreamEvent::Text { .. }))
            .count();
        assert!(
            text_events >= 3,
            "expected >=3 incremental Text events (one per line), got {text_events}: {got:?}"
        );
        // The final assembled answer still carries all three lines.
        assert!(resp.text.contains("line one"));
        assert!(resp.text.contains("line three"));
        assert_eq!(resp.id, "opencode-cli");
    }

    #[tokio::test]
    async fn complete_drives_a_fake_opencode_binary() {
        let d = OpenCodeDriver::with_program("echo").with_version_output_for_test("1.17.16");
        let req = CompletionRequest {
            model: "anthropic/claude-sonnet-4-5".into(),
            system: Some("be concise".into()),
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "explain the repo".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let resp = d.complete(req).await.unwrap();
        assert_eq!(resp.id, "opencode-cli");
        assert_eq!(resp.model, "anthropic/claude-sonnet-4-5");
        assert!(resp.text.contains("run"));
        assert!(resp.text.contains("--model"));
        assert!(resp.text.contains("be concise"));
        assert!(resp.text.contains("explain the repo"));
    }

    #[test]
    fn stream_events_redact_synthetic_secrets() {
        const SECRET: &str = "SYNTH_OPENCODE_SECRET_DO_NOT_LEAK_73";
        let text = parse_opencode_stream_line(&format!("password={SECRET}"));
        let tool = parse_opencode_stream_line(&format!(
            "│ Bash curl -H 'Authorization: Bearer {SECRET}' example.test"
        ));
        let rendered = format!("{text:?}{tool:?}");
        assert!(
            !rendered.contains(SECRET),
            "stream event leaked: {rendered}"
        );
    }
}
