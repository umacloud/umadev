//! `CodexDriver` — drives the `codex` CLI in non-interactive exec mode.
//!
//! Shells out to:
//!
//! ```text
//! <prompt on stdin> | codex exec --skip-git-repo-check --sandbox workspace-write --color never --json
//! ```
//!
//! IMPORTANT — the prompt goes on STDIN, not as a positional arg. codex 0.141's
//! `exec` reads its prompt from stdin ("Reading prompt from stdin…"); when the
//! prompt is passed as an arg and stdin is then closed (UmaDev's Arg channel
//! closes stdin to avoid hangs), codex prints "Reading additional input from
//! stdin…" and exits 1 — every call fails and falls back to an offline scaffold.
//! Feeding the prompt via `PromptChannel::Stdin` is what makes real codex runs
//! work. `--json` makes codex emit JSONL events we parse for the answer.
//!
//! Like the Claude Code driver, it uses the user's already-authenticated
//! `codex` session — no API key required.
//!
//! Flag rationale:
//!
//! - `--skip-git-repo-check`: UmaDev workspaces are often `output/` + `.umadev/` scratch dirs without a git repo. Codex otherwise refuses to run.
//! - `--sandbox workspace-write`: required for headless use. Without it codex enters interactive approval mode and hangs waiting for stdin confirmation. `workspace-write` permits reads + writes scoped to cwd, no network or system mutation.
//! - `--color never`: don't emit ANSI escape sequences. (`run_subprocess` strips them anyway; this is cleaner at the source.)
//!
//! ## Known environment requirements
//!
//! `codex exec` calls `https://chatgpt.com/backend-api/...` on the user's
//! `ChatGPT` subscription. If that endpoint is unreachable (firewall,
//! corporate proxy, region block), codex retries 5 times then errors —
//! UmaDev catches the failure and falls back to the offline template
//! (with a `tracing::warn!`). The driver itself is correct; the failure
//! is environmental.
//!
//! Per-call timeout is [`DEFAULT_TIMEOUT`] (5 minutes). If your codex CLI
//! is hanging (e.g. `codex login` hasn't completed), the call falls back
//! to the offline template after the timeout fires.
//!
//! Overridable for forward compatibility:
//!
//! - `UMADEV_CODEX_BIN`       — program name (default `codex`)
//! - `UMADEV_CODEX_EXEC_SUBCMD` — exec subcommand (default `exec`)

use std::time::Duration;

use async_trait::async_trait;
use umadev_runtime::{
    CompletionRequest, CompletionResponse, Runtime, RuntimeError, RuntimeKind, Usage,
};

use crate::{
    default_workspace, merge_prompt, model_args, run_subprocess, run_subprocess_streaming,
    HostDriver, ProbeResult, PromptChannel, SubprocessCall,
};

/// Drives the `codex` CLI as a subprocess.
#[derive(Debug, Clone)]
pub struct CodexDriver {
    program: String,
    exec_subcmd: String,
    timeout: Duration,
    /// When `true`, the next `complete` resumes the most recent `codex`
    /// session (`codex exec resume --last`) so the base keeps its own memory.
    continue_session: bool,
    /// The cwd the `codex` subprocess runs in (the pipeline project root).
    workspace: Option<std::path::PathBuf>,
}

impl Default for CodexDriver {
    fn default() -> Self {
        Self {
            program: std::env::var("UMADEV_CODEX_BIN").unwrap_or_else(|_| "codex".to_string()),
            exec_subcmd: std::env::var("UMADEV_CODEX_EXEC_SUBCMD")
                .unwrap_or_else(|_| "exec".to_string()),
            timeout: crate::worker_timeout_from_env(),
            continue_session: false,
            workspace: None,
        }
    }
}

impl CodexDriver {
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

    /// Argument vector for resuming the most recent session. `codex exec
    /// resume --last` continues the last recorded session in this workspace so
    /// the base answers with its own prior context.
    ///
    /// CRITICAL ordering: codex parses flags per-subcommand. Exec-parent flags
    /// (`--skip-git-repo-check`, `--sandbox`, `--color`, `--json`, `--dangerously-
    /// bypass-approvals-and-sandbox`) MUST come BEFORE the `resume` token —
    /// placed after it, codex's clap rejects them with "unexpected argument" and
    /// the whole resume call errors out. So resume = the full exec flag set
    /// (`base_args`, which already carries `--json` + the bypass) followed by
    /// `resume --last`. `--model` is appended at the call site (global flag).
    #[must_use]
    pub fn resume_args(&self) -> Vec<String> {
        let mut args = self.base_args();
        args.push("resume".to_string());
        args.push("--last".to_string());
        args
    }

