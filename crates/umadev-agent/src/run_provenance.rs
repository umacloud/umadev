//! Which saved run state this installation wrote.
//!
//! `/continue` (and its natural-language aliases such as 「继续」) resumes the
//! plan, workflow state and review cursor saved under `.umadev/`. Those files
//! are ordinary project files, so a repository can ship them: a user who types
//! 「继续」 in a freshly cloned project would then drive steps and gates that
//! somebody else wrote and the user never saw. Every time UmaDev writes one of
//! them it records an HMAC of the file's exact bytes, bound to the canonical
//! project path and keyed by the installation key, in the user's state
//! directory. State is resumed as-is only when every such file on disk still
//! matches what this installation last wrote; otherwise the surface shows the
//! plan and resumes only after the user adopts it explicitly ([`adopt`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::checkpoint::store_trust::{installation_tag, project_root_bytes};

/// The saved-run files a resume acts on, all under `<project>/.umadev/`.
pub(crate) const PLAN: &str = "plan.json";
pub(crate) const WORKFLOW_STATE: &str = "workflow-state.json";
pub(crate) const REVIEW_CHECKPOINT: &str = "director-operational-review.json";
const RUN_FILES: [&str; 3] = [PLAN, WORKFLOW_STATE, REVIEW_CHECKPOINT];

const STAMP_DIR: &str = "run-state";
const NAME_DOMAIN: &[u8] = b"umadev.run-state-owner.v1";
const CONTENT_DOMAIN: &[u8] = b"umadev.run-state-content.v1";
const MAX_RUN_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_STAMP_BYTES: u64 = 16 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stamps {
    #[serde(default)]
    files: BTreeMap<String, String>,
}

fn stamp_path(root_bytes: &[u8]) -> Option<PathBuf> {
    let name = installation_tag(NAME_DOMAIN, &[root_bytes])?;
    Some(Path::new(STAMP_DIR).join(format!("{name}.json")))
}

fn content_tag(root_bytes: &[u8], file: &str, bytes: &[u8]) -> Option<String> {
    installation_tag(CONTENT_DOMAIN, &[root_bytes, file.as_bytes(), bytes])
}

fn read_stamps(state: &umadev_state::fs::RootedDir, relative: &Path) -> Stamps {
    state
        .read_bounded(relative, MAX_STAMP_BYTES)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn read_run_file(project_root: &Path, file: &str) -> Option<Vec<u8>> {
    umadev_state::fs::read_bounded_beneath(
        project_root,
        &Path::new(".umadev").join(file),
        MAX_RUN_FILE_BYTES,
    )
    .ok()
}

/// Record that this installation just wrote `bytes` to `.umadev/<file>`.
/// Best-effort: without a state directory the file simply stays unowned, and a
/// later resume asks the user to adopt it.
pub(crate) fn record(project_root: &Path, file: &str, bytes: &[u8]) {
    let _ = record_all(project_root, &[(file, bytes)]);
}

fn record_all(project_root: &Path, files: &[(&str, &[u8])]) -> Option<()> {
    let root_bytes = project_root_bytes(project_root)?;
    let relative = stamp_path(&root_bytes)?;
    let state = umadev_state::privacy::state_root(true)?;
    let mut stamps = read_stamps(&state, &relative);
    for (file, bytes) in files {
        stamps
            .files
            .insert((*file).to_string(), content_tag(&root_bytes, file, bytes)?);
    }
    let text = serde_json::to_vec_pretty(&stamps).ok()?;
    state.ensure_dir(Path::new(STAMP_DIR), false).ok()?;
    state.atomic_write(&relative, &text, false).ok()
}

/// Whether every saved-run file present in `project_root` is exactly what this
/// installation last wrote there. `true` when none is present.
#[must_use]
pub fn is_own(project_root: &Path) -> bool {
    let present: Vec<(&str, Vec<u8>)> = RUN_FILES
        .iter()
        .filter_map(|file| Some((*file, read_run_file(project_root, file)?)))
        .collect();
    if present.is_empty() {
        return true;
    }
    let Some(root_bytes) = project_root_bytes(project_root) else {
        return false;
    };
    let Some(stamps) = stamp_path(&root_bytes).and_then(|relative| {
        umadev_state::privacy::state_root(false).map(|state| read_stamps(&state, &relative))
    }) else {
        return false;
    };
    present.iter().all(|(file, bytes)| {
        stamps.files.get(*file).is_some_and(|stamp| {
            content_tag(&root_bytes, file, bytes).is_some_and(|tag| &tag == stamp)
        })
    })
}

/// Take ownership of the saved-run files currently in `project_root`, after the
/// user has seen the plan and explicitly chosen to run it. Returns `false` when
/// the ownership could not be stored (the resume may still go ahead, since the
/// user consented, but the next one asks again).
pub fn adopt(project_root: &Path) -> bool {
    let present: Vec<(&str, Vec<u8>)> = RUN_FILES
        .iter()
        .filter_map(|file| Some((*file, read_run_file(project_root, file)?)))
        .collect();
    let files: Vec<(&str, &[u8])> = present
        .iter()
        .map(|(file, bytes)| (*file, bytes.as_slice()))
        .collect();
    record_all(project_root, &files).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, file: &str, body: &str) {
        std::fs::create_dir_all(root.join(".umadev")).unwrap();
        std::fs::write(root.join(".umadev").join(file), body).unwrap();
    }

    #[test]
    fn only_state_this_installation_wrote_is_its_own() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(is_own(tmp.path()), "no saved state is nothing foreign");

        // A shipped plan was never recorded.
        write(tmp.path(), PLAN, r#"{"steps":[]}"#);
        assert!(!is_own(tmp.path()));

        // What UmaDev writes (and records) is its own ...
        write(tmp.path(), PLAN, r#"{"steps":[1]}"#);
        record(tmp.path(), PLAN, br#"{"steps":[1]}"#);
        assert!(is_own(tmp.path()));

        // ... until someone else changes it or adds another run file.
        write(
            tmp.path(),
            WORKFLOW_STATE,
            r#"{"active_gate":"docs_confirm"}"#,
        );
        assert!(!is_own(tmp.path()));
        assert!(adopt(tmp.path()));
        assert!(is_own(tmp.path()));
        write(tmp.path(), PLAN, r#"{"steps":[2]}"#);
        assert!(!is_own(tmp.path()));
    }
}
