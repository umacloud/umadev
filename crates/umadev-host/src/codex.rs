//! `CodexDriver` — drives the `codex` CLI in non-interactive exec mode.
//!
//! Shells out to:
//!
//! ```text
//! <prompt on stdin> | codex exec --skip-git-repo-check --sandbox <profile> --color never --json
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
//! - `--sandbox <profile>`: Plan is always `read-only`; Guarded/Auto receive the
//!   full development environment unless an explicit project restriction
//!   narrows it.
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
//! Per-call timeout defaults to [`crate::DEFAULT_TIMEOUT`] (10 minutes) and can
//! be overridden with `UMADEV_WORKER_TIMEOUT`. If your codex CLI
//! is hanging (e.g. `codex login` hasn't completed), the call falls back
//! to the offline template after the timeout fires.
//!
//! Overridable for forward compatibility:
//!
//! - `UMADEV_CODEX_BIN`       — program name (default `codex`)
//! - `UMADEV_CODEX_EXEC_SUBCMD` — exec subcommand (default `exec`)

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use umadev_runtime::{
    BasePermissionProfile, CompletionRequest, CompletionResponse, Runtime, RuntimeError,
    RuntimeKind, Usage,
};

use crate::{
    default_workspace, merge_prompt, model_args, run_auth_status, run_subprocess,
    run_subprocess_streaming, AuthState, HostDriver, ProbeResult, PromptChannel, SubprocessCall,
};

/// Drives the `codex` CLI as a subprocess.
#[derive(Debug, Clone)]
pub struct CodexDriver {
    program: String,
    exec_subcmd: String,
    timeout: Duration,
    /// Permission posture for this legacy one-shot driver. Defaults to Plan so
    /// an omitted profile cannot silently create a writable subprocess.
    permissions: BasePermissionProfile,
    /// When `true`, a later `complete` may resume only the exact Codex thread id
    /// captured by this driver (or explicitly restored by its owner). With no
    /// pinned id the call is deliberately fresh: UmaDev never uses `--last`,
    /// because "most recent" may be a user's unrelated Codex conversation.
    continue_session: bool,
    /// Exact native Codex thread id. A successful fresh `codex exec --json`
    /// captures it from the authoritative `thread.started` event; subsequent
    /// calls resume that id explicitly. The lock is shared across sequential
    /// calls on one driver while [`Runtime::fork`] replaces it with a fresh cell.
    session_id: Arc<RwLock<Option<String>>>,
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
            permissions: BasePermissionProfile::Plan,
            continue_session: false,
            session_id: Arc::new(RwLock::new(None)),
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

