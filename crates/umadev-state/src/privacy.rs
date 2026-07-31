//! Installation-local privacy keys for deterministic, non-enumerable persisted identities.
//!
//! The key lives in the user's `UmaDev` state directory, not in a project workspace. A copied
//! `.umadev/*.json` artifact therefore does not carry the material needed to dictionary-test
//! short requirements. Key creation is private, no-follow, atomic, and race-safe: concurrent
//! processes use exclusive creation and converge on one installation key.

use std::path::{Path, PathBuf};

/// Bytes of entropy in the installation provenance key.
pub const PROVENANCE_KEY_BYTES: usize = 32;

const STATE_DIR: &str = ".umadev";
const KEY_FILE: &str = "provenance.key";

/// Resolve the installation state directory selected by the environment.
///
/// `UMADEV_HOME` is the directory itself. Without it, `.umadev` is resolved
/// below `HOME`/`USERPROFILE`. Every returned path is canonical and verified as
/// a real directory; linked/reparse roots fail closed. Callers performing a
/// write may request safe single-component creation.
#[must_use]
pub fn state_directory(create_if_missing: bool) -> Option<PathBuf> {
    let candidate = state_candidate()?;
    let rooted = open_state_candidate(&candidate, create_if_missing)?;
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    rooted
        .matches_path(&canonical)
        .ok()
        .filter(|matches| *matches)
        .map(|_| canonical)
}

/// Open the installation state directory as a pinned filesystem capability.
///
/// Callers that perform more than a diagnostic path lookup must retain this
/// value across their complete read/modify/write lifecycle. A later rename or
/// replacement of the ambient state path then cannot redirect an operation to
/// a different directory generation.
#[must_use]
pub fn state_root(create_if_missing: bool) -> Option<crate::fs::RootedDir> {
    let candidate = state_candidate()?;
    open_state_candidate(&candidate, create_if_missing)
}

fn open_state_candidate(candidate: &Path, create_if_missing: bool) -> Option<crate::fs::RootedDir> {
    if create_if_missing {
        open_or_create_directory(candidate)
    } else {
        crate::fs::RootedDir::open_no_follow(candidate).ok()
    }
}

fn state_candidate() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("UMADEV_HOME").filter(|value| !value.is_empty()) {
        Some(PathBuf::from(state))
    } else {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))?;
        Some(PathBuf::from(home).join(STATE_DIR))
    }
}

/// Load or create the current installation's provenance key.
///
/// `UMADEV_HOME` names the `UmaDev` state directory itself (normally `~/.umadev`). When it is
/// absent, `HOME`/`USERPROFILE` is treated as the user home and `.umadev` is appended. Missing
/// or unsafe state fails closed by returning `None`: callers must decline to persist or trust
/// provenance, never fall back to an enumerable hash.
#[must_use]
pub fn installation_key() -> Option<[u8; PROVENANCE_KEY_BYTES]> {
    let state = open_or_create_directory(&state_candidate()?)?;
    installation_key_in_state(&state)
}

/// Load or create a provenance key below an explicit home root.
///
/// This is public so higher layers can remain hermetic in tests without mutating process-wide
/// environment variables.
#[must_use]
pub fn installation_key_in(home: &Path) -> Option<[u8; PROVENANCE_KEY_BYTES]> {
    let root = crate::fs::RootedDir::open_no_follow(home).ok()?;
    installation_key_beneath_home(&root)
}

#[cfg(test)]
fn installation_key_at(state: &Path) -> Option<[u8; PROVENANCE_KEY_BYTES]> {
    let state = open_or_create_directory(state)?;
    installation_key_in_state(&state)
}

fn installation_key_beneath_home(
    home: &crate::fs::RootedDir,
) -> Option<[u8; PROVENANCE_KEY_BYTES]> {
    home.ensure_dir(Path::new(STATE_DIR), false).ok()?;
    let state = home.open_dir(Path::new(STATE_DIR)).ok()?;
    installation_key_in_state(&state)
}

fn installation_key_in_state(state: &crate::fs::RootedDir) -> Option<[u8; PROVENANCE_KEY_BYTES]> {
    let relative = Path::new(KEY_FILE);
    if let Some(key) = read_key(state) {
        return Some(key);
    }
    if state.regular_file_exists(relative).is_err() {
        return None;
    }

    let mut key = [0_u8; PROVENANCE_KEY_BYTES];
    getrandom::getrandom(&mut key).ok()?;
    if key.iter().all(|byte| *byte == 0) {
        return None;
    }

    match state.publish_new_private(relative, &key, false) {
        Ok(()) => Some(key),
        // Another process can win exclusive creation while it is still writing. Wait for only
        // a small bounded window; a permanently partial/corrupt key must fail closed.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            for _ in 0..20 {
                if let Some(key) = read_key(state) {
                    return Some(key);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            None
        }
        Err(_) => read_key(state),
    }
}

