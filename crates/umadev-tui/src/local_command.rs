//! Bounded one-shot commands launched from the TUI.
//!
//! Local shell (`!cmd`) and UmaDev helper commands share this path so none of
//! them can block the render loop, retain unbounded output, or leave a process
//! tree behind after timeout/cancellation.
//!
//! These tasks, and a confirmed `/deploy`, never touch the resident base
//! session. Esc/Ctrl+C/`/cancel` therefore stops only the task through its
//! stop handle (see [`crate::app::App::stop_local_task`]); the task settles
//! through its own terminal result instead of the chat-turn cancel, which
//! would tear down the base session and record a cancelled request.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use umadev_agent::{ChannelSink, EngineEvent, EventSink as _};
use umadev_process::{BoundedCommandOptions, BoundedCommandOutput};

use crate::app::{App, LocalCommandPresentation};
use crate::RouteDecision;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// `!cmd` runs installs, builds and test suites, which routinely take minutes.
/// The user can stop one sooner with Esc/Ctrl+C.
const SHELL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const OUTPUT_BYTES: usize = 256 * 1024;
const MAX_DISPLAY_LINES: usize = 300;
const MAX_DISPLAY_CHARS: usize = 16_000;
const READER_GRACE: Duration = Duration::from_secs(1);

/// Immutable command snapshot handed from the UI model to the async executor.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct LocalCommandRequest {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) display: String,
    pub(crate) presentation: LocalCommandPresentation,
    pub(crate) timeout: Duration,
}

impl LocalCommandRequest {
    /// Build the platform shell invocation for an explicit `!cmd` request.
    pub(crate) fn shell(root: &Path, command: &str) -> Self {
        #[cfg(windows)]
        let (program, args) = (
            "cmd.exe".to_string(),
            vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                command.to_string(),
            ],
        );
        #[cfg(not(windows))]
        let (program, args) = (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        );
        Self {
            program,
            args,
            cwd: root.to_path_buf(),
            display: command.to_string(),
            presentation: LocalCommandPresentation::Shell,
            timeout: SHELL_TIMEOUT,
        }
    }

    /// Build a non-interactive invocation of the current UmaDev executable.
    pub(crate) fn umadev(
        root: &Path,
        args: &[&str],
        presentation: LocalCommandPresentation,
    ) -> Self {
        let program = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("umadev"))
            .to_string_lossy()
            .into_owned();
        let owned_args = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        let display = std::iter::once("umadev".to_string())
            .chain(owned_args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            program,
            args: owned_args,
            cwd: root.to_path_buf(),
            display,
            presentation,
            timeout: COMMAND_TIMEOUT,
        }
    }
}

/// Whether a local command must wait for the task that owns the event-loop
/// slot. A finished handle does not count: a legacy engine block reports its
/// end only through engine events, so nothing ever clears its handle.
pub(crate) fn slot_busy(run_task: Option<&tokio::task::JoinHandle<()>>, app: &App) -> bool {
    run_task.is_some_and(|task| !task.is_finished()) || app.thinking || app.cancelling
}

/// Stop handle for the running TUI-local task (see the module docs). Clones
/// share one handle, so the task is asked to stop at most once.
#[derive(Debug, Clone)]
pub(crate) struct LocalTaskStop(Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>);

impl LocalTaskStop {
    pub(crate) fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        (Self(Arc::new(std::sync::Mutex::new(Some(stop)))), stopped)
    }

    /// Ask the task to stop. `false` when it was already asked or has ended.
    pub(crate) fn request(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some_and(|stop| stop.send(()).is_ok())
    }
}

/// Start a local command that reports through
/// [`RouteDecision::LocalCommandDone`] and can be stopped on its own.
pub(crate) fn spawn(
    app: &mut App,
    request: LocalCommandRequest,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) -> tokio::task::JoinHandle<()> {
    app.begin_local_command(&request);
    let stopped = app.register_local_task();
    let lang = app.lang;
    let route_tx = route_tx.clone();
    tokio::spawn(async move {
        let result = run_until_stopped(request, lang, stopped).await;
        let _ = route_tx.send(RouteDecision::LocalCommandDone(result));
    })
}

async fn run_until_stopped(
    request: LocalCommandRequest,
    lang: umadev_i18n::Lang,
    stopped: tokio::sync::oneshot::Receiver<()>,
) -> LocalCommandResult {
    let stopped_request = request.clone();
    // Dropping `run` terminates the command's whole process tree.
    tokio::select! {
        result = run(request, lang) => result,
        Ok(()) = stopped => LocalCommandResult {
            request: stopped_request,
            ok: false,
            output: umadev_i18n::t(lang, "tui.local.cancelled").to_string(),
        },
    }
}