    /// The full argument vector for a `complete` call — resume args when
    /// [`Self::continue_session`] is set, otherwise a fresh `exec`. Exposed for
    /// tests. The prompt is appended by the subprocess layer as the last
    /// positional argument.
    #[must_use]
    pub fn call_args(&self) -> Vec<String> {
        if self.continue_session {
            self.resume_args()
        } else {
            self.base_args()
        }
    }

    /// The argument vector preceding the prompt. Exposed for tests.
    ///
    /// Flag rationale:
    /// - `--skip-git-repo-check`: UmaDev workspaces are frequently
    ///   `output/` + `.umadev/` scratch dirs that aren't git repos;
    ///   codex otherwise refuses to run.
    /// - `--sandbox workspace-write`: required for headless use,
    ///   otherwise codex enters interactive approval mode and hangs
    ///   waiting for user input on stdin. `workspace-write` permits
    ///   reads + writes scoped to cwd, no network or system mutation.
    /// - `--dangerously-bypass-approvals-and-sandbox`: skip ALL
    ///   confirmation prompts so the pipeline is fully autonomous.
    ///   Without this, codex pauses on every tool call waiting for a
    ///   y/n that never arrives in non-interactive subprocess mode.
    ///   UmaDev's governance layer (112 rules + quality gate) is the
    ///   safety net that replaces codex's built-in approval system.
    ///   Set `UMADEV_NO_SKIP_PERMS=1` to disable.
    /// - `--color never`: don't emit ANSI escape sequences that would
    ///   later need stripping. (`run_subprocess` strips them anyway,
    ///   but this is cleaner at the source.)
    #[must_use]
    pub fn base_args(&self) -> Vec<String> {
        let mut args = vec![
            self.exec_subcmd.clone(),
            "--skip-git-repo-check".to_string(),
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "--color".to_string(),
            "never".to_string(),
            // Emit newline-delimited JSON events so BOTH the streaming path AND
            // the non-streaming `complete` path can extract the real answer
            // (`agent_message`) instead of codex's human-readable banner/footer
            // ("OpenAI Codex vX … user … codex … tokens used"). Without this,
            // `complete` returned that whole banner as the "answer".
            "--json".to_string(),
        ];
        // Bypass all approval prompts so the pipeline runs unattended.
        // UmaDev's governance replaces the host's permission system.
        if std::env::var("UMADEV_NO_SKIP_PERMS").as_deref() != Ok("1") {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        }
        args
    }
}