    /// Builder form of [`HostDriver::set_session_id`] (mainly for tests and an
    /// explicit persisted-session restore).
    #[must_use]
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        // Replace the cell rather than mutating a shared clone: a fork must reset
        // only itself and must never erase its parent's captured thread id.
        let session_id = session_id.filter(|id| valid_codex_thread_id(id));
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
        if !valid_codex_thread_id(session_id) {
            return;
        }
        if let Ok(mut slot) = self.session_id.write() {
            *slot = Some(session_id.to_string());
        }
    }

    /// Argument vector for resuming one exact native Codex thread. UmaDev never
    /// emits `--last`; an absent id always means a brand-new `codex exec`.
    ///
    /// CRITICAL ordering: codex parses flags per-subcommand. Exec-parent flags
    /// (`--skip-git-repo-check`, `--sandbox`, `--color`, `--json`, `--dangerously-
    /// bypass-approvals-and-sandbox`) MUST come BEFORE the `resume` token —
    /// placed after it, codex's clap rejects them with "unexpected argument" and
    /// the whole resume call errors out. So resume = the full exec flag set
    /// (`base_args`, which already carries `--json` + the bypass) followed by
    /// `resume <thread-id>`. `--model` is appended at the call site (global flag).
    #[must_use]
    pub fn resume_args(&self, session_id: &str) -> Vec<String> {
        let mut args = self.base_args();
        args.push("resume".to_string());
        args.push(session_id.to_string());
        args
    }

    /// The full argument vector for a `complete` call. Continuation requires an
    /// exact native id; `continue_session=true` with no captured id is the first
    /// fresh call and never falls back to Codex's ambient `--last` selection.
    #[must_use]
    pub fn call_args(&self) -> Vec<String> {
        self.continue_session
            .then(|| self.pinned_session_id())
            .flatten()
            .map_or_else(|| self.base_args(), |id| self.resume_args(&id))
    }

    /// The argument vector preceding the prompt. Exposed for tests.
    ///
    /// Flag rationale:
    /// - `--skip-git-repo-check`: UmaDev workspaces are frequently
    ///   `output/` + `.umadev/` scratch dirs that aren't git repos;
    ///   codex otherwise refuses to run.
    /// - `--sandbox`: Plan is hard-pinned to `read-only`; Guarded uses
    ///   `workspace-write` unless `.umadevrc` restricts it. Emitted for every
    ///   profile EXCEPT Auto+full-access (which uses the bypass flag below instead).
    /// - `--dangerously-bypass-approvals-and-sandbox`: Auto only, and only when
    ///   the resolved sandbox is already `danger-full-access`. Emitted ALONE (it
    ///   disables the sandbox entirely, so a paired `--sandbox` is redundant and
    ///   clap-conflicts on some codex builds → exit 2). It is never allowed to erase
    ///   a project restriction. `UMADEV_NO_SKIP_PERMS=1` tightens Auto back to
    ///   ordinary host approvals (and thus back to an explicit `--sandbox`).
    /// - `--color never`: don't emit ANSI escape sequences that would
    ///   later need stripping. (`run_subprocess` strips them anyway,
    ///   but this is cleaner at the source.)
    #[must_use]
    pub fn base_args(&self) -> Vec<String> {
        self.base_args_for(std::env::var("UMADEV_NO_SKIP_PERMS").as_deref() == Ok("1"))
    }

    fn base_args_for(&self, no_skip: bool) -> Vec<String> {
        let sandbox = crate::codex_session::codex_sandbox_mode(self.permissions);
        self.base_args_with_sandbox(no_skip, sandbox)
    }

    fn base_args_with_sandbox(&self, no_skip: bool, sandbox: &str) -> Vec<String> {
        // A Plan driver has a second local fence even if a future caller hands
        // this helper an unsafe value. Explicit project restrictions also stay
        // effective in Auto: the dangerous bypass is only valid with the actual
        // full-access sandbox, otherwise it would silently nullify that override.
        let sandbox = if matches!(self.permissions, BasePermissionProfile::Plan) {
            "read-only"
        } else {
            sandbox
        };
        let effective_auto = self.permissions.auto_approve() && !no_skip;
        let bypass = effective_auto && sandbox == "danger-full-access";
        // Exec has no dedicated `--ask-for-approval` option, but its `-c`
        // override is authoritative over user/project config. This prevents a
        // local `approval_policy = "never"` from widening Plan/Guarded.
        let approval = if matches!(self.permissions, BasePermissionProfile::Plan) || effective_auto
        {
            "never"
        } else {
            "on-request"
        };
        let mut args = vec![
            self.exec_subcmd.clone(),
            "--skip-git-repo-check".to_string(),
        ];
        // Emit EXACTLY ONE of the two sandbox controls, never both.
        // `--dangerously-bypass-approvals-and-sandbox` disables the sandbox ENTIRELY,
        // so pairing it with `--sandbox danger-full-access` is at best redundant (the
        // bypass wins, the `--sandbox` value is ignored) and — on codex builds that put
        // the two flags in a clap `conflicts_with` group — a hard `exit 2` that drops
        // EVERY Auto one-shot to the offline scaffold. So Auto+full-access emits the
        // bypass flag ALONE; every other profile emits `--sandbox <mode>` as before.
        if bypass {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        } else {
            args.push("--sandbox".to_string());
            args.push(sandbox.to_string());
        }
        args.extend([
            "--config".to_string(),
            format!("approval_policy=\"{approval}\""),
            "--color".to_string(),
            "never".to_string(),
            // Emit newline-delimited JSON events so BOTH the streaming path AND
            // the non-streaming `complete` path can extract the real answer
            // (`agent_message`) instead of codex's human-readable banner/footer
            // ("OpenAI Codex vX … user … codex … tokens used"). Without this,
            // `complete` returned that whole banner as the "answer".
            "--json".to_string(),
        ]);
        args
    }
}