fn open_or_create_directory(path: &Path) -> Option<crate::fs::RootedDir> {
    match crate::fs::RootedDir::open_no_follow(path) {
        Ok(root) => return Some(root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let parent = crate::fs::RootedDir::open_no_follow(path.parent()?).ok()?;
    let name = Path::new(path.file_name()?);
    parent.ensure_dir(name, false).ok()?;
    parent.open_dir(name).ok()
}

/// Generate an ephemeral privacy key for a non-persistent fallback.
///
/// Callers use this only when an installation key cannot be stored. It prevents a fallback to
/// enumerable hashing, but deliberately does not promise identity across calls or processes.
#[must_use]
pub fn ephemeral_key() -> Option<[u8; PROVENANCE_KEY_BYTES]> {
    let mut key = [0_u8; PROVENANCE_KEY_BYTES];
    getrandom::getrandom(&mut key).ok()?;
    (!key.iter().all(|byte| *byte == 0)).then_some(key)
}

fn read_key(state: &crate::fs::RootedDir) -> Option<[u8; PROVENANCE_KEY_BYTES]> {
    let bytes = state
        .read_bounded(Path::new(KEY_FILE), PROVENANCE_KEY_BYTES as u64)
        .ok()?;
    let key: [u8; PROVENANCE_KEY_BYTES] = bytes.try_into().ok()?;
    (!key.iter().all(|byte| *byte == 0)).then_some(key)
}

/// Location used by tests and diagnostics without exposing key material.
#[must_use]
pub fn key_path_in(home: &Path) -> PathBuf {
    home.join(STATE_DIR).join(KEY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_private_across_reads() {
        let home = tempfile::TempDir::new().unwrap();
        let first = installation_key_in(home.path()).expect("create key");
        let second = installation_key_in(home.path()).expect("read key");
        assert_eq!(first, second);
        assert_ne!(first, [0; PROVENANCE_KEY_BYTES]);
        assert_eq!(std::fs::read(key_path_in(home.path())).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(key_path_in(home.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn explicit_umadev_home_is_the_state_directory_not_its_parent() {
        let parent = tempfile::TempDir::new().unwrap();
        let state = parent.path().join("custom-state");
        let key = installation_key_at(&state).expect("create key in explicit state directory");
        assert_eq!(std::fs::read(state.join(KEY_FILE)).unwrap(), key);
        assert!(!state.join(STATE_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn key_creation_refuses_linked_state_or_key_paths() {
        use std::os::unix::fs::symlink;

        let home = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), home.path().join(STATE_DIR)).unwrap();
        assert_eq!(installation_key_in(home.path()), None);
        assert!(!outside.path().join(KEY_FILE).exists());

        std::fs::remove_file(home.path().join(STATE_DIR)).unwrap();
        std::fs::create_dir(home.path().join(STATE_DIR)).unwrap();
        let outside_key = outside.path().join("outside-key");
        std::fs::write(&outside_key, [7_u8; PROVENANCE_KEY_BYTES]).unwrap();
        symlink(&outside_key, key_path_in(home.path())).unwrap();
        assert_eq!(installation_key_in(home.path()), None);
        assert_eq!(
            std::fs::read(outside_key).unwrap(),
            [7; PROVENANCE_KEY_BYTES]
        );

        let linked_home = home.path().join("linked-home");
        symlink(outside.path(), &linked_home).unwrap();
        assert_eq!(installation_key_in(&linked_home), None);
    }

    #[cfg(unix)]
    #[test]
    fn key_creation_stays_on_the_open_home_generation() {
        let parent = tempfile::TempDir::new().unwrap();
        let home = parent.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let rooted = crate::fs::RootedDir::open_no_follow(&home).unwrap();

        let moved = parent.path().join("home-moved");
        std::fs::rename(&home, &moved).unwrap();
        std::fs::create_dir(&home).unwrap();
        let key = installation_key_beneath_home(&rooted).expect("create key in pinned home");

        assert_eq!(
            std::fs::read(moved.join(STATE_DIR).join(KEY_FILE)).unwrap(),
            key
        );
        assert!(!home.join(STATE_DIR).join(KEY_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn state_capability_keeps_settings_on_the_open_generation() {
        let parent = tempfile::TempDir::new().unwrap();
        let state = parent.path().join("state");
        let rooted = open_state_candidate(&state, true).expect("create state capability");

        let moved = parent.path().join("state-moved");
        std::fs::rename(&state, &moved).unwrap();
        std::fs::create_dir(&state).unwrap();
        rooted
            .atomic_write(
                Path::new("settings.json"),
                br#"{"animations_enabled":false}"#,
                false,
            )
            .unwrap();

        assert!(moved.join("settings.json").exists());
        assert!(!state.join("settings.json").exists());
    }
}
