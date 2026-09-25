//! Ownership of the shadow checkpoint store.
//!
//! `.umadev/checkpoints.git` lives inside the project, so a repository can ship
//! one as ordinary tracked files: hooks, a `commondir` that points Git at a
//! config of the author's choosing, object alternates, and checkpoints an
//! automatic heal would restore into the work tree. UmaDev therefore only uses
//! a store this installation created for this project path. Creating a store
//! writes an owner stamp, an HMAC of the canonical project path under the
//! installation's private provenance key, which never leaves the user's state
//! directory and so cannot be forged by repository content. A store without a
//! matching stamp is never read, written, or restored from.

use std::path::{Path, PathBuf};

const OWNER_STAMP: &str = "umadev-owner";
const STAMP_DOMAIN: &[u8] = b"umadev.checkpoint-store-owner.v1";
const MAX_STAMP_BYTES: usize = 256;

/// Where an untrusted store is moved so a fresh one can be created. Exactly
/// one is kept: its contents are never read, and the user can inspect or
/// delete it. A second untrusted store is refused rather than piling up.
pub(super) const SET_ASIDE_DIR: &str = "checkpoints-untrusted.git";

/// Entries Git honours to take config, refs or objects from outside the
/// directory whose ownership was verified.
const REDIRECTING_ENTRIES: &[&str] = &[
    "commondir",
    "gitdir",
    "config.worktree",
    "worktrees",
    "objects/info/alternates",
    "objects/info/http-alternates",
];

fn stamp_path() -> PathBuf {
    Path::new(".umadev")
        .join("checkpoints.git")
        .join(OWNER_STAMP)
}

/// The installation key, read once per process: every stamp this process
/// writes or checks must agree even if `HOME` changes underneath it.
fn installation_key() -> Option<[u8; umadev_state::privacy::PROVENANCE_KEY_BYTES]> {
    static KEY: std::sync::OnceLock<[u8; umadev_state::privacy::PROVENANCE_KEY_BYTES]> =
        std::sync::OnceLock::new();
    if let Some(key) = KEY.get() {
        return Some(*key);
    }
    let key = umadev_state::privacy::installation_key()?;
    Some(*KEY.get_or_init(|| key))
}

fn expected_stamp(project_root: &Path) -> Option<String> {
    installation_tag(
        STAMP_DOMAIN,
        &[project_root_bytes(project_root)?.as_slice()],
    )
}

/// The canonical project path as bytes, the identity every per-project tag
/// below is bound to.
pub(crate) fn project_root_bytes(project_root: &Path) -> Option<Vec<u8>> {
    let canonical = std::fs::canonicalize(project_root).ok()?;
    Some(canonical.as_os_str().as_encoded_bytes().to_vec())
}

/// Hex HMAC of `parts` under the installation key, separated by `domain`.
/// Parts are length-prefixed so no two different part lists share a tag.
/// `None` without an installation key: callers then trust nothing.
pub(crate) fn installation_tag(domain: &[u8], parts: &[&[u8]]) -> Option<String> {
    let key = installation_key()?;
    let value = if let [single] = parts {
        single.to_vec()
    } else {
        parts
            .iter()
            .flat_map(|part| {
                (part.len() as u64)
                    .to_le_bytes()
                    .into_iter()
                    .chain(part.iter().copied())
            })
            .collect()
    };
    let tag = umadev_governance::privacy_fingerprint(&key, domain, &value);
    Some(tag.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Whether the store carries this installation's stamp for this project and
/// nothing inside it redirects Git elsewhere.
pub(super) fn is_trusted(project_root: &Path) -> bool {
    let Some(expected) = expected_stamp(project_root) else {
        return false;
    };
    let stamped =
        crate::bounded_fs::read_utf8_beneath(project_root, &stamp_path(), MAX_STAMP_BYTES)
            .is_ok_and(|stamp| stamp.trim() == expected);
    let git_dir = super::git_dir(project_root);
    stamped
        && REDIRECTING_ENTRIES.iter().all(|entry| {
            std::fs::symlink_metadata(git_dir.join(entry))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        })
}

/// Stamp the (new, empty) store as this installation's.
pub(super) fn claim(project_root: &Path) -> std::io::Result<()> {
    let stamp = expected_stamp(project_root).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "no installation key to stamp the checkpoint store with",
        )
    })?;
    umadev_state::fs::RootedDir::open(project_root)?.atomic_write(
        &stamp_path(),
        stamp.as_bytes(),
        false,
    )
}