#[async_trait]
impl Runtime for CodexDriver {
    /// Concurrent-safe fork: clone with a FRESH session and an independent id
    /// cell, so a critic can neither resume nor erase the parent's native thread.
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
        // Codex streams (`--json`). `persistent_goal: true` means UmaDev FORWARDS a
        // `/goal` directive to the base when the user runs `/goal` (the intended
        // interaction). Codex has no native `/goal` slash command of its own (its
        // CLI is `codex exec`), so it reads the directive as a strong "keep working
        // until the objective is met" instruction — only Claude Code has a native
        // `/goal` mode. It has no PreToolUse hook (`realtime_governance: false` → the
        // after-turn governance scan applies).
        umadev_runtime::BrainCapabilities {
            persistent_goal: true,
            streaming: true,
            reports_usage: true,
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

        if let Some(session_id) = extract_codex_thread_id(&out.stdout) {
            self.remember_session_id(&session_id);
        }

        // base_args carries `--json`, so stdout is a JSONL event stream — extract
        // the `agent_message` text(s). Fall back to raw stdout only if extraction
        // yields nothing (so an unexpected format never silently empties the run).
        // Capture real token usage from the terminal `turn.completed` line
        // BEFORE `out.stdout` may be moved into `text` below.
        let usage = extract_codex_usage(&out.stdout);
        let mut text = extract_codex_messages(&out.stdout);
        if text.trim().is_empty() && !out.stdout.trim().is_empty() {
            text = out.stdout;
        }
        Ok(crate::redaction::sanitize_completion_response(
            &CompletionResponse {
                text,
                id: "codex-cli".to_string(),
                model: req.model,
                usage,
            },
        ))
    }

    /// Streaming completion via `codex exec --json`.
    ///
    /// Codex emits newline-delimited JSON events (verified against real
    /// `codex exec --json` output):
    /// - `{"type":"thread.started"}` / `{"type":"turn.started"}` — lifecycle,
    ///   skipped.
    /// - `{"type":"item.completed","item":{"type":"agent_message","text":"…"}}`
    ///   → [`umadev_runtime::StreamEvent::Text`].
    /// - `{"type":"item.completed","item":{"type":"command_execution","command":"sed …"}}`
    ///   → [`umadev_runtime::StreamEvent::ToolUse`] with name "Bash" + the command.
    /// - `{"type":"item.completed","item":{"type":"file_change",...}}`
    ///   → [`umadev_runtime::StreamEvent::ToolUse`] with name "Write" + the path.
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

        // Accumulate the raw stream so a mid-stream failure can salvage whatever
        // already arrived instead of cold-restarting a whole new run.
        let stream_buf = std::sync::Mutex::new(String::new());
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
                if let Ok(mut b) = stream_buf.lock() {
                    b.push_str(line);
                    b.push('\n');
                }
                if let Some(ev) = parse_codex_stream_line(line) {
                    on_event(ev);
                }
            },
        )
        .await;

        match result {
            Ok(out) => {
                if let Some(session_id) = extract_codex_thread_id(&out.stdout) {
                    self.remember_session_id(&session_id);
                }
                // Real token usage from the terminal `turn.completed` line
                // (captured before `out.stdout` may be moved into `final_text`).
                let usage = extract_codex_usage(&out.stdout);
                // Extract all agent_message texts from the JSONL stream.
                let mut final_text = extract_codex_messages(&out.stdout);
                if final_text.trim().is_empty() && !out.stdout.trim().is_empty() {
                    final_text = out.stdout;
                }
                Ok(crate::redaction::sanitize_completion_response(
                    &CompletionResponse {
                        text: final_text,
                        id: "codex-cli".to_string(),
                        model,
                        usage,
                    },
                ))
            }
            Err(e) => {
                // Routine self-healing (often the base being SIGTERM/SIGALRM'd —
                // exit 143/142 — by its own environment), so `debug!` not a scary
                // warning. Salvage what already streamed before a full cold restart.
                tracing::debug!(error = %e, "codex streaming failed, falling back");
                let partial = stream_buf.into_inner().unwrap_or_default();
                let salvaged = extract_codex_messages(&partial);
                if !salvaged.trim().is_empty() {
                    let usage = extract_codex_usage(&partial);
                    return Ok(crate::redaction::sanitize_completion_response(
                        &CompletionResponse {
                            text: salvaged,
                            id: "codex-cli".to_string(),
                            model,
                            usage,
                        },
                    ));
                }
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
    parse_codex_stream_line_raw(line).map(crate::redaction::sanitize_stream_event)
}

fn parse_codex_stream_line_raw(line: &str) -> Option<umadev_runtime::StreamEvent> {
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
            Some(umadev_runtime::StreamEvent::tool_use(name, detail))
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
            // codex's `file_change` normally carries only the path + kind — no
            // before/after content — so we degrade to a plain tool row (`edit:
            // None`). If a future codex build DOES expose the new content (an
            // `add` with `content`, or a `diff`/`unified_diff`), fill a ToolEdit
            // for the diff card; otherwise stay `None`. Fail-open: any missing
            // field just keeps `edit = None`.
            let edit = codex_file_change_edit(item, kind, &path);
            Some(umadev_runtime::StreamEvent::ToolUse {
                name: tool_name.to_string(),
                detail: path,
                edit,
            })
        }
        _ => None,
    }
}

