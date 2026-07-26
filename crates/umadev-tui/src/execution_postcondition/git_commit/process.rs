use super::{
    git_commit_blocked, git_output, git_tokio_command, Duration, GitTransactionGuard, Path,
    ResidentExecutionBlocked,
};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MUTATION_OUTPUT_LIMIT: usize = 64 * 1024;
const MUTATION_REAP_GRACE: Duration = Duration::from_secs(1);

pub(crate) async fn git_mutating_output(
    root: &Path,
    args: &[&str],
    paths: &[&str],
    timeout: Duration,
    timeout_code: &'static str,
    command_label: &str,
    transaction: &mut GitTransactionGuard,
) -> Result<std::process::Output, ResidentExecutionBlocked> {
    git_mutating_output_inner(
        root,
        args,
        paths,
        GitMutationInvocation {
            input: None,
            timeout,
            timeout_code,
            command_label,
        },
        transaction,
    )
    .await
}

pub(crate) async fn git_mutating_output_with_input(
    root: &Path,
    args: &[&str],
    input: &[u8],
    timeout: Duration,
    timeout_code: &'static str,
    command_label: &str,
    transaction: &mut GitTransactionGuard,
) -> Result<std::process::Output, ResidentExecutionBlocked> {
    git_mutating_output_inner(
        root,
        args,
        &[],
        GitMutationInvocation {
            input: Some(input),
            timeout,
            timeout_code,
            command_label,
        },
        transaction,
    )
    .await
}

struct GitMutationInvocation<'a> {
    input: Option<&'a [u8]>,
    timeout: Duration,
    timeout_code: &'static str,
    command_label: &'a str,
}

async fn git_mutating_output_inner(
    root: &Path,
    args: &[&str],
    paths: &[&str],
    invocation: GitMutationInvocation<'_>,
    transaction: &mut GitTransactionGuard,
) -> Result<std::process::Output, ResidentExecutionBlocked> {
    let GitMutationInvocation {
        input,
        timeout,
        timeout_code,
        command_label,
    } = invocation;
    let mut command = git_tokio_command(root);
    command
        .env("GIT_REFLOG_ACTION", transaction.reflog_action())
        .args(args)
        .args(paths)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = umadev_process::ManagedChild::spawn(command).map_err(|error| {
        git_commit_blocked(
            "git-command-unavailable",
            &format!("无法执行 Git 事务命令 / unable to execute Git transaction: {error}"),
        )
    })?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| git_commit_blocked("git-output-invalid", "Git stdout pipe 不可用"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| git_commit_blocked("git-output-invalid", "Git stderr pipe 不可用"))?;
    let stdin = if input.is_some() {
        Some(child.take_stdin().ok_or_else(|| {
            git_commit_blocked(
                "git-input-invalid",
                "Git stdin pipe 不可用 / Git stdin unavailable",
            )
        })?)
    } else {
        None
    };

    // The readers and optional writer are ordinary futures owned by this
    // stack frame, rather than detached tasks. Dropping the outer future closes
    // every pipe before `ManagedChild` tears down the complete process tree.
    let completed = tokio::time::timeout(timeout, async {
        let write_input = async move {
            let Some(mut stdin) = stdin else {
                return Ok::<(), std::io::Error>(());
            };
            let bytes = input.unwrap_or_default();
            stdin.write_all(bytes).await?;
            stdin.shutdown().await
        };
        tokio::join!(
            child.wait(),
            read_bounded_output_tail(stdout, MUTATION_OUTPUT_LIMIT),
            read_bounded_output_tail(stderr, MUTATION_OUTPUT_LIMIT),
            write_input,
        )
    })
    .await;
    let Ok((status, stdout, stderr, write)) = completed else {
        let _ = child.terminate_and_reap(MUTATION_REAP_GRACE).await;
        return Err(git_commit_blocked(
            timeout_code,
            &format!(
                "{command_label} 超过 {} 秒并已终止完整进程树 / command timed out and its process tree was terminated",
                timeout.as_secs_f64(),
            ),
        ));
    };
    let status = status.map_err(|error| {
        git_commit_blocked(
            "git-command-unavailable",
            &format!("无法等待 Git 事务命令 / unable to wait for Git transaction: {error}"),
        )
    })?;
    let stdout = stdout.map_err(|error| {
        git_commit_blocked(
            "git-output-invalid",
            &format!("读取 Git stdout 失败 / unable to read Git stdout: {error}"),
        )
    })?;
    let stderr = stderr.map_err(|error| {
        git_commit_blocked(
            "git-output-invalid",
            &format!("读取 Git stderr 失败 / unable to read Git stderr: {error}"),
        )
    })?;
    write.map_err(|error| {
        git_commit_blocked(
            "git-input-failed",
            &format!("无法写入 Git stdin / unable to write Git stdin: {error}"),
        )
    })?;
    if stdout.truncated || stderr.truncated {
        return Err(git_commit_blocked(
            "git-output-limit-exceeded",
            &format!(
                "{command_label} 输出超过每路 {MUTATION_OUTPUT_LIMIT} bytes 上限,拒绝使用截断结果 / Git mutation output exceeded its hard limit; truncated output was rejected"
            ),
        ));
    }
    Ok(std::process::Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

#[derive(Debug)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_bounded_output_tail<R>(
    mut reader: R,
    max_retained_bytes: usize,
) -> Result<CapturedOutput, std::io::Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut retained = umadev_process::BoundedTail::new(max_retained_bytes);
    let mut chunk = [0u8; 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        retained.push(&chunk[..read]);
    }
    let truncated = retained.truncated();
    Ok(CapturedOutput {
        bytes: retained.into_bytes(),
        truncated,
    })
}

pub(crate) fn git_required_text(
    root: &Path,
    args: &[&str],
    code: &'static str,
) -> Result<String, ResidentExecutionBlocked> {
    let output = git_output(root, args)?;
    if !output.status.success() {
        return Err(git_command_failed(code, "git", &output));
    }
    let value = String::from_utf8(output.stdout).map_err(|_| {
        git_commit_blocked(
            code,
            "Git 返回了非 UTF-8 输出 / Git returned non-UTF-8 output",
        )
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(git_commit_blocked(
            code,
            "Git 返回了空结果 / Git returned an empty result",
        ));
    }
    Ok(value.to_string())
}

pub(crate) fn git_command_failed(
    code: &'static str,
    command: &str,
    output: &std::process::Output,
) -> ResidentExecutionBlocked {
    let detail = bounded_git_stderr(&output.stderr);
    git_commit_blocked(
        code,
        &format!(
            "{command} 执行失败{}{} / command failed; no automatic retry",
            if detail.is_empty() { "" } else { ": " },
            detail
        ),
    )
}

pub(crate) fn bounded_git_stderr(stderr: &[u8]) -> String {
    const MAX_CHARS: usize = 2_000;
    let decoded = String::from_utf8_lossy(stderr);
    let cleaned = umadev_agent::base_error::strip_ansi(&decoded)
        .chars()
        .map(|character| {
            if character == '\n' || character == '\r' || character == '\t' {
                ' '
            } else if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect::<String>();
    let count = cleaned.chars().count();
    let tail = cleaned
        .chars()
        .skip(count.saturating_sub(MAX_CHARS))
        .collect::<String>();
    tail.trim().to_string()
}

pub(crate) fn bounded_text(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    value
        .chars()
        .skip(count.saturating_sub(max_chars))
        .collect()
}
