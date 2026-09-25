//! Hardened `git` children for the commands UmaDev runs on its own initiative.
//!
//! A repository is data, not a program. A workspace that arrived as an archive
//! or a shared folder, or one an agent has since written to, can carry a
//! `.git/config`, `.git/hooks` and in-tree `.gitattributes` that name programs
//! Git runs as a side effect of ordinary read commands: `core.fsmonitor`, hooks
//! such as `post-index-change`, clean/smudge/process filters, textconv and
//! external diff drivers, a pager, and transport programs for a lazy fetch in a
//! partial clone. Every automatic `git` child is built here so none of them can
//! run. The user's own global configuration is still honoured; only a program
//! the repository defines is suppressed.
//!
//! Two controls depend on the subcommand, so callers add them:
//! - diff/log family: [`NO_DIFF_PROGRAMS`]. Git has no neutral value for
//!   `diff.<driver>.textconv` or `diff.<driver>.command` (an empty one is an
//!   error, not "unset"), so those drivers are disabled by flag instead.
//! - work-tree scans (`status`, a work-tree `diff`): [`IGNORE_DIRTY_SUBMODULES`].
//!   Git re-runs itself inside every submodule with that submodule's own config,
//!   whose filter drivers the discovery below never sees.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

/// The platform null device, for configuration paths that must name nothing.
#[cfg(windows)]
pub const NULL_DEVICE: &str = "NUL";
/// The platform null device, for configuration paths that must name nothing.
#[cfg(not(windows))]
pub const NULL_DEVICE: &str = "/dev/null";

#[cfg(windows)]
const INERT_HOOKS: &str = "core.hooksPath=NUL";
#[cfg(not(windows))]
const INERT_HOOKS: &str = "core.hooksPath=/dev/null";
#[cfg(windows)]
const EMPTY_ATTRIBUTES: &str = "core.attributesFile=NUL";
#[cfg(not(windows))]
const EMPTY_ATTRIBUTES: &str = "core.attributesFile=/dev/null";

/// Flags every diff/log-family caller passes: no external diff, no textconv.
pub const NO_DIFF_PROGRAMS: [&str; 2] = ["--no-ext-diff", "--no-textconv"];

/// Flag every work-tree scan passes so Git does not descend into a submodule's
/// work tree under configuration this module never inspected. Submodule commit
/// changes are still reported.
pub const IGNORE_DIRTY_SUBMODULES: &str = "--ignore-submodules=dirty";

const MAX_CONFIG_LISTING_BYTES: usize = 512 * 1024;
const MAX_FILTER_DRIVERS: usize = 256;
const CONFIG_LISTING_OPTIONS: crate::BoundedCommandOptions = crate::BoundedCommandOptions {
    timeout: Duration::from_secs(10),
    stdout_bytes: MAX_CONFIG_LISTING_BYTES,
    stderr_bytes: 32 * 1024,
    reader_grace: Duration::from_secs(1),
};

/// Whether a hardened child may take Git's optional locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAccess {
    /// Status, diff and probes. `--no-optional-locks` keeps a background
    /// snapshot from refreshing the index, which would both contend with the
    /// base's own Git work and fire `post-index-change`.
    ReadOnly,
    /// A deliberate mutation such as a branch switch. Hooks stay inert.
    Mutating,
}

/// How a `git config --null --list` listing was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigListing {
    /// A plain listing: every configured filter driver is blanked, whoever
    /// defined it.
    Unscoped,
    /// A `--show-scope` listing: a driver defined only in the user's global or
    /// system configuration (Git LFS, typically) is the user's own program and
    /// is kept; one the repository defines or redefines is blanked.
    Scoped,
}

/// Why a configuration listing cannot be turned into filter overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterConfigError {
    /// A configuration key is not UTF-8.
    NonUtf8Key,
    /// A filter driver name cannot be expressed as a `-c` override.
    InvalidDriverName,
    /// More filter drivers than one command line should carry.
    TooManyDrivers,
}

impl std::fmt::Display for FilterConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NonUtf8Key => "Git config key is not UTF-8",
            Self::InvalidDriverName => "invalid Git filter driver name",
            Self::TooManyDrivers => "too many Git filter drivers",
        })
    }
}

