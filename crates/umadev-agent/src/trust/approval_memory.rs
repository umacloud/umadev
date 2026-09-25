//! Remembered approvals, kept where a repository cannot put them.
//!
//! The allow-rules that let an approved action class skip its prompt used to
//! live in `<project>/.umadev/trust.json`, so a cloned repository could ship
//! rules the user never granted (`shell:<payload>`, `write_out_of_tree:<home>`),
//! and a model could write them after one approved in-tree edit. They now live
//! in the user's `UmaDev` state directory, one file per project, named by an
//! HMAC of the canonical project path under the installation key (the same key
//! that stamps checkpoint stores). Nothing inside the project is read for them,
//! and the file name does not reveal which project it belongs to.
//!
//! Everything here fails open toward asking: without a state directory or
//! installation key nothing is remembered, so every approval is asked again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MEMORY_DIR: &str = "approvals";
const NAME_DOMAIN: &[u8] = b"umadev.approval-memory.v1";
const MAX_MEMORY_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ApprovalMemory {
    #[serde(default)]
    allow_rules: BTreeSet<String>,
}

/// This project's memory file, relative to the state directory.
fn relative_path(project_root: &Path) -> Option<PathBuf> {
    let key = crate::checkpoint::store_trust::installation_key()?;
    let canonical = std::fs::canonicalize(project_root).ok()?;
    let tag = umadev_governance::privacy_fingerprint(
        &key,
        NAME_DOMAIN,
        canonical.as_os_str().as_encoded_bytes(),
    );
    let name: String = tag.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(Path::new(MEMORY_DIR).join(format!("{name}.json")))
}

fn read(state: &umadev_state::fs::RootedDir, relative: &Path) -> ApprovalMemory {
    state
        .read_bounded(relative, MAX_MEMORY_BYTES)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The approval classes remembered for `project_root`; empty when none were
/// recorded or the state directory is unavailable.
pub(super) fn load(project_root: &Path) -> BTreeSet<String> {
    let Some(relative) = relative_path(project_root) else {
        return BTreeSet::new();
    };
    umadev_state::privacy::state_root(false)
        .map(|state| read(&state, &relative).allow_rules)
        .unwrap_or_default()
}

/// Add `class` to this project's remembered approvals. Returns `true` when it
/// was new and has been stored.
pub(super) fn remember(project_root: &Path, class: String) -> bool {
    let Some(relative) = relative_path(project_root) else {
        return false;
    };
    let Some(state) = umadev_state::privacy::state_root(true) else {
        return false;
    };
    let mut memory = read(&state, &relative);
    if !memory.allow_rules.insert(class) {
        return false;
    }
    let Ok(text) = serde_json::to_string_pretty(&memory) else {
        return false;
    };
    text.len() as u64 <= MAX_MEMORY_BYTES
        && state
            .ensure_dir(Path::new(MEMORY_DIR), false)
            .and_then(|()| state.atomic_write(&relative, text.as_bytes(), false))
            .is_ok()
}