/// Best-effort structured edit for a codex `file_change` item.
///
/// codex's stream usually reports a file change as just `{path, kind}` with no
/// content, so there's nothing to diff and this returns `None` (the caller
/// falls back to a plain `Write`/`Edit` row). It only produces a
/// [`umadev_runtime::ToolEdit`] when codex actually hands over the new file
/// content (an `add`/`create` carrying `content`/`new_content`) — a full-add
/// card. A unified-`diff`-only payload is intentionally left as `None` here:
/// we don't reconstruct before/after from a patch, so we never risk a wrong
/// card. **Fail-open:** any absent field yields `None`.
fn codex_file_change_edit(
    item: &serde_json::Value,
    kind: &str,
    path: &str,
) -> Option<umadev_runtime::ToolEdit> {
    if kind != "add" && kind != "create" {
        // An update without before/after content can't be diffed safely.
        return None;
    }
    let first = item
        .get("changes")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());
    // Look for the new file content on either the change entry or the item.
    let content = first
        .and_then(|ch| ch.get("content").or_else(|| ch.get("new_content")))
        .or_else(|| item.get("content").or_else(|| item.get("new_content")))
        .and_then(|c| c.as_str())?;
    if content.is_empty() {
        return None;
    }
    Some(umadev_runtime::ToolEdit {
        path: path.to_string(),
        before: String::new(),
        after: content.to_string(),
    })
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

/// Parse token usage from the codex `--json` JSONL stream.
///
/// The terminal `{"type":"turn.completed","usage":{"input_tokens":…,
/// "cached_input_tokens":…,"output_tokens":…,"reasoning_output_tokens":…}}`
/// line carries real usage (verified against live `codex exec --json` output).
/// Cached input and reasoning output are subsets of Codex's input/output totals,
/// so they are preserved as breakdowns but never added twice. If several
/// `turn.completed` lines appear, the LAST valid one wins. Returns incomplete
/// [`Usage::default`] when no usable line is present.
fn extract_codex_usage(stdout: &str) -> Usage {
    let mut usage = Usage::default();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("turn.completed") {
                if let Some(u) = v.get("usage") {
                    let required = |key: &str| u.get(key)?.as_u64();
                    let optional = |key: &str| -> Option<Option<u64>> {
                        match u.get(key) {
                            None | Some(serde_json::Value::Null) => Some(None),
                            Some(value) => value.as_u64().map(Some),
                        }
                    };
                    let Some(input_tokens) = required("input_tokens") else {
                        continue;
                    };
                    let Some(output_tokens) = required("output_tokens") else {
                        continue;
                    };
                    let Some(cached_read_tokens) = optional("cached_input_tokens") else {
                        continue;
                    };
                    let Some(reasoning_tokens) = optional("reasoning_output_tokens") else {
                        continue;
                    };
                    let Some(total_tokens) = input_tokens.checked_add(output_tokens) else {
                        continue;
                    };
                    if cached_read_tokens.unwrap_or(0) > input_tokens
                        || reasoning_tokens.unwrap_or(0) > output_tokens
                        || optional("total_tokens")
                            .is_none_or(|value| value.is_some_and(|total| total != total_tokens))
                    {
                        continue;
                    }
                    usage = Usage {
                        cached_read_tokens: cached_read_tokens.unwrap_or(0),
                        reasoning_tokens: reasoning_tokens.unwrap_or(0),
                        ..Usage::exact(input_tokens, output_tokens)
                    };
                }
            }
        }
    }
    usage
}

/// Capture the exact native thread id emitted by `codex exec --json`.
/// Workspace recency and caller-generated UUIDs are never guessed as Codex
/// threads; only the base's authoritative `thread.started` event can mint the
/// continuation pointer.
fn extract_codex_thread_id(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(codex_thread_id_from_line)
        .next_back()
}

fn codex_thread_id_from_line(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("thread.started") {
        return None;
    }
    let id = value.get("thread_id").and_then(serde_json::Value::as_str)?;
    valid_codex_thread_id(id).then(|| id.to_string())
}

