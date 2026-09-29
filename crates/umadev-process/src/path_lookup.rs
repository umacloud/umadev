//! `PATH` lookup for the programs UmaDev resolves before spawning.
//!
//! Resolving up front instead of leaving it to the OS has two purposes. On
//! Windows, `CreateProcess` appends only `.exe`, so the npm `.cmd` shims most
//! CLIs install as are never found; and a caller can report "not installed"
//! without spawning anything.
//!
//! Only absolute `PATH` entries are searched. A relative entry such as `.` or
//! `node_modules\.bin` is resolved against the current directory, which for
//! UmaDev is usually a workspace it has no reason to trust, so a program found
//! that way would come from the repository rather than from the user's install.

use std::path::{Path, PathBuf};

/// The extensions Windows can spawn directly. Anything else in `PATHEXT`
/// (`.PS1`, `.JS`, `.VBS`, ...) needs a separate interpreter and is ignored.
const SPAWNABLE_WINDOWS_EXTENSIONS: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];

/// The absolute directories on `PATH`, in order. Empty and relative entries are
/// dropped (see the module docs).
#[must_use]
pub fn search_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| search_dirs_in(&path))
        .unwrap_or_default()
}

fn search_dirs_in(path: &std::ffi::OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .collect()
}

/// Candidate file extensions for a bare program name on this platform, most
/// specific first. See [`executable_extensions_for`].
#[must_use]
pub fn executable_extensions() -> Vec<String> {
    executable_extensions_for(std::env::var("PATHEXT").ok().as_deref(), cfg!(windows))
}

/// Candidate file extensions for `pathext` on the given platform.
///
/// On Windows this is the directly spawnable subset of `PATHEXT` in its order,
/// then any of those four missing from it, then the bare name as the last
/// resort: npm puts both `codex` (a Unix shell script, not a PE) and
/// `codex.cmd` in the same directory, and the `.cmd` must win. Elsewhere it is
/// only the bare name. Pure so both platforms are testable on either.
#[must_use]
pub fn executable_extensions_for(pathext: Option<&str>, windows: bool) -> Vec<String> {
    if !windows {
        return vec![String::new()];
    }
    let mut extensions: Vec<String> = Vec::with_capacity(SPAWNABLE_WINDOWS_EXTENSIONS.len() + 1);
    let listed = pathext
        .unwrap_or_default()
        .split(';')
        .filter_map(|extension| {
            SPAWNABLE_WINDOWS_EXTENSIONS
                .iter()
                .find(|candidate| candidate.eq_ignore_ascii_case(extension.trim()))
        });
    for extension in listed.chain(SPAWNABLE_WINDOWS_EXTENSIONS.iter()) {
        if !extensions.iter().any(|existing| existing == extension) {
            extensions.push((*extension).to_string());
        }
    }
    extensions.push(String::new());
    extensions
}

/// The first `dir/program{extension}` that is a spawnable file, trying
/// `extensions` in order. A relative or empty `dir` never matches.
#[must_use]
pub fn find_in_dir(dir: &Path, program: &str, extensions: &[String]) -> Option<PathBuf> {
    if !dir.is_absolute() {
        return None;
    }
    extensions
        .iter()
        .map(|extension| dir.join(format!("{program}{extension}")))
        .find(|candidate| is_spawnable_file(candidate))
}

/// Resolve a bare program name against the absolute `PATH` entries. A name
/// with a path separator is not looked up and yields `None`.
#[must_use]
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    if program.is_empty() || program.contains(std::path::is_separator) {
        return None;
    }
    let extensions = executable_extensions();
    search_dirs()
        .iter()
        .find_map(|dir| find_in_dir(dir, program, &extensions))
}

/// Resolve `program` for spawning: its full path when [`find_on_path`] finds
/// it, otherwise `program` unchanged so the spawn reports the real error.
#[must_use]
pub fn resolve_on_path(program: &str) -> String {
    find_on_path(program)
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| program.to_string())
}

/// Whether `program` is installed: an explicit path must name a spawnable file,
/// and a bare name must be found by [`find_on_path`].
#[must_use]
pub fn is_installed(program: &str) -> bool {
    if program.contains(std::path::is_separator) {
        is_spawnable_file(Path::new(program))
    } else {
        find_on_path(program).is_some()
    }
}

/// Whether `path` can be spawned on this OS.
///
/// On Unix a regular file also needs an execute bit: package-manager debris or
/// a downloaded source file earlier on `PATH` must not shadow the real program.
/// Windows decides executability from the extension and file format when the
/// process is created, so a regular file is enough.
#[must_use]
pub fn is_spawnable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        path.metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::{executable_extensions, executable_extensions_for, find_in_dir, search_dirs_in};
    use std::path::Path;

    #[test]
    fn windows_extensions_keep_only_directly_spawnable_types() {
        assert_eq!(
            executable_extensions_for(Some(".PS1;.CMD;.EXE;.JS;.cmd"), true),
            [".CMD", ".EXE", ".COM", ".BAT", ""]
        );
        assert_eq!(
            executable_extensions_for(None, true),
            [".COM", ".EXE", ".BAT", ".CMD", ""]
        );
        assert_eq!(executable_extensions_for(Some(".PS1"), false), [""]);
    }

    #[test]
    fn empty_missing_and_relative_directories_never_match() {
        let extensions = executable_extensions();
        assert!(find_in_dir(Path::new(""), "codex", &extensions).is_none());
        assert!(find_in_dir(
            Path::new("/umadev/no/such/dir/at/all"),
            "codex",
            &extensions
        )
        .is_none());
        assert!(find_in_dir(Path::new("."), "codex", &extensions).is_none());
    }

    #[test]
    fn relative_path_entries_are_never_searched() {
        let absolute = std::env::temp_dir();
        let path = std::env::join_paths([
            Path::new("."),
            absolute.as_path(),
            Path::new("node_modules/.bin"),
            Path::new(""),
        ])
        .unwrap();
        assert_eq!(search_dirs_in(&path), [absolute]);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_regular_file_is_not_a_program() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("not-a-command");
        std::fs::write(&candidate, "plain text\n").unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(find_in_dir(dir.path(), "not-a-command", &executable_extensions()).is_none());
    }
}
