//! Bounded one-shot commands launched from the TUI.
//!
//! Local shell (`!cmd`) and UmaDev helper commands share this path so none of
//! them can block the render loop, retain unbounded output, or leave a process
//! tree behind after timeout/cancellation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use umadev_process::{BoundedCommandOptions, BoundedCommandOutput};

use crate::app::LocalCommandPresentation;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
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