/// Move an untrusted store to [`SET_ASIDE_DIR`] without looking inside it.
pub(super) fn set_aside(project_root: &Path) -> std::io::Result<()> {
    let target = Path::new(".umadev").join(SET_ASIDE_DIR);
    if std::fs::symlink_metadata(project_root.join(&target)).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "an untrusted checkpoint store was already set aside",
        ));
    }
    umadev_state::fs::RootedDir::open(project_root)?
        .rename(&Path::new(".umadev").join("checkpoints.git"), &target)
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::{
        create_checkpoint, has_checkpoints, list_checkpoints, recover_abandoned_temp_rewind,
        TempRewindMarker, TEMP_REWIND_MARKER_REL,
    };
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    /// A project that arrived with someone else's `.umadev/checkpoints.git`:
    /// a real store, plus executable hooks everywhere a hooks path could point
    /// and a config naming them. Returns the store's only checkpoint.
    fn project_with_shipped_store(temp: &Path) -> (PathBuf, String) {
        let author = temp.join("author");
        std::fs::create_dir_all(&author).unwrap();
        std::fs::write(author.join("app.rs"), "author's tree").unwrap();
        let head = create_checkpoint(&author, "phase").expect("author checkpoint");

        let victim = temp.join("victim");
        copy_tree(&author, &victim);
        std::fs::write(victim.join("app.rs"), "victim's work").unwrap();
        let store = victim.join(".umadev/checkpoints.git");
        for hooks in ["disabled-hooks", "hooks"] {
            std::fs::create_dir_all(store.join(hooks)).unwrap();
            for name in [
                "reference-transaction",
                "post-index-change",
                "post-checkout",
            ] {
                let hook = store.join(hooks).join(name);
                let marker = temp.join(format!("{name}-ran"));
                std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))
                    .unwrap();
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let config = std::fs::read_to_string(store.join("config")).unwrap();
        std::fs::write(
            store.join("config"),
            format!("{config}[core]\n\thooksPath = hooks\n"),
        )
        .unwrap();
        (victim, head)
    }

    fn markers(temp: &Path) -> Vec<String> {
        std::fs::read_dir(temp)
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with("-ran"))
            .collect()
    }

    #[test]
    fn a_shipped_store_is_never_used_and_its_hooks_never_run() {
        if !git_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (victim, shipped_head) = project_with_shipped_store(temp.path());

        assert!(
            !is_trusted(&victim),
            "another project's stamp must not match"
        );
        assert!(!has_checkpoints(&victim));
        assert!(list_checkpoints(&victim).is_empty());

        let id = create_checkpoint(&victim, "phase").expect("a fresh store takes over");
        assert_ne!(id, shipped_head);
        assert!(is_trusted(&victim));
        assert_eq!(list_checkpoints(&victim).len(), 1);
        let set_aside = victim.join(".umadev").join(SET_ASIDE_DIR);
        assert!(set_aside
            .join("disabled-hooks/reference-transaction")
            .is_file());
        assert_eq!(markers(temp.path()), Vec::<String>::new());
    }

    #[test]
    fn a_shipped_store_and_marker_never_drive_the_startup_heal() {
        if !git_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let (victim, shipped_head) = project_with_shipped_store(temp.path());
        let marker = TempRewindMarker {
            head: shipped_head,
            to: String::new(),
            pid: 0,
            started_at: 1,
            boot: "another-boot".to_string(),
            host: "another-host".to_string(),
        };
        std::fs::write(
            victim.join(TEMP_REWIND_MARKER_REL),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();

        let _ = recover_abandoned_temp_rewind(&victim);
        assert_eq!(
            std::fs::read_to_string(victim.join("app.rs")).unwrap(),
            "victim's work",
            "a shipped checkpoint was restored over the work tree"
        );
        assert_eq!(markers(temp.path()), Vec::<String>::new());
    }

    #[test]
    fn a_redirecting_entry_revokes_trust_in_an_owned_store() {
        if !git_available() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("app.rs"), "one").unwrap();
        create_checkpoint(&root, "phase").expect("checkpoint");
        assert!(is_trusted(&root));
        assert!(has_checkpoints(&root));

        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            root.join(".umadev/checkpoints.git/commondir"),
            elsewhere.to_string_lossy().as_bytes(),
        )
        .unwrap();
        assert!(!is_trusted(&root));
        assert!(!has_checkpoints(&root));
    }
}