impl std::error::Error for FilterConfigError {}

impl From<FilterConfigError> for std::io::Error {
    fn from(error: FilterConfigError) -> Self {
        Self::new(std::io::ErrorKind::InvalidData, error)
    }
}

/// Build a hardened `git` command rooted at `root` (see the module docs).
///
/// Discovering the filter drivers to blank spawns one bounded
/// `git config --list`, which executes nothing. Any failure is returned so the
/// caller skips its probe rather than running Git without the overrides.
pub fn hardened_git_command(
    root: &Path,
    access: GitAccess,
) -> std::io::Result<std::process::Command> {
    let overrides = repository_filter_overrides(root)?;
    Ok(command_with_overrides(root, access, &overrides))
}

/// Whether the repository itself, rather than the user's global or system
/// configuration, defines a clean, smudge or process filter driver.
///
/// A hardened command that rewrites work-tree files (a branch switch) runs
/// with those drivers blanked, which for git-crypt or a repository-local LFS
/// setup would leave ciphertext or pointer files in the work tree. An automatic
/// caller skips such a command when this is `true` instead.
pub fn repository_defines_filters(root: &Path) -> std::io::Result<bool> {
    Ok(!repository_filter_overrides(root)?.is_empty())
}

fn repository_filter_overrides(root: &Path) -> std::io::Result<Vec<String>> {
    let scoped = crate::run_bounded_std_command(
        config_listing_command(root, ConfigListing::Scoped),
        CONFIG_LISTING_OPTIONS,
    )?;
    match listing_overrides(&scoped, ConfigListing::Scoped) {
        Some(overrides) => overrides,
        None => {
            let unscoped = crate::run_bounded_std_command(
                config_listing_command(root, ConfigListing::Unscoped),
                CONFIG_LISTING_OPTIONS,
            )?;
            listing_overrides(&unscoped, ConfigListing::Unscoped).ok_or_else(listing_failed)?
        }
    }
}

/// Async form of [`hardened_git_command`] for callers on a runtime thread.
pub async fn hardened_git_tokio_command(
    root: &Path,
    access: GitAccess,
) -> std::io::Result<tokio::process::Command> {
    let listing = |kind| {
        crate::run_bounded_command(
            tokio::process::Command::from(config_listing_command(root, kind)),
            CONFIG_LISTING_OPTIONS,
        )
    };
    let scoped = listing(ConfigListing::Scoped).await?;
    let overrides = match listing_overrides(&scoped, ConfigListing::Scoped) {
        Some(overrides) => overrides?,
        None => {
            let unscoped = listing(ConfigListing::Unscoped).await?;
            listing_overrides(&unscoped, ConfigListing::Unscoped).ok_or_else(listing_failed)??
        }
    };
    Ok(tokio::process::Command::from(command_with_overrides(
        root, access, &overrides,
    )))
}

/// Remove every environment variable that reroutes Git or injects
/// configuration, including ones newer than this build of UmaDev knows.
pub fn remove_git_environment(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_")
            || matches!(upper.as_str(), "EMAIL" | "SSH_ASKPASS" | "GCM_INTERACTIVE")
        {
            command.env_remove(key);
        }
    }
}

