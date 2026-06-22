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
//! the same dir); with no pinned id it falls back to `--continue`.

use std::time::Duration;

use async_trait::async_trait;
use umadev_runtime::{
    CompletionRequest, CompletionResponse, Runtime, RuntimeError, RuntimeKind, Usage,
};

use crate::{
    default_workspace, merge_prompt, run_subprocess, run_subprocess_streaming, HostDriver,
    ProbeResult, PromptChannel, SubprocessCall,
};

/// Drives the `opencode` CLI as a subprocess.
#[derive(Debug, Clone)]
pub struct OpenCodeDriver {
    program: String,
    timeout: Duration,
    /// When `true`, the next `complete` resumes a prior `opencode` session so
    /// the base keeps its own memory — deterministically via `-s <id>` when a
    /// [`Self::session_id`] is pinned, otherwise `--continue` (most recent).
    continue_session: bool,
    /// An explicit `opencode` session id to resume. When set AND
    /// [`Self::continue_session`] is true, the call uses `-s <id>` so UmaDev
    /// resumes *its own* session deterministically instead of grabbing
    /// "the most recent in this dir" (which could be the user's other
    /// conversation). When `None`, falls back to `--continue`.
    session_id: Option<String>,
    /// The cwd the `opencode` subprocess runs in (the pipeline project root).
    workspace: Option<std::path::PathBuf>,
}

impl Default for OpenCodeDriver {
    fn default() -> Self {
        Self {
            program: std::env::var("UMADEV_OPENCODE_BIN")
                .unwrap_or_else(|_| "opencode".to_string()),
            timeout: crate::worker_timeout_from_env(),
            continue_session: false,
            session_id: None,
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

    /// The argument vector preceding the prompt. Exposed for tests.
    #[must_use]
    pub fn base_args(&self, model: &str) -> Vec<String> {
        let mut args = vec!["run".to_string()];
        // OpenCode model ids are provider/model. UmaDev's default launch
        // model (`claude-sonnet-4-6`) is not in that shape, so only pass a
        // model when the user explicitly selected an OpenCode-compatible id.
        if model.contains('/') {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
        // Auto-approve tool permissions so the headless `run` never blocks on
        // an interactive y/n that can't be answered in subprocess mode (the
        // claude/codex drivers have their equivalent). UmaDev's governance
        // layer is the safety net. Opt out with UMADEV_NO_SKIP_PERMS=1.
        if std::env::var("UMADEV_NO_SKIP_PERMS").as_deref() != Ok("1") {
            args.push("--dangerously-skip-permissions".to_string());
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
    /// live `opencode run --help`. A pinned id is preferred because `--continue`
    /// could otherwise grab the user's other conversation in the same directory.
    ///
    // TODO(opencode): we cannot yet *capture* the session id opencode assigns on
    // a fresh turn (opencode has no "create with this id" flag like claude's
    // `--session-id`; the id only appears in `--format json` output, whose exact
    // event schema is not yet confirmed on this machine). Until that schema is
    // verified, turn 1 stays a fresh `run` and only an externally-pinned id
    // drives deterministic `-s <id>` resume. Do NOT add `--format json` to the
    // run path before the usage/session-id event shape is confirmed — it would
    // turn the plain-text stdout this driver parses into raw JSON and break
    // `complete`'s answer extraction.
    #[must_use]
    pub fn call_args(&self, model: &str) -> Vec<String> {
        let mut args = self.base_args(model);
        if self.continue_session {
            match &self.session_id {
                Some(id) => {
                    // Resume OUR specific session — never "the most recent in
                    // this dir", so we can't continue the user's other chat.
                    args.push("--session".to_string());
                    args.push(id.clone());
                }
                None => {
                    // `--continue` resumes the last session so `opencode` answers
                    // with its own prior context instead of starting cold.
                    args.push("--continue".to_string());
                }
            }
        }
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
        // OpenCode has no `/goal` mode, no structured stream-json (its
        // `complete_streaming` override forwards plain-text lines, but there's
        // no machine-readable event schema), no usage on stdout, and no
        // PreToolUse hook — the most conservative of the three CLIs.
        umadev_runtime::BrainCapabilities::default()
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, RuntimeError> {
        let prompt = merge_prompt(&req);
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);
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

        Ok(CompletionResponse {
            text: out.stdout,
            id: "opencode-cli".to_string(),
            model: req.model,
            usage: Usage::default(),
        })
    }

    /// Streaming completion via `opencode run`, forwarding stdout **line by
    /// line** so the TUI shows live progress instead of a frozen spinner.
    ///
    /// `opencode run` writes a plain-text answer (NOT structured JSONL like
    /// `claude --output-format stream-json` / `codex exec --json`), so there is
    /// no machine-readable tool-call schema to parse — and `--format json` is
    /// deliberately NOT used here (it would turn the plain-text stdout this
    /// driver parses into raw JSON and break answer extraction; see the
    /// `call_args` TODO). What this DOES win is the thing that mattered most:
    /// before this override, `OpenCodeDriver` fell back to the trait-default
    /// `complete_streaming` — a blocking `complete` that emits **nothing** until
    /// the whole phase ends, leaving the entire research phase with zero
    /// intermediate events and a 25s+ blank screen. Forwarding each line as a
    /// [`StreamEvent::Text`] delta as it arrives — even without a structured
    /// stream — is strictly better than buffer-then-dump: the user sees the
    /// answer materialize in real time and the runner's stream-activity tracker
    /// stops heartbeating "still working" over a base that IS producing output.
    ///
    /// A line that looks like a tool-call marker (`opencode` prints lines such
    /// as `|  Read  path` for tool steps in some configurations) is forwarded as
    /// a [`StreamEvent::ToolUse`] so the activity reads richer when present;
    /// everything else is plain text. Fail-open: on ANY streaming error we fall
    /// back to the non-streaming [`complete`](Self::complete) so a format change
    /// or an environment without line-buffered stdout never breaks the phase.
    /// Timeout / empty-reply / error semantics are inherited unchanged from the
    /// shared subprocess layer and the `complete` fallback.
    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
    ) -> Result<CompletionResponse, RuntimeError> {
        let prompt = merge_prompt(&req);
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);
        let args = self.call_args(&req.model);
        let model = req.model.clone();
        let program = self.program.clone();
        let timeout = self.timeout;

        // Accumulate the raw stream so a mid-stream failure can salvage whatever
        // already arrived (opencode's answer IS its plain stdout) instead of
        // cold-restarting a whole new run.
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
                if let Some(ev) = parse_opencode_stream_line(line) {
                    on_event(ev);
                }
            },
        )
        .await;

        match result {
            Ok(out) => Ok(CompletionResponse {
                text: out.stdout,
                id: "opencode-cli".to_string(),
                model,
                usage: Usage::default(),
            }),
            Err(e) => {
                // Fail-open: drop to the non-streaming path so a streaming-only
                // failure (no line-buffered stdout, format drift, or the base
                // being SIGTERM/SIGALRM'd — exit 143/142) never empties the
                // phase. Routine self-healing → `debug!`, not a scary warning.
                // Salvage what already streamed (opencode's text IS its stdout)
                // before paying for a full `complete` re-run.
                tracing::debug!(error = %e, "opencode streaming failed, falling back to non-streaming");
                let partial = stream_buf.into_inner().unwrap_or_default();
                if !partial.trim().is_empty() {
                    return Ok(CompletionResponse {
                        text: partial,
                        id: "opencode-cli".to_string(),
                        model,
                        usage: Usage::default(),
                    });
                }
                drop(args);
                drop(prompt);
                self.complete(req).await
            }
        }
    }
}

/// Turn one line of plain-text `opencode run` stdout into a [`StreamEvent`].
///
/// `opencode run` is NOT structured JSONL, so this is a best-effort plain-text
/// forwarder, not a parser: every non-blank line becomes a [`StreamEvent::Text`]
/// delta (with its newline restored so the typewriter view reads naturally),
/// EXCEPT lines that match a recognisable tool-step marker, which become a
/// [`StreamEvent::ToolUse`] so the activity reads richer when opencode prints
/// them. A blank line yields `None` (no empty events). This is deliberately
/// conservative — anything not confidently a tool step is treated as text, so a
/// format change degrades to "plain text streaming", never to a wrong tag.
fn parse_opencode_stream_line(line: &str) -> Option<umadev_runtime::StreamEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
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
/// returning a [`StreamEvent::ToolUse`] or `None`. Conservative: only fires when
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
    Some(umadev_runtime::StreamEvent::ToolUse {
        name: name.to_string(),
        detail,
    })
}