#[async_trait]
impl Runtime for CodexDriver {
    /// Concurrent-safe fork: clone with a FRESH session (no `resume --last`).
    fn fork(&self) -> Option<Box<dyn Runtime>> {
        Some(Box::new(self.clone().with_continue_session(false)))
    }

    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Openai
    }

    fn capabilities(&self) -> umadev_runtime::BrainCapabilities {
        // Codex streams (`--json`) but has no `/goal` mode, no usage on stdout,
        // and no PreToolUse hook.
        umadev_runtime::BrainCapabilities {
            persistent_goal: false,
            streaming: true,
            reports_usage: false,
            realtime_governance: false,
        }
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, RuntimeError> {
        let prompt = merge_prompt(&req);
        let mut args = self.call_args();
        // `--model` is a global flag (valid on `exec` AND `exec resume`).
        args.extend(codex_model_args(&req.model));
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);
        let out = run_subprocess(SubprocessCall {
            program: &self.program,
            args: &args,
            prompt: &prompt,
            channel: PromptChannel::Stdin,
            workspace: &ws,
            timeout: self.timeout,
            env: &[],
        })
        .await
        .map_err(crate::map_subprocess_error)?;

        // base_args carries `--json`, so stdout is a JSONL event stream — extract
        // the `agent_message` text(s). Fall back to raw stdout only if extraction
        // yields nothing (so an unexpected format never silently empties the run).
        let mut text = extract_codex_messages(&out.stdout);
        if text.trim().is_empty() && !out.stdout.trim().is_empty() {
            text = out.stdout;
        }
        Ok(CompletionResponse {
            text,
            id: "codex-cli".to_string(),
            model: req.model,
            usage: Usage::default(),
        })
    }

    /// Streaming completion via `codex exec --json`.
    ///
    /// Codex emits newline-delimited JSON events (verified against real
    /// `codex exec --json` output):
    /// - `{"type":"thread.started"}` / `{"type":"turn.started"}` — lifecycle,
    ///   skipped.
    /// - `{"type":"item.completed","item":{"type":"agent_message","text":"…"}}`
    ///   → [`StreamEvent::Text`].
    /// - `{"type":"item.completed","item":{"type":"command_execution","command":"sed …"}}`
    ///   → [`StreamEvent::ToolUse`] with name "Bash" + the command.
    /// - `{"type":"item.completed","item":{"type":"file_change",...}}`
    ///   → [`StreamEvent::ToolUse`] with name "Write" + the path.
    /// - `{"type":"turn.completed",...}` → done.
    ///
    /// Falls back to non-streaming `complete` on any error.
    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        on_event: &(dyn Fn(umadev_runtime::StreamEvent) + Send + Sync),
    ) -> Result<CompletionResponse, RuntimeError> {
        let prompt = merge_prompt(&req);
        // Identical args to `complete` (base_args / resume_args — both carry
        // `--json` and the bypass), so streaming also resumes the session on
        // multi-turn calls and the two paths can't drift apart.
        let mut args = self.call_args();
        args.extend(codex_model_args(&req.model));

        let model = req.model.clone();
        let timeout = self.timeout;
        let program = self.program.clone();
        let ws = self.workspace.clone().unwrap_or_else(default_workspace);

        let result = run_subprocess_streaming(
            SubprocessCall {
                program: &program,
                args: &args,
                prompt: &prompt,
                channel: PromptChannel::Stdin,
                workspace: &ws,
                timeout,
                env: &[],
            },
            &|line: &str| {
                if let Some(ev) = parse_codex_stream_line(line) {
                    on_event(ev);
                }
            },
        )
        .await;

        match result {
            Ok(out) => {
                // Extract all agent_message texts from the JSONL stream.
                let mut final_text = extract_codex_messages(&out.stdout);
                if final_text.trim().is_empty() && !out.stdout.trim().is_empty() {
                    final_text = out.stdout;
                }
                Ok(CompletionResponse {
                    text: final_text,
                    id: "codex-cli".to_string(),
                    model,
                    usage: Usage::default(),
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "codex streaming failed, falling back");
                drop(args);
                drop(prompt);
                self.complete(req).await
            }
        }
    }
}

/// Parse one line of `codex exec --json` output into a [`StreamEvent`].
/// Returns `None` for lines that aren't JSON or don't carry displayable
/// content (thread.started, turn.started, etc.).
///
/// Verified against real `codex exec --json` output — codex uses
/// `command_execution` (not `tool_call`) for shell commands, and the
/// command is in the `command` field.
fn parse_codex_stream_line(line: &str) -> Option<umadev_runtime::StreamEvent> {
    let line = line.trim();
    if line.is_empty() || !line.starts_with('{') {
        return None;
    }
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("item.completed") {
        return None;
    }
    let item = v.get("item")?;
    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match item_type {
        "agent_message" => {
            let text = item.get("text").and_then(|t| t.as_str())?;
            if text.is_empty() {
                None
            } else {
                Some(umadev_runtime::StreamEvent::Text {
                    delta: text.to_string(),
                })
            }
        }
        "command_execution" | "tool_call" | "shell_tool_call" => {
            let name = if item_type == "command_execution" {
                "Bash".to_string()
            } else {
                item.get("tool_name")
                    .or_else(|| item.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool")
                    .to_string()
            };
            let detail = item
                .get("command")
                .or_else(|| item.get("args_text"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            Some(umadev_runtime::StreamEvent::ToolUse { name, detail })
        }
        "file_change" | "file_edit" => {
            // Codex file_change has a `changes` array: [{"path":"…","kind":"update"}].
            // Fall back to top-level `path` for forward-compat if the format changes.
            let path = item
                .get("changes")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|ch| ch.get("path"))
                .and_then(|p| p.as_str())
                .or_else(|| item.get("path").and_then(|p| p.as_str()))
                .or_else(|| item.get("file_path").and_then(|p| p.as_str()))
                .unwrap_or("")
                .to_string();
            // Determine if it's a create vs update for the icon.
            let kind = item
                .get("changes")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|ch| ch.get("kind"))
                .and_then(|k| k.as_str())
                .unwrap_or("update");
            // codex's PatchChangeKind serializes to `add`/`update`/`delete`
            // (NOT `create`) — so a new file is `add`. (Keep `create` as a
            // forward-compat alias.)
            let tool_name = if kind == "add" || kind == "create" {
                "Write"
            } else {
                "Edit"
            };
            Some(umadev_runtime::StreamEvent::ToolUse {
                name: tool_name.to_string(),
                detail: path,
            })
        }
        _ => None,
    }
}

/// `--model` args for codex, but ONLY when the model is one codex can actually
/// run. codex with a ChatGPT account accepts its own models (`gpt-*`, `o1`/`o3`/
/// `o4`, `codex-*`); the pipeline's DEFAULT model id is claude-centric
/// (`claude-sonnet-4-6`), and forwarding it makes codex reject the entire turn:
/// "The 'claude-sonnet-4-6' model is not supported when using Codex with a
/// ChatGPT account." So a non-codex model id is dropped — codex then uses the
/// account default (gpt-5.x) — while an explicit gpt/codex model is honored.
fn codex_model_args(model: &str) -> Vec<String> {
    let m = model.trim().to_ascii_lowercase();
    let codex_native = m.starts_with("gpt")
        || m.starts_with("codex")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4");
    if codex_native {
        model_args(model)
    } else {
        Vec::new()
    }
}

/// Extract all `agent_message` texts from a codex `--json` JSONL stream.
fn extract_codex_messages(stdout: &str) -> String {
    let mut texts = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
                if let Some(item) = v.get("item") {
                    if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            texts.push(text.to_string());
                        }
                    }
                }
            }
        }
    }
    texts.join("\n")
}

