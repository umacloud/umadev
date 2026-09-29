use super::{
    bounded_git_command_output, git_command_failed, git_commit_blocked, git_std_command,
    remove_git_environment_overrides, GitCommandLimits, Path, ResidentExecutionBlocked,
};
use std::process::Command;
use umadev_process::git::{ConfigListing, LineEndings};

const MAX_GIT_CONFIG_BYTES: usize = 512 * 1024;

/// `-c` overrides that let an isolated child read the work tree the way the
/// user's own Git does without running a configured program: every filter
/// driver is blanked, and the user's line-ending settings, which the isolated
/// configuration would otherwise drop, are forwarded.
pub(crate) struct IsolatedGitOverrides {
    filters: Vec<String>,
    pub(crate) line_endings: LineEndings,
}

impl IsolatedGitOverrides {
    pub(crate) fn args(&self) -> Vec<String> {
        let mut args = self.filters.clone();
        args.extend(self.line_endings.overrides());
        args
    }
}

pub(crate) fn git_output_without_filter_programs(
    root: &Path,
    args: &[&str],
) -> Result<std::process::Output, ResidentExecutionBlocked> {
    let overrides = configured_git_overrides(root)?;
    let mut command = git_std_command(root);
    for value in overrides.args() {
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

/// Read the user's effective configuration (system, global and repository)
/// once and derive the overrides every isolated child needs.
pub(crate) fn configured_git_overrides(
    root: &Path,
) -> Result<IsolatedGitOverrides, ResidentExecutionBlocked> {
    let output = bounded_git_command_output(
        effective_config_listing(root),
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
    isolated_git_overrides(&output.stdout)
}

/// A plain listing of every scope. Listing configuration runs no program.
fn effective_config_listing(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args(["config", "--null", "--list", "--includes"]);
    remove_git_environment_overrides(&mut command);
    command
}

fn isolated_git_overrides(listing: &[u8]) -> Result<IsolatedGitOverrides, ResidentExecutionBlocked> {
    let filters = umadev_process::git::filter_driver_overrides(listing, ConfigListing::Unscoped)
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
        })?;
    let line_endings =
        LineEndings::from_listing(listing, ConfigListing::Unscoped).map_err(|error| {
            git_commit_blocked(
                "git-config-invalid",
                &format!(
                    "Git 换行配置 `{0}` 的值无效 / Git line-ending setting `{0}` has an invalid value",
                    error.key
                ),
            )
        })?;
    Ok(IsolatedGitOverrides {
        filters,
        line_endings,
    })
}

#[cfg(test)]
mod tests {
    use super::{effective_config_listing, isolated_git_overrides};

    /// Git for Windows keeps `core.autocrlf=true` in the system config, which
    /// the isolated children never load; the listing must see it anyway.
    #[test]
    fn the_listing_forwards_system_and_global_line_endings() {
        let root = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(init.status.success());
        let system = root.path().join("system-gitconfig");
        std::fs::write(&system, "[core]\n\tautocrlf = true\n\tsafecrlf = warn\n").unwrap();
        let global = root.path().join("global-gitconfig");
        std::fs::write(&global, "[core]\n\tautocrlf = input\n").unwrap();

        let mut listing = effective_config_listing(root.path());
        listing
            .env("GIT_CONFIG_SYSTEM", &system)
            .env("GIT_CONFIG_GLOBAL", &global);
        let listing = listing.output().unwrap();
        assert!(listing.status.success());
        let overrides = isolated_git_overrides(&listing.stdout).unwrap();

        assert_eq!(
            overrides.args(),
            ["core.autocrlf=input", "core.safecrlf=warn"]
        );
        assert!(overrides.line_endings.normalizes_stored_text());
    }
}