#[async_trait]
impl HostDriver for OpenCodeDriver {
    fn backend_id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode CLI"
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
        let forked = OpenCodeDriver::default().with_continue_session(true).fork();
        assert!(forked.is_some(), "a real base must fork for parallel work");
    }

    #[test]
    fn defaults_are_sane() {
        let d = OpenCodeDriver::default();
        assert_eq!(d.backend_id(), "opencode");
        assert_eq!(d.display_name(), "OpenCode CLI");
        assert_eq!(d.kind(), RuntimeKind::Openai);
        // A bare (non provider/model) id is NOT passed; the headless
        // skip-permissions flag is added by default.
        assert_eq!(
            d.base_args("claude-sonnet-4-6"),
            vec![
                "run".to_string(),
                "--dangerously-skip-permissions".to_string()
            ]
        );
        assert_eq!(
            d.base_args("anthropic/claude-sonnet-4-5"),
            vec![
                "run".to_string(),
                "--model".to_string(),
                "anthropic/claude-sonnet-4-5".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]
        );
    }

    #[test]
    fn continue_session_appends_resume_flag() {
        let fresh = OpenCodeDriver::default();
        assert!(!fresh.call_args("m").contains(&"--continue".to_string()));

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

    #[tokio::test]
    async fn probe_reports_not_installed_for_missing_binary() {
        let d = OpenCodeDriver::with_program("umadev-fake-opencode-xyz");
        let probe = d.probe().await;
        assert!(matches!(probe, ProbeResult::NotInstalled { .. }));
        assert!(!probe.is_ready());
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
            umadev_runtime::StreamEvent::ToolUse { name, detail } => {
                assert_eq!(name, "Read");
                assert_eq!(detail, "src/app.tsx");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        // Prose that merely mentions "Read" (no gutter) stays plain text.
        match parse_opencode_stream_line("I will Read the file next").unwrap() {
            umadev_runtime::StreamEvent::Text { .. } => {}
            other => panic!("plain prose must stay Text, got {other:?}"),
        }
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
        let d = OpenCodeDriver::with_program(script.to_str().unwrap());
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
        let d = OpenCodeDriver::with_program("echo");
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
}