#[async_trait]
impl HostDriver for CodexDriver {
    fn backend_id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn set_continue_session(&mut self, continue_session: bool) {
        self.continue_session = continue_session;
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
        let forked = CodexDriver::default().with_continue_session(true).fork();
        assert!(forked.is_some(), "a real base must fork for parallel work");
    }
    use umadev_runtime::StreamEvent;

    // ---- codex --json parsing (verified against real codex output) ----

    #[test]
    fn parse_skips_thread_started() {
        let line = r#"{"type":"thread.started","thread_id":"abc-123"}"#;
        assert!(parse_codex_stream_line(line).is_none());
    }

    #[test]
    fn parse_skips_turn_started() {
        let line = r#"{"type":"turn.started"}"#;
        assert!(parse_codex_stream_line(line).is_none());
    }

    #[test]
    fn parse_extracts_agent_message() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"The version is 4.6.0."}}"#;
        let ev = parse_codex_stream_line(line).expect("should parse");
        match ev {
            StreamEvent::Text { delta } => assert!(delta.contains("4.6.0")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn parse_extracts_command_execution() {
        // Real codex format: type=command_execution, command field has the shell cmd.
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc \"sed -n '1,120p' Cargo.toml\"","exit_code":0,"status":"completed"}}"#;
        let ev = parse_codex_stream_line(line).expect("should parse");
        match ev {
            StreamEvent::ToolUse { name, detail } => {
                assert_eq!(name, "Bash", "command_execution should map to Bash");
                assert!(detail.contains("sed"), "detail should contain the command");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn parse_extracts_file_change_update() {
        // Real codex format: changes array with path + kind.
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/tmp/test.txt","kind":"update"}],"status":"completed"}}"#;
        let ev = parse_codex_stream_line(line).expect("should parse");
        match ev {
            StreamEvent::ToolUse { name, detail } => {
                assert_eq!(name, "Edit", "kind=update should map to Edit");
                assert_eq!(detail, "/tmp/test.txt");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn parse_extracts_file_change_add_as_write() {
        // Real codex emits kind `add` for a new file (NOT `create`).
        let line = r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"src/new.ts","kind":"add"}]}}"#;
        let ev = parse_codex_stream_line(line).expect("should parse");
        match ev {
            StreamEvent::ToolUse { name, detail } => {
                assert_eq!(name, "Write", "kind=add should map to Write");
                assert_eq!(detail, "src/new.ts");
            }
            _ => panic!("expected ToolUse"),
        }
        // `update` → Edit.
        let upd = r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"src/x.ts","kind":"update"}]}}"#;
        match parse_codex_stream_line(upd).expect("parse") {
            StreamEvent::ToolUse { name, .. } => assert_eq!(name, "Edit"),
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn parse_skips_non_json_and_empty() {
        assert!(parse_codex_stream_line("").is_none());
        assert!(parse_codex_stream_line("not json").is_none());
        assert!(parse_codex_stream_line("{broken").is_none());
    }

    #[test]
    fn extract_codex_messages_from_full_stream() {
        let stream = r#"{"type":"thread.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"I'll check the version."}}
{"type":"item.completed","item":{"type":"command_execution","command":"cat Cargo.toml"}}
{"type":"item.completed","item":{"type":"agent_message","text":"The version is 4.6.0."}}
{"type":"turn.completed"}"#;
        let result = extract_codex_messages(stream);
        assert!(result.contains("I'll check the version."));
        assert!(result.contains("4.6.0"));
        // command_execution is NOT an agent_message — should not appear.
        assert!(!result.contains("cat Cargo.toml"));
    }

    #[test]
    fn defaults_are_sane() {
        let d = CodexDriver::default();
        assert_eq!(d.backend_id(), "codex");
        assert_eq!(d.display_name(), "Codex CLI");
        assert_eq!(d.kind(), RuntimeKind::Openai);
        let args = d.base_args();
        // Stable prefix (the bypass flag is appended conditionally).
        assert_eq!(
            &args[..6],
            &[
                "exec".to_string(),
                "--skip-git-repo-check".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--color".to_string(),
                "never".to_string(),
            ]
        );
        assert!(
            args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()),
            "base_args should include bypass flag by default: {args:?}"
        );
    }

    #[test]
    fn continue_session_switches_to_resume_subcommand() {
        // Fresh: a normal `codex exec ...` (no resume).
        let fresh = CodexDriver::default();
        let args = fresh.call_args();
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(!args.contains(&"resume".to_string()));

        // Continued: `codex exec <exec-parent flags> resume --last …`.
        // CRITICAL: every exec-parent flag (--skip-git-repo-check / --sandbox /
        // --color / --json) MUST come BEFORE the `resume` token, or codex's clap
        // rejects it with "unexpected argument" and the resume call errors out.
        let mut resumed = CodexDriver::default();
        resumed.set_continue_session(true);
        let args = resumed.call_args();
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        let resume_idx = args
            .iter()
            .position(|a| a == "resume")
            .expect("resume args contain `resume`");
        assert_eq!(args.get(resume_idx + 1).map(String::as_str), Some("--last"));
        for flag in ["--skip-git-repo-check", "--sandbox", "--color", "--json"] {
            let idx = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("resume args missing {flag}: {args:?}"));
            assert!(idx < resume_idx, "{flag} must precede `resume`: {args:?}");
        }
    }

    #[test]
    fn codex_model_args_drops_non_codex_models() {
        // The claude-centric pipeline default must NOT reach codex (it would
        // reject the whole turn). Non-codex / empty ids are dropped.
        assert!(codex_model_args("claude-sonnet-4-6").is_empty());
        assert!(codex_model_args("").is_empty());
        assert!(codex_model_args("gemini-2.0-flash").is_empty());
        // codex-native models ARE forwarded.
        assert_eq!(codex_model_args("gpt-5.5"), vec!["--model", "gpt-5.5"]);
        assert_eq!(codex_model_args("o3-mini"), vec!["--model", "o3-mini"]);
        assert_eq!(
            codex_model_args("codex-mini-latest"),
            vec!["--model", "codex-mini-latest"]
        );
    }

    #[tokio::test]
    async fn probe_reports_not_installed_for_missing_binary() {
        let d = CodexDriver::with_program("umadev-fake-codex-xyz");
        assert!(matches!(d.probe().await, ProbeResult::NotInstalled { .. }));
    }

    #[tokio::test]
    async fn complete_drives_a_fake_codex_binary() {
        // Fake codex models 0.141: read the prompt from STDIN and emit a JSONL
        // `agent_message` echoing it — exercising the real
        // stdin -> --json -> extract_codex_messages round-trip (a bare `echo`
        // fake would not, since the prompt no longer rides on argv).
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-codex");
        std::fs::write(
            &script,
            "#!/bin/sh\nline=$(cat)\nprintf '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"%s\"}}\\n' \"$line\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let d = CodexDriver::with_program(script.to_str().unwrap());
        let req = CompletionRequest {
            model: "gpt-5-codex".into(),
            system: None,
            messages: vec![umadev_runtime::Message {
                role: "user".into(),
                content: "generate a migration".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        let resp = d.complete(req).await.unwrap();
        assert!(
            resp.text.contains("generate a migration"),
            "prompt should reach codex via stdin and parse back; got: {}",
            resp.text
        );
        assert_eq!(resp.model, "gpt-5-codex");
    }
}