/// Start a confirmed `/deploy` that reports through
/// [`RouteDecision::DeployDone`] and can be stopped on its own.
pub(crate) fn spawn_deploy(
    app: &mut App,
    command: String,
    root: PathBuf,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) -> tokio::task::JoinHandle<()> {
    app.begin_deploy();
    let stopped = app.register_local_task();
    tokio::spawn(async move {
        sink.emit(EngineEvent::Note(umadev_i18n::tlf(
            "deploy.running",
            &[&command],
        )));
        let login_hint = umadev_i18n::tl("deploy.login_hint");
        // Dropping the deploy future terminates the deploy's process tree.
        let proof = tokio::select! {
            proof = umadev_agent::run_deploy(&root, Some(&command)) => proof,
            Ok(()) = stopped => {
                sink.emit(EngineEvent::Note(umadev_i18n::tl("deploy.cancelled").to_string()));
                let _ = route_tx.send(RouteDecision::DeployDone { succeeded: false });
                return;
            }
        };
        let succeeded = matches!(&proof.status, umadev_agent::DeployStatus::Deployed);
        match &proof.status {
            umadev_agent::DeployStatus::Deployed => {
                let address = proof
                    .url
                    .clone()
                    .unwrap_or_else(|| umadev_i18n::tl("deploy.done_no_url").into());
                sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "deploy.done",
                    &[&address],
                )));
            }
            umadev_agent::DeployStatus::NotDeployed(reason) => {
                let exit = proof
                    .exit_code
                    .map_or_else(|| "-".to_string(), |code| code.to_string());
                sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "deploy.failed",
                    &[&exit, reason, login_hint],
                )));
            }
        }
        if let Ok(path) = umadev_agent::write_deploy_proof(&root, &proof) {
            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                "deploy.proof_written",
                &[&path.display().to_string()],
            )));
        }
        let _ = route_tx.send(RouteDecision::DeployDone { succeeded });
    })
}

/// Terminal result sent back through the route-decision channel.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct LocalCommandResult {
    pub(crate) request: LocalCommandRequest,
    pub(crate) ok: bool,
    pub(crate) output: String,
}

/// Execute one request outside the render loop with a hard resource envelope.
pub(crate) async fn run(
    request: LocalCommandRequest,
    lang: umadev_i18n::Lang,
) -> LocalCommandResult {
    let mut command = tokio::process::Command::new(&request.program);
    command.args(&request.args).current_dir(&request.cwd);
    let options = BoundedCommandOptions {
        timeout: request.timeout,
        stdout_bytes: OUTPUT_BYTES,
        stderr_bytes: OUTPUT_BYTES,
        reader_grace: READER_GRACE,
    };
    let (ok, output) = match umadev_process::run_bounded_command(command, options).await {
        Ok(output) => format_output(&output, lang),
        Err(error) => (
            false,
            umadev_i18n::tf(lang, "tui.bang.spawn_failed", &[&error.to_string()]),
        ),
    };
    let output = wrap_output(request.presentation, output, lang);
    LocalCommandResult {
        request,
        ok,
        output,
    }
}

fn format_output(output: &BoundedCommandOutput, lang: umadev_i18n::Lang) -> (bool, String) {
    let mut body = String::new();
    let truncation_notice = (output.stdout_truncated || output.stderr_truncated)
        .then(|| umadev_i18n::t(lang, "tui.local.output_truncated"));
    body.push_str(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    let ok = output
        .status
        .as_ref()
        .is_some_and(std::process::ExitStatus::success)
        && !output.timed_out;
    if output.timed_out {
        if !body.trim().is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(umadev_i18n::t(lang, "tui.bang.timeout"));
    } else if !ok {
        if !body.trim().is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&match output
            .status
            .as_ref()
            .and_then(std::process::ExitStatus::code)
        {
            Some(code) => umadev_i18n::tf(lang, "tui.bang.exit", &[&code.to_string()]),
            None => umadev_i18n::t(lang, "tui.bang.failed").to_string(),
        });
    }
    let body = bound_display_tail_with_notice(&body, truncation_notice);
    if body.trim().is_empty() {
        (ok, umadev_i18n::t(lang, "tui.bang.no_output").to_string())
    } else {
        (ok, body)
    }
}

fn bound_display_tail_with_notice(body: &str, notice: Option<&str>) -> String {
    let Some(notice) = notice.filter(|notice| !notice.is_empty()) else {
        return bound_display_tail(body);
    };
    let notice_chars = notice.chars().count().min(MAX_DISPLAY_CHARS);
    let notice = notice.chars().take(notice_chars).collect::<String>();
    if notice_chars == MAX_DISPLAY_CHARS || body.is_empty() {
        return notice;
    }

    let body_lines = MAX_DISPLAY_LINES.saturating_sub(notice.lines().count());
    let body_chars = MAX_DISPLAY_CHARS.saturating_sub(notice_chars + 1);
    let tail = bound_display_tail_to(body, body_lines, body_chars);
    if tail.is_empty() {
        notice
    } else {
        format!("{notice}\n{tail}")
    }
}