fn valid_codex_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Extract all `agent_message` texts from a codex `--json` JSONL stream.
fn codex_message_from_line(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("item.completed") {
        return None;
    }
    let item = value.get("item")?;
    if item.get("type").and_then(serde_json::Value::as_str) != Some("agent_message") {
        return None;
    }
    item.get("text")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn extract_codex_messages(stdout: &str) -> String {
    stdout
        .lines()
        .filter(|line| line.trim_start().starts_with('{'))
        .filter_map(codex_message_from_line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl HostDriver for CodexDriver {
    fn backend_id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex CLI"
    }

    fn permission_profile(&self) -> BasePermissionProfile {
        self.permissions
    }

    fn set_continue_session(&mut self, continue_session: bool) {
        self.continue_session = continue_session;
    }

    fn set_session_id(&mut self, session_id: Option<String>) {
        let session_id = session_id.filter(|id| valid_codex_thread_id(id));
        self.session_id = Arc::new(RwLock::new(session_id));
    }

    fn set_workspace(&mut self, workspace: std::path::PathBuf) {
        self.workspace = Some(workspace);
    }

    fn install_hint(&self) -> Option<&'static str> {
        Some("npm install -g @openai/codex")
    }

    fn login_hint(&self) -> Option<&'static str> {
        Some("codex login")
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
            // Installed — resolve the honest auth state too (gap G10).
            Ok(out) => ProbeResult::Ready {
                version: out.stdout.lines().next().unwrap_or("unknown").to_string(),
                auth_state: self.probe_auth().await,
            },
            Err(e) if e.contains("not found on PATH") => ProbeResult::NotInstalled {
                program: self.program.clone(),
            },
            Err(e) => ProbeResult::Unhealthy { detail: e },
        }
    }

    /// Cheapest authenticated no-op for Codex, in cost order — NO real
    /// generation, NO tokens.
    ///
    /// Codex's official auth paths (see the Codex Authentication docs): a cached
    /// credential file at `$CODEX_HOME/auth.json` (defaults to `~/.codex`), OR
    /// the `OPENAI_API_KEY` env var.
    ///
    /// 1. **`OPENAI_API_KEY` env** (instant): codex uses it when present.
    /// 2. **`auth.json` exists** under `CODEX_HOME` (default `~/.codex`): the file
    ///    `codex login` writes — its presence means a stored login. (Verified
    ///    against the live file: keys `OPENAI_API_KEY` / `tokens` / `auth_mode`.)
    /// 3. **Authoritative subcommand** `codex login status` — prints "Logged in
    ///    using ChatGPT" (exit 0) when logged in (confirmed via `codex login
    ///    --help` → `status` subcommand + live output). Used as the cross-check
    ///    / fallback; bounded by the short auth-probe timeout.
    ///
    /// Fail-open: anything indeterminate → [`AuthState::Unknown`].
    async fn probe_auth(&self) -> AuthState {
        // 1. API-key env — definitive and instant.
        if crate::any_env_set(&["OPENAI_API_KEY"]) {
            return AuthState::LoggedIn;
        }
        // 2. Authoritative subcommand — the ONLY evidence of a LIVE login. `codex login
        //    status` exits 0 + prints "Logged in …" when authenticated. This runs BEFORE
        //    any credential-file check: a stale / expired / revoked `$CODEX_HOME/auth.json`
        //    still sits on disk long after it stopped working, so trusting mere file
        //    presence (as this did before) showed a green "logged in" that failed at the
        //    first real turn — violating the "never a false LoggedIn" contract. `false` =
        //    capture the output regardless of exit code, so the non-zero "Not logged in"
        //    codex returns when logged out is classified, not swallowed into Unknown.
        if let Some(out) = run_auth_status(
            &self.program,
            &["login".to_string(), "status".to_string()],
            false,
        )
        .await
        {
            let lower = out.to_ascii_lowercase();
            if lower.contains("not logged in") || lower.contains("not authenticated") {
                return AuthState::NotLoggedIn;
            } else if lower.contains("logged in") {
                return AuthState::LoggedIn;
            }
        }
        // 3. The status subcommand was indeterminate (missing/renamed on an older codex,
        //    or timed out). A stored `auth.json` is a WEAK, stale-prone signal — it proves
        //    a login was once configured, never that it is live — so we stay Unknown
        //    (fail-open), never a false LoggedIn. The picker then shows "installed, auth
        //    unknown" rather than a green light that lies.
        AuthState::Unknown
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
            StreamEvent::ToolUse { name, detail, .. } => {
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
            StreamEvent::ToolUse { name, detail, edit } => {
                assert_eq!(name, "Edit", "kind=update should map to Edit");
                assert_eq!(detail, "/tmp/test.txt");
                // codex's file_change is path-only → no diff card, degrade to row.
                assert!(
                    edit.is_none(),
                    "a path-only codex file_change must NOT fabricate a diff"
                );
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
            StreamEvent::ToolUse { name, detail, edit } => {
                assert_eq!(name, "Write", "kind=add should map to Write");
                assert_eq!(detail, "src/new.ts");
                // A path-only `add` (no content) still degrades to a plain row.
                assert!(edit.is_none(), "no content → no diff card");
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
    fn parse_file_change_add_with_content_fills_edit() {
        // Forward-compat: IF codex ever exposes the new file content on an `add`,
        // we fill an all-additions ToolEdit for the diff card.
        let line = r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"src/new.ts","kind":"add","content":"export const x = 1;\n"}]}}"#;
        let ev = parse_codex_stream_line(line).expect("parse");
        let StreamEvent::ToolUse { name, edit, .. } = ev else {
            panic!("expected ToolUse");
        };
        assert_eq!(name, "Write");
        let edit = edit.expect("an add carrying content should fill a ToolEdit");
        assert_eq!(edit.path, "src/new.ts");
        assert!(edit.before.is_empty());
        assert_eq!(edit.after, "export const x = 1;\n");
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
    fn extract_codex_usage_reads_tokens_from_turn_completed() {
        // Official Codex semantics: cached_input_tokens is already included in
        // input_tokens and reasoning_output_tokens is already included in
        // output_tokens. These captured values must therefore remain 31_751 / 2_367,
        // not be double-counted as 46_471 / 2_780.
        let stdout = concat!(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"PONG"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":31751,"cached_input_tokens":14720,"output_tokens":2367,"reasoning_output_tokens":413,"total_tokens":34118}}"#,
        );
        let u = extract_codex_usage(stdout);
        assert_eq!(u.input_tokens, 31_751);
        assert_eq!(u.output_tokens, 2_367);
        assert_eq!(u.total_tokens, 34_118);
        assert_eq!(u.cached_read_tokens, 14_720);
        assert_eq!(u.cached_write_tokens, 0);
        assert_eq!(u.reasoning_tokens, 413);
        assert!(!u.usage_incomplete);

        // Missing or malformed terminal usage is unknown, never an exact zero.
        for invalid in [
            "plain text",
            r#"{"type":"turn.started"}"#,
            r#"{"type":"turn.completed","usage":{}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":2,"total_tokens":99}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"cached_input_tokens":6,"output_tokens":2}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":2,"reasoning_output_tokens":3}}"#,
        ] {
            let usage = extract_codex_usage(invalid);
            assert_eq!(usage, Usage::default(), "invalid fixture: {invalid}");
        }
    }

    #[test]
    fn extract_codex_usage_last_turn_wins() {
        // A resumed multi-turn run can emit several turn.completed lines; the
        // last (cumulative) one wins.
        let stdout = concat!(
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":250,"output_tokens":30}}"#,
        );
        let u = extract_codex_usage(stdout);
        assert_eq!(u.input_tokens, 250);
        assert_eq!(u.output_tokens, 30);
    }

    #[test]
    fn capabilities_reports_usage_and_streaming() {
        // Codex now parses usage off turn.completed, so it must declare it.
        let caps = CodexDriver::default().capabilities();
        assert!(caps.reports_usage, "codex parses usage → must report it");
        assert!(caps.streaming, "codex --json streams");
        assert!(
            caps.persistent_goal,
            "codex supports a persistent /goal mode"
        );
        assert!(!caps.realtime_governance);
    }

    #[test]
    fn defaults_are_sane() {
        let d = CodexDriver::default();
        assert_eq!(d.backend_id(), "codex");
        assert_eq!(d.display_name(), "Codex CLI");
        assert_eq!(d.kind(), RuntimeKind::Openai);
        assert_eq!(d.permission_profile(), BasePermissionProfile::Plan);
    }

    #[test]
    fn permission_profiles_shape_legacy_args_and_no_skip_only_tightens() {
        let cases = [
            (BasePermissionProfile::Plan, "read-only", "never", false),
            (
                BasePermissionProfile::Guarded,
                "danger-full-access",
                "on-request",
                false,
            ),
            (
                BasePermissionProfile::Auto,
                "danger-full-access",
                "never",
                true,
            ),
        ];
        for (profile, sandbox, approval, bypass) in cases {
            let args = CodexDriver::default()
                .with_permissions(profile)
                .base_args_with_sandbox(false, sandbox);
            let has_sandbox = args.windows(2).any(|w| w[0] == "--sandbox");
            let has_bypass = args
                .iter()
                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox");
            // EXACTLY ONE sandbox control, never both (some codex builds clap-conflict
            // on the pair → exit 2 → offline scaffold).
            assert!(
                has_sandbox ^ has_bypass,
                "profile {profile:?} must emit exactly one of --sandbox / bypass: {args:?}"
            );
            if bypass {
                assert!(
                    has_bypass && !has_sandbox,
                    "Auto+full-access emits the bypass flag ALONE, no redundant --sandbox: {args:?}"
                );
            } else {
                assert!(
                    args.windows(2)
                        .any(|w| w[0] == "--sandbox" && w[1] == sandbox),
                    "profile {profile:?} emits --sandbox {sandbox}: {args:?}"
                );
                assert!(
                    !has_bypass,
                    "profile {profile:?} emits no bypass flag: {args:?}"
                );
            }
            assert!(args
                .iter()
                .any(|a| a == &format!("approval_policy=\"{approval}\"")));
        }

        let tightened = CodexDriver::default()
            .with_permissions(BasePermissionProfile::Auto)
            .base_args_with_sandbox(true, "danger-full-access");
        assert!(tightened
            .iter()
            .any(|a| a == "approval_policy=\"on-request\""));
        assert!(!tightened
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));

        let hostile_override = CodexDriver::default()
            .with_permissions(BasePermissionProfile::Plan)
            .base_args_with_sandbox(false, "danger-full-access");
        assert!(hostile_override
            .windows(2)
            .any(|w| w[0] == "--sandbox" && w[1] == "read-only"));
        assert!(!hostile_override
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));

        let restricted_auto = CodexDriver::default()
            .with_permissions(BasePermissionProfile::Auto)
            .base_args_with_sandbox(false, "workspace-write");
        assert!(restricted_auto
            .iter()
            .any(|a| a == "approval_policy=\"never\""));
        assert!(!restricted_auto
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn continuation_is_fresh_until_an_exact_native_thread_is_known() {
        // Fresh: a normal `codex exec ...` (no resume).
        let fresh = CodexDriver::default();
        let args = fresh.call_args();
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(!args.contains(&"resume".to_string()));

        // `continue=true` without an exact id is still fresh. It must never select
        // Codex's ambient most-recent conversation.
        let mut first = CodexDriver::default();
        first.set_continue_session(true);
        let args = first.call_args();
        assert!(!args.contains(&"resume".to_string()));
        assert!(!args.contains(&"--last".to_string()));

        // A corrupt persisted/caller id cannot smuggle Codex's ambient selector
        // back in through the positional thread-id slot.
        let mut forged = CodexDriver::default().with_session_id(Some("--last".to_string()));
        forged.set_continue_session(true);
        let args = forged.call_args();
        assert!(!args.contains(&"resume".to_string()));
        assert!(!args.contains(&"--last".to_string()));

        // Once Codex itself reports a native thread id, continuation is exact:
        // `codex exec <exec-parent flags> resume <id> …`.
        // CRITICAL: every exec-parent flag (--skip-git-repo-check / --sandbox /
        // --color / --json) MUST come BEFORE the `resume` token, or codex's clap
        // rejects it with "unexpected argument" and the resume call errors out.
        let mut resumed = CodexDriver::default()
            .with_session_id(Some("019bf92c-6a90-76e1-8f84-40d4abc6e840".to_string()));
        resumed.set_continue_session(true);
        let args = resumed.call_args();
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        let resume_idx = args
            .iter()
            .position(|a| a == "resume")
            .expect("resume args contain `resume`");
        assert_eq!(
            args.get(resume_idx + 1).map(String::as_str),
            Some("019bf92c-6a90-76e1-8f84-40d4abc6e840")
        );
        assert!(!args.contains(&"--last".to_string()));
        for flag in ["--skip-git-repo-check", "--sandbox", "--color", "--json"] {
            let idx = args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("resume args missing {flag}: {args:?}"));
            assert!(idx < resume_idx, "{flag} must precede `resume`: {args:?}");
        }
    }

    #[test]
    fn thread_started_is_the_only_continuation_id_authority() {
        let stream = concat!(
            "not json\n",
            "{\"type\":\"item.completed\",\"thread_id\":\"wrong\"}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"thr-old-project\"}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"../../escape\"}\n"
        );
        assert_eq!(
            extract_codex_thread_id(stream).as_deref(),
            Some("thr-old-project")
        );
        assert_eq!(
            extract_codex_thread_id("{\"type\":\"thread.started\"}"),
            None
        );
    }

    #[test]
    fn a_fork_never_inherits_or_clears_the_parent_thread() {
        let parent = CodexDriver::default()
            .with_continue_session(true)
            .with_session_id(Some("thr-parent".to_string()));
        let child = parent
            .clone()
            .with_continue_session(false)
            .with_session_id(None);
        assert_eq!(parent.pinned_session_id().as_deref(), Some("thr-parent"));
        assert!(child.pinned_session_id().is_none());
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
        let probe = d.probe().await;
        assert!(matches!(probe, ProbeResult::NotInstalled { .. }));
        // NotInstalled auth state — distinct from NotLoggedIn, never LoggedIn.
        assert_eq!(probe.auth_state(), AuthState::NotInstalled);
        assert!(!probe.is_ready_and_authed());
    }

    #[test]
    fn install_and_login_hints_are_actionable() {
        let d = CodexDriver::default();
        assert!(d.install_hint().unwrap().contains("codex"));
        assert_eq!(d.login_hint().unwrap(), "codex login");
    }

    struct EnvRestore {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                prev: std::env::var_os(key),
            }
        }

        fn set<P: AsRef<std::ffi::OsStr>>(&self, value: P) {
            std::env::set_var(self.key, value);
        }

        fn remove(&self) {
            std::env::remove_var(self.key);
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prev.as_ref() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    // The `auth.json` existence path: an empty CODEX_HOME (no file) + no
    // OPENAI_API_KEY + a missing binary → the only signals are absent, so a
    // missing-binary `login status` fails → Unknown (fail-open, never LoggedIn).
    #[tokio::test]
    async fn probe_auth_unknown_when_no_creds_and_status_cmd_missing() {
        let _g = crate::AUTH_ENV_TEST_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let codex_home = EnvRestore::capture("CODEX_HOME");
        let openai_key = EnvRestore::capture("OPENAI_API_KEY");
        codex_home.set(tmp.path()); // empty → no auth.json
        openai_key.remove();
        let d = CodexDriver::with_program("umadev-fake-codex-xyz");
        let state = d.probe_auth().await;
        assert_eq!(
            state,
            AuthState::Unknown,
            "no creds + no status command must fail-open to Unknown, not LoggedIn"
        );
    }

    // #8 FIX: a stale / expired / revoked `auth.json` still sits on disk long after
    // it stopped working. Mere file presence must NOT read as LoggedIn — only the
    // authoritative `codex login status` proves a LIVE login. With the file present
    // but no OPENAI_API_KEY and a missing status binary, probe stays Unknown, never a
    // false green "logged in" that fails at the first real turn.
    #[tokio::test]
    async fn probe_auth_does_not_trust_a_stale_auth_json_file() {
        let _g = crate::AUTH_ENV_TEST_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let codex_home = EnvRestore::capture("CODEX_HOME");
        let openai_key = EnvRestore::capture("OPENAI_API_KEY");
        codex_home.set(tmp.path());
        openai_key.remove();
        // A leftover credential file — the exact stale-login case the old
        // file-presence short-circuit mis-read as LoggedIn.
        std::fs::write(tmp.path().join("auth.json"), "{\"tokens\":{}}").unwrap();
        let d = CodexDriver::with_program("umadev-fake-codex-xyz");
        assert_eq!(
            d.probe_auth().await,
            AuthState::Unknown,
            "a stale auth.json on disk must never be trusted as a live login"
        );
    }

    // `OPENAI_API_KEY` short-circuits to LoggedIn instantly (no subprocess), even
    // with an empty CODEX_HOME and a missing binary.
    #[tokio::test]
    async fn probe_auth_logged_in_via_openai_api_key() {
        let _g = crate::AUTH_ENV_TEST_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let codex_home = EnvRestore::capture("CODEX_HOME");
        let openai_key = EnvRestore::capture("OPENAI_API_KEY");
        codex_home.set(tmp.path());
        openai_key.set("sk-test");
        let d = CodexDriver::with_program("umadev-fake-codex-xyz");
        let state = d.probe_auth().await;
        assert_eq!(state, AuthState::LoggedIn);
    }

    // The fake codex is a `#!/bin/sh` script, which Windows cannot exec; the
    // JSONL parsing it exercises is covered by the unit tests above.
    #[cfg(unix)]
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

    #[test]
    fn stream_events_redact_synthetic_secrets() {
        const SECRET: &str = "SYNTH_CODEX_SECRET_DO_NOT_LEAK_72";
        let text = serde_json::json!({
            "type": "item.completed",
            "item": {"type": "agent_message", "text": format!("password={SECRET}")}
        })
        .to_string();
        let tool = serde_json::json!({
            "type": "item.completed",
            "item": {
                "type": "command_execution",
                "command": format!("curl -H 'Authorization: Bearer {SECRET}' example.test")
            }
        })
        .to_string();
        let rendered = format!(
            "{:?}{:?}",
            parse_codex_stream_line(&text),
            parse_codex_stream_line(&tool)
        );
        assert!(
            !rendered.contains(SECRET),
            "stream event leaked: {rendered}"
        );
    }
}
