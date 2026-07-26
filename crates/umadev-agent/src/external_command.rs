use std::path::Path;
use std::process::Command;
use std::time::Duration;

const STDERR_BYTES: usize = 256 * 1024;
const READER_GRACE: Duration = Duration::from_secs(1);

#[cfg(windows)]
const INERT_HOOKS: &str = "core.hooksPath=NUL";
#[cfg(not(windows))]
const INERT_HOOKS: &str = "core.hooksPath=/dev/null";
#[cfg(windows)]
const EMPTY_ATTRIBUTES: &str = "core.attributesFile=NUL";
#[cfg(not(windows))]
const EMPTY_ATTRIBUTES: &str = "core.attributesFile=/dev/null";

pub(crate) fn bounded_git_output(
    root: &Path,
    args: &[&str],
    timeout: Duration,
    stdout_bytes: usize,
) -> Option<std::process::Output> {
    let mut command = git_command(root);
    command.args(args);
    bounded_output(command, timeout, stdout_bytes)
}

pub(crate) fn bounded_gh_output(
    root: &Path,
    args: &[&str],
    timeout: Duration,
    stdout_bytes: usize,
) -> Option<std::process::Output> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("TERM", "dumb");
    bounded_output(command, timeout, stdout_bytes)
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_")
            || matches!(upper.as_str(), "EMAIL" | "SSH_ASKPASS" | "GCM_INTERACTIVE")
        {
            command.env_remove(key);
        }
    }
    command
        .arg("--no-pager")
        .arg("--literal-pathspecs")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            INERT_HOOKS,
            "-c",
            EMPTY_ATTRIBUTES,
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "gc.auto=0",
        ])
        .arg("-C")
        .arg(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat");
    command
}

fn bounded_output(
    command: Command,
    timeout: Duration,
    stdout_bytes: usize,
) -> Option<std::process::Output> {
    let output = umadev_process::run_bounded_std_command(
        command,
        umadev_process::BoundedCommandOptions {
            timeout,
            stdout_bytes,
            stderr_bytes: STDERR_BYTES,
            reader_grace: READER_GRACE,
        },
    )
    .ok()?;
    if output.timed_out
        || output.stdout_truncated
        || output.stderr_truncated
        || output.status.is_none()
    {
        return None;
    }
    Some(std::process::Output {
        status: output.status?,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::{bounded_output, git_command};
    use std::ffi::OsStr;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn git_children_are_noninteractive_and_ignore_environment_redirects() {
        let command = git_command(std::path::Path::new("."));
        let env = command.get_envs().collect::<Vec<_>>();
        let value = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == OsStr::new(name))
                .and_then(|(_, value)| *value)
        };
        assert_eq!(value("GIT_TERMINAL_PROMPT"), Some(OsStr::new("0")));
        assert_eq!(value("GCM_INTERACTIVE"), Some(OsStr::new("Never")));
        assert_eq!(value("GIT_PAGER"), Some(OsStr::new("cat")));
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_rejects_floods_and_hanging_descendants() {
        let mut flood = Command::new("sh");
        flood.args(["-c", "head -c 65536 /dev/zero"]);
        assert!(bounded_output(flood, Duration::from_secs(2), 1024).is_none());

        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("descendant-survived");
        let mut hang = Command::new("sh");
        hang.env("UMADEV_TEST_MARKER", &marker)
            .args(["-c", "(sleep 1; : > \"$UMADEV_TEST_MARKER\") & sleep 30"]);
        assert!(bounded_output(hang, Duration::from_millis(150), 1024).is_none());
        std::thread::sleep(Duration::from_millis(1_100));
        assert!(!marker.exists());
    }

    #[test]
    fn nonzero_exit_is_preserved_as_failure_status() {
        let mut command = Command::new("git");
        command.arg("--definitely-not-a-real-option");
        let output = bounded_output(command, Duration::from_secs(5), 8 * 1024).unwrap();
        assert!(!output.status.success());
    }
}
