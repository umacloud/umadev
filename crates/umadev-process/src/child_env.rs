//! Environment every child spawned through this crate carries.
//!
//! On Windows, `cmd.exe` looks for a bare command name in the current directory
//! before it searches `PATH`. That covers every bare name inside a batch file it
//! runs, including the npm `cmd-shim` launchers (`%APPDATA%\npm\<tool>.cmd`) for
//! the base CLIs, which run a bare `node` when no `node.exe` sits next to them.
//! UmaDev starts most children in the workspace, so without this a `node.cmd`
//! committed to a repository would run in place of Node the first time a base
//! CLI is probed, before any prompt or approval. Rust's own program lookup never
//! searches the current directory; only the batch interpreter does.
//!
//! `NoDefaultCurrentDirectoryInExePath` switches that lookup off for `cmd.exe`
//! and for anything else that consults `NeedCurrentDirectoryForExePathW`. It is
//! set on every child rather than only on batch targets because descendants
//! inherit it: a native base that later runs a shell command gets the same
//! protection.

/// The variable that removes the current directory from Windows executable
/// search, and the value UmaDev sets. Windows checks only for its presence.
pub const NO_CURRENT_DIRECTORY_EXE_SEARCH: (&str, &str) =
    ("NoDefaultCurrentDirectoryInExePath", "1");

/// Environment overrides a child receives on the given platform.
///
/// Pure so the Windows set is testable on every target.
#[must_use]
pub fn child_env_overrides(windows: bool) -> &'static [(&'static str, &'static str)] {
    if windows {
        &[NO_CURRENT_DIRECTORY_EXE_SEARCH]
    } else {
        &[]
    }
}

/// Apply [`child_env_overrides`] for the running platform to `command`.
///
/// Every spawn helper in this crate calls this, so callers only need it for a
/// child they spawn some other way.
pub fn harden_child_env(command: &mut std::process::Command) {
    for (name, value) in child_env_overrides(cfg!(windows)) {
        command.env(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::{child_env_overrides, harden_child_env, NO_CURRENT_DIRECTORY_EXE_SEARCH};
    use std::ffi::OsStr;

    fn env_value<'a>(command: &'a std::process::Command, name: &str) -> Option<&'a OsStr> {
        command
            .get_envs()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value)
    }

    #[test]
    fn windows_children_never_search_the_current_directory() {
        assert_eq!(
            child_env_overrides(true),
            [("NoDefaultCurrentDirectoryInExePath", "1")]
        );
        assert!(child_env_overrides(false).is_empty());
    }

    #[test]
    fn hardening_sets_exactly_the_platform_overrides() {
        let mut command = std::process::Command::new("tool");
        harden_child_env(&mut command);
        let (name, value) = NO_CURRENT_DIRECTORY_EXE_SEARCH;
        if cfg!(windows) {
            assert_eq!(env_value(&command, name), Some(OsStr::new(value)));
        } else {
            assert_eq!(command.get_envs().count(), 0);
        }
    }

    /// A batch file that runs a bare name must not pick up a same-named batch
    /// file from the child's working directory, which is exactly what an npm
    /// shim's bare `node` did with a repository's `node.cmd`.
    #[cfg(windows)]
    #[test]
    fn a_batch_child_ignores_a_program_planted_in_its_working_directory() {
        let shim_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("planted-ran");
        let shim = shim_dir.path().join("tool.cmd");
        std::fs::write(&shim, "@echo off\r\ncall umadevplanted\r\n").unwrap();
        std::fs::write(
            workspace.path().join("umadevplanted.cmd"),
            format!("@echo off\r\necho ran> \"{}\"\r\n", marker.display()),
        )
        .unwrap();

        let mut command = std::process::Command::new(&shim);
        command.current_dir(workspace.path());
        let output = crate::run_bounded_std_command(
            command,
            crate::BoundedCommandOptions {
                timeout: std::time::Duration::from_secs(20),
                stdout_bytes: 4096,
                stderr_bytes: 4096,
                reader_grace: std::time::Duration::from_secs(1),
            },
        )
        .unwrap();

        assert!(!output.timed_out);
        assert!(
            !marker.exists(),
            "cmd.exe ran a program from the working directory"
        );
    }
}
