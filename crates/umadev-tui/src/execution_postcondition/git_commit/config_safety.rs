use super::{
    bounded_git_command_output, configured_git_path, git_command_failed, git_commit_blocked,
    git_content_probe_command, remove_git_environment_overrides, GitCommandLimits, Path, PathBuf,
    ResidentExecutionBlocked,
};
use std::process::Command;
use umadev_process::git::{ConfigListing, LineEndings};

const MAX_GIT_CONFIG_BYTES: usize = 512 * 1024;

/// What an isolated child needs to read the work tree the way the user's own
/// Git does without running a configured program: every filter driver is
/// blanked, the user's line-ending settings (which the isolated configuration
/// would otherwise drop) are forwarded, and the user's attributes file is
/// named.
pub(crate) struct IsolatedGitOverrides {
    filters: Vec<String>,
    pub(crate) line_endings: LineEndings,
    /// The user's `core.attributesFile`; unset means Git's own default file.
    pub(crate) attributes_file: Option<PathBuf>,
}

impl IsolatedGitOverrides {
    /// The `-c` values: blanked filter drivers and forwarded line endings.
    pub(crate) fn args(&self) -> Vec<String> {
        let mut args = self.filters.clone();
        args.extend(self.line_endings.overrides());
        args
    }
}

/// Run a status or diff probe that compares work-tree content exactly as the
/// user's own `git status` does, with no filter program able to run.
pub(crate) fn git_output_without_filter_programs(
    root: &Path,
    args: &[&str],
) -> Result<std::process::Output, ResidentExecutionBlocked> {
    let overrides = configured_git_overrides(root)?;
    let mut command = git_content_probe_command(root, overrides.attributes_file.as_deref());
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
    let mut overrides = isolated_git_overrides(&output.stdout)?;
    overrides.attributes_file = configured_git_path(root, "core.attributesFile")?;
    Ok(overrides)
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

fn isolated_git_overrides(
    listing: &[u8],
) -> Result<IsolatedGitOverrides, ResidentExecutionBlocked> {
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
        attributes_file: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{effective_config_listing, git_content_probe_command, isolated_git_overrides};
    use std::path::Path;

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
    }

    /// Git for Windows keeps `core.autocrlf=true` in the system config, which
    /// the isolated children never load; the listing must see it anyway, and a
    /// child given the forwarded setting reads a CRLF copy of an LF blob as
    /// unchanged, like the user's own `git status`.
    #[test]
    fn the_listing_forwards_system_and_global_line_endings() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        let system = root.path().join(".git").join("system-gitconfig");
        std::fs::write(&system, "[core]\n\tautocrlf = true\n\tsafecrlf = warn\n").unwrap();
        let global = root.path().join(".git").join("global-gitconfig");
        std::fs::write(&global, "[core]\n\teol = crlf\n").unwrap();

        let mut listing = effective_config_listing(root.path());
        listing
            .env("GIT_CONFIG_SYSTEM", &system)
            .env("GIT_CONFIG_GLOBAL", &global);
        let listing = listing.output().unwrap();
        assert!(listing.status.success());
        let overrides = isolated_git_overrides(&listing.stdout).unwrap();
        assert_eq!(
            overrides.args(),
            ["core.autocrlf=true", "core.eol=crlf", "core.safecrlf=warn"]
        );
        assert!(overrides.line_endings.normalizes_stored_text());

        // The user's own `git add` stored LF; an editor then rewrote the same
        // CRLF bytes.
        let notes = root.path().join("notes.txt");
        std::fs::write(&notes, "one\r\ntwo\r\n").unwrap();
        git(
            root.path(),
            &["-c", "core.autocrlf=true", "add", "notes.txt"],
        );
        std::fs::write(&notes, "one\r\ntwo\r\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&notes)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();
        let status = |args: &[String]| {
            let mut command = git_content_probe_command(root.path(), None);
            for value in args {
                command.arg("-c").arg(value);
            }
            let output = command
                .args(["status", "--porcelain", "--", "notes.txt"])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(status(&[]), "AM notes.txt\n");
        assert_eq!(status(&overrides.args()), "A  notes.txt\n");
    }
}
