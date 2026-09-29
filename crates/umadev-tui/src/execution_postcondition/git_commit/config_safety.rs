use super::{
    bounded_git_command_output, git_command_failed, git_commit_blocked, git_std_command,
    remove_git_environment_overrides, GitCommandLimits, Path, ResidentExecutionBlocked,
};
use std::process::Command;

const MAX_GIT_CONFIG_BYTES: usize = 512 * 1024;

pub(crate) fn git_output_without_filter_programs(
    root: &Path,
    args: &[&str],
) -> Result<std::process::Output, ResidentExecutionBlocked> {
    let overrides = configured_filter_overrides(root)?;
    let mut command = git_std_command(root);
    for value in &overrides {
        command.arg("-c").arg(value);
    }
    command.args(args);
    bounded_git_command_output(
        command,
        GitCommandLimits::default(),
        "git-command-unavailable",
        "isolated git",
    )
}

fn configured_filter_overrides(root: &Path) -> Result<Vec<String>, ResidentExecutionBlocked> {
    let mut command = Command::new("git");
    command
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args(["config", "--null", "--list", "--includes"]);
    remove_git_environment_overrides(&mut command);
    let output = bounded_git_command_output(
        command,
        GitCommandLimits {
            stdout_bytes: MAX_GIT_CONFIG_BYTES,
            ..GitCommandLimits::default()
        },
        "git-config-unverifiable",
        "git config --null --list --includes",
    )?;
    if !output.status.success() {
        return Err(git_command_failed(
            "git-config-unverifiable",
            "git config --list",
            &output,
        ));
    }
    umadev_process::git::filter_driver_overrides(
        &output.stdout,
        umadev_process::git::ConfigListing::Unscoped,
    )
    .map_err(|error| {
        let detail = match error {
            umadev_process::git::FilterConfigError::NonUtf8Key => {
                "Git config key 不是 UTF-8 / Git config key is not UTF-8"
            }
            umadev_process::git::FilterConfigError::InvalidDriverName => {
                "Git filter 名称无效 / invalid Git filter driver name"
            }
            umadev_process::git::FilterConfigError::TooManyDrivers => {
                "Git filter 数量过多 / too many Git filter drivers"
            }
        };
        git_commit_blocked("git-config-invalid", detail)
    })
}
