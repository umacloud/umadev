use super::{
    bounded_git_command_output, git_command_failed, git_commit_blocked, git_std_command,
    remove_git_environment_overrides, BTreeSet, GitCommandLimits, Path, ResidentExecutionBlocked,
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
    let mut drivers = BTreeSet::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        let Some(separator) = record.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let key = std::str::from_utf8(&record[..separator]).map_err(|_| {
            git_commit_blocked(
                "git-config-invalid",
                "Git config key 不是 UTF-8 / Git config key is not UTF-8",
            )
        })?;
        let lower = key.to_ascii_lowercase();
        let Some(lower_rest) = lower.strip_prefix("filter.") else {
            continue;
        };
        let rest = &key["filter.".len()..];
        for suffix in [".clean", ".smudge", ".process", ".required"] {
            if lower_rest.ends_with(suffix) {
                let driver = &rest[..rest.len() - suffix.len()];
                if driver.is_empty()
                    || driver.len() > 256
                    || driver.contains('=')
                    || driver.chars().any(char::is_control)
                {
                    return Err(git_commit_blocked(
                        "git-config-invalid",
                        "Git filter 名称无效 / invalid Git filter driver name",
                    ));
                }
                drivers.insert(driver.to_string());
                if drivers.len() > 256 {
                    return Err(git_commit_blocked(
                        "git-config-invalid",
                        "Git filter 数量过多 / too many Git filter drivers",
                    ));
                }
            }
        }
    }
    let mut overrides = Vec::with_capacity(drivers.len().saturating_mul(4));
    for driver in drivers {
        overrides.push(format!("filter.{driver}.clean="));
        overrides.push(format!("filter.{driver}.smudge="));
        overrides.push(format!("filter.{driver}.process="));
        overrides.push(format!("filter.{driver}.required=false"));
    }
    Ok(overrides)
}