fn wrap_output(
    presentation: LocalCommandPresentation,
    output: String,
    lang: umadev_i18n::Lang,
) -> String {
    match presentation {
        LocalCommandPresentation::Mcp => umadev_i18n::tf(lang, "slash.mcp_header", &[&output]),
        LocalCommandPresentation::Skill => umadev_i18n::tf(lang, "slash.skill_header", &[&output]),
        LocalCommandPresentation::Shell | LocalCommandPresentation::UmaDev => output,
    }
}

/// Keep the newest useful tail while retaining a hard transcript storage cap.
fn bound_display_tail(body: &str) -> String {
    bound_display_tail_to(body, MAX_DISPLAY_LINES, MAX_DISPLAY_CHARS)
}

fn bound_display_tail_to(body: &str, max_lines: usize, max_chars: usize) -> String {
    if max_lines == 0 || max_chars == 0 {
        return String::new();
    }
    let lines = body.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    let by_lines = lines[start..].join("\n");
    let chars = by_lines.chars().collect::<Vec<_>>();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_bound_keeps_the_newest_lines_and_chars() {
        let body = (0..500)
            .map(|line| format!("line-{line:04}-{}", "x".repeat(80)))
            .collect::<Vec<_>>()
            .join("\n");
        let bounded = bound_display_tail(&body);
        assert!(bounded.contains("line-0499"));
        assert!(!bounded.contains("line-0000"));
        assert!(bounded.chars().count() <= MAX_DISPLAY_CHARS);
        assert!(bounded.lines().count() <= MAX_DISPLAY_LINES);
    }

    #[test]
    fn truncation_notice_survives_the_display_tail_cap() {
        let output = BoundedCommandOutput {
            status: None,
            timed_out: false,
            stdout: vec![b'x'; MAX_DISPLAY_CHARS * 2],
            stderr: Vec::new(),
            stdout_truncated: true,
            stderr_truncated: false,
        };
        let (_, rendered) = format_output(&output, umadev_i18n::Lang::En);
        let notice = umadev_i18n::t(umadev_i18n::Lang::En, "tui.local.output_truncated");
        assert!(rendered.starts_with(notice));
        assert!(rendered.chars().count() <= MAX_DISPLAY_CHARS);
        assert!(rendered.lines().count() <= MAX_DISPLAY_LINES);
        assert!(rendered.ends_with(umadev_i18n::t(umadev_i18n::Lang::En, "tui.bang.failed")));
    }

    #[test]
    fn shell_commands_get_a_budget_for_installs_and_test_runs() {
        let root = tempfile::tempdir().unwrap();
        let request = LocalCommandRequest::shell(root.path(), "npm install");
        assert_eq!(request.timeout, Duration::from_secs(600));
    }

    #[tokio::test]
    async fn a_finished_task_does_not_hold_the_local_command_slot() {
        let root = tempfile::tempdir().unwrap();
        let app = App::new(
            "slot",
            crate::config::UserConfig::default(),
            root.path().join("config.toml"),
            root.path().to_path_buf(),
        );
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(!slot_busy(Some(&finished), &app));
        assert!(!slot_busy(None, &app));

        let running = tokio::spawn(std::future::pending::<()>());
        assert!(slot_busy(Some(&running), &app));
        running.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_command_ends_promptly_and_reports_the_stop() {
        let root = tempfile::tempdir().unwrap();
        let request = LocalCommandRequest::shell(root.path(), "sleep 30");
        let (stop, stopped) = LocalTaskStop::new();
        let started = std::time::Instant::now();
        let task = tokio::spawn(run_until_stopped(request, umadev_i18n::Lang::En, stopped));
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(stop.request());
        assert!(!stop.request(), "a stop is requested once");
        let result = task.await.unwrap();
        assert!(!result.ok);
        assert_eq!(
            result.output,
            umadev_i18n::t(umadev_i18n::Lang::En, "tui.local.cancelled")
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_is_bounded_and_reported() {
        let root = tempfile::tempdir().unwrap();
        let mut request =
            LocalCommandRequest::shell(root.path(), "printf partial-marker; sleep 30");
        request.timeout = Duration::from_millis(50);
        let started = std::time::Instant::now();
        let result = run(request, umadev_i18n::Lang::En).await;
        assert!(!result.ok);
        assert!(result.output.contains("partial-marker"));
        assert!(result.output.to_ascii_lowercase().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