/// Parse `git config --null --list` output into `-c` overrides that blank the
/// clean, smudge and process programs of each driver `listing` selects.
///
/// # Errors
///
/// Fails closed on a key that is not UTF-8, a driver name that cannot be
/// written as a `-c` override, or more than 256 drivers.
pub fn filter_driver_overrides(
    listing: &[u8],
    kind: ConfigListing,
) -> Result<Vec<String>, FilterConfigError> {
    let mut records = listing.split(|byte| *byte == 0);
    let mut drivers = BTreeSet::new();
    while let Some(first) = records.next() {
        let (user_scope, record) = match kind {
            ConfigListing::Unscoped => (false, first),
            ConfigListing::Scoped => {
                let Some(record) = records.next() else {
                    break;
                };
                (matches!(first, b"global" | b"system"), record)
            }
        };
        let key_end = record
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(record.len());
        let key =
            std::str::from_utf8(&record[..key_end]).map_err(|_| FilterConfigError::NonUtf8Key)?;
        if user_scope {
            continue;
        }
        let Some(driver) = filter_driver_name(key) else {
            continue;
        };
        if driver.is_empty()
            || driver.len() > 256
            || driver.contains('=')
            || driver.chars().any(char::is_control)
        {
            return Err(FilterConfigError::InvalidDriverName);
        }
        drivers.insert(driver.to_string());
        if drivers.len() > MAX_FILTER_DRIVERS {
            return Err(FilterConfigError::TooManyDrivers);
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

/// The driver of a `filter.<driver>.<program>` key. Git lowercases the section
/// and variable but keeps the subsection's case, so only those are folded.
fn filter_driver_name(key: &str) -> Option<&str> {
    let lower = key.to_ascii_lowercase();
    let rest = lower.strip_prefix("filter.")?;
    [".clean", ".smudge", ".process", ".required"]
        .iter()
        .find(|suffix| rest.ends_with(**suffix))
        .map(|suffix| &key["filter.".len()..key.len() - suffix.len()])
}

fn config_listing_command(root: &Path, kind: ConfigListing) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    remove_git_environment(&mut command);
    command
        .arg("-C")
        .arg(root)
        .args(["config", "--null", "--list", "--includes"])
        .args((kind == ConfigListing::Scoped).then_some("--show-scope"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// Overrides from a finished listing, or `None` when this listing kind could
/// not be produced (`--show-scope` predates Git 2.26).
fn listing_overrides(
    output: &crate::BoundedCommandOutput,
    kind: ConfigListing,
) -> Option<std::io::Result<Vec<String>>> {
    if output.timed_out || output.stdout_truncated {
        return Some(Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Git config listing exceeded its bounded command envelope",
        )));
    }
    if !output.status.is_some_and(|status| status.success()) {
        return None;
    }
    Some(filter_driver_overrides(&output.stdout, kind).map_err(Into::into))
}

fn listing_failed() -> std::io::Error {
    std::io::Error::other("Git config listing failed")
}

fn command_with_overrides(
    root: &Path,
    access: GitAccess,
    overrides: &[String],
) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    remove_git_environment(&mut command);
    command.arg("--no-pager").arg("--literal-pathspecs");
    if access == GitAccess::ReadOnly {
        command.arg("--no-optional-locks");
    }
    command.args([
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
        "-c",
        "protocol.allow=never",
    ]);
    for value in overrides {
        command.arg("-c").arg(value);
    }
    command
        .arg("-C")
        .arg(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_PAGER", "cat");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[cfg(unix)]
    fn git(root: &Path, args: &[&str]) {
        let mut command = std::process::Command::new("git");
        remove_git_environment(&mut command);
        let status = command
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=UmaDev Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[cfg(unix)]
    fn run(command: std::process::Command) -> std::process::Output {
        let output =
            crate::run_bounded_std_command(command, crate::BoundedCommandOptions::default())
                .unwrap();
        std::process::Output {
            status: output.status.unwrap(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }

    /// A committed repository whose own config and hooks name programs that
    /// each leave a marker, with every tracked file stat-dirty.
    #[cfg(unix)]
    fn hostile_repository(temp: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitattributes"),
            "* filter=hostile diff=hostile\n",
        )
        .unwrap();
        std::fs::write(root.join("file.txt"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);
        let program = |name: &str| {
            format!(
                "touch '{}'; cat",
                temp.join(format!("{name}-ran")).display()
            )
        };
        for (key, value) in [
            ("filter.hostile.clean", program("clean")),
            ("filter.hostile.smudge", program("smudge")),
            ("filter.hostile.required", "true".to_string()),
            ("diff.hostile.textconv", program("textconv")),
            ("diff.hostile.command", program("diff-command")),
            ("diff.external", program("external-diff")),
            ("core.fsmonitor", program("fsmonitor")),
            ("core.pager", program("pager")),
        ] {
            git(&root, &["config", key, &value]);
        }
        let hook = root.join(".git/hooks/post-index-change");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", temp.join("hook-ran").display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("file.txt"), "two\n").unwrap();
        root
    }

    #[cfg(unix)]
    fn markers(temp: &Path) -> Vec<String> {
        std::fs::read_dir(temp)
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with("-ran"))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn automatic_status_and_diff_run_no_repository_program() {
        if !git_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = hostile_repository(temp.path());

        let mut status = hardened_git_command(&root, GitAccess::ReadOnly).unwrap();
        status.args(["status", "--porcelain", IGNORE_DIRTY_SUBMODULES]);
        let status = run(status);
        assert!(status.status.success(), "{status:?}");
        assert!(String::from_utf8_lossy(&status.stdout).contains("file.txt"));

        let mut diff = hardened_git_command(&root, GitAccess::ReadOnly).unwrap();
        diff.arg("diff")
            .args(NO_DIFF_PROGRAMS)
            .args([IGNORE_DIRTY_SUBMODULES, "HEAD"]);
        let diff = run(diff);
        assert!(diff.status.success(), "{diff:?}");
        assert!(String::from_utf8_lossy(&diff.stdout).contains("+two"));

        let mut stat = hardened_git_command(&root, GitAccess::Mutating).unwrap();
        stat.args(["diff", "--stat", IGNORE_DIRTY_SUBMODULES]);
        assert!(run(stat).status.success());

        assert_eq!(markers(temp.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn async_builder_runs_no_repository_filter() {
        if !git_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = hostile_repository(temp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime.block_on(async {
            let mut command = hardened_git_tokio_command(&root, GitAccess::ReadOnly)
                .await
                .unwrap();
            command.args(["status", "--porcelain", IGNORE_DIRTY_SUBMODULES]);
            crate::run_bounded_command(command, crate::BoundedCommandOptions::default())
                .await
                .unwrap()
        });
        assert!(output.status.is_some_and(|status| status.success()));
        assert_eq!(markers(temp.path()), Vec::<String>::new());
    }

    #[test]
    fn git_children_are_noninteractive_and_ignore_environment_redirects() {
        let command = command_with_overrides(Path::new("."), GitAccess::ReadOnly, &[]);
        let env = command.get_envs().collect::<Vec<_>>();
        let value = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .and_then(|(_, value)| *value)
        };
        assert_eq!(value("GIT_TERMINAL_PROMPT"), Some("0".as_ref()));
        assert_eq!(value("GCM_INTERACTIVE"), Some("Never".as_ref()));
        assert_eq!(value("GIT_PAGER"), Some("cat".as_ref()));
        assert_eq!(value("GIT_CONFIG_NOSYSTEM"), Some("1".as_ref()));
        let args = command.get_args().collect::<Vec<_>>();
        assert!(args.contains(&"--no-optional-locks".as_ref()));
        assert!(args.contains(&INERT_HOOKS.as_ref()));
    }

    #[test]
    fn scoped_listing_blanks_only_repository_defined_drivers() {
        let listing = b"global\0filter.lfs.clean\ngit-lfs clean -- %f\0\
            global\0filter.Shared.clean\nuser\0\
            local\0filter.Shared.smudge\nrepo\0\
            local\0filter.Evil.process\nrepo\0\
            local\0core.bare\nfalse\0";
        let overrides = filter_driver_overrides(listing, ConfigListing::Scoped).unwrap();
        assert!(overrides.contains(&"filter.Evil.process=".to_string()));
        assert!(overrides.contains(&"filter.Shared.clean=".to_string()));
        assert!(!overrides
            .iter()
            .any(|value| value.starts_with("filter.lfs.")));
        assert_eq!(overrides.len(), 8);

        let unscoped = b"filter.lfs.clean\ngit-lfs\0core.bare\nfalse\0";
        assert_eq!(
            filter_driver_overrides(unscoped, ConfigListing::Unscoped).unwrap(),
            [
                "filter.lfs.clean=",
                "filter.lfs.smudge=",
                "filter.lfs.process=",
                "filter.lfs.required=false"
            ]
        );
    }

    #[test]
    fn unrepresentable_driver_names_fail_closed() {
        assert_eq!(
            filter_driver_overrides(b"filter.a=b.clean\nx\0", ConfigListing::Unscoped),
            Err(FilterConfigError::InvalidDriverName)
        );
        assert_eq!(
            filter_driver_overrides(b"local\0filter.\xff.clean\nx\0", ConfigListing::Scoped),
            Err(FilterConfigError::NonUtf8Key)
        );
    }
}
