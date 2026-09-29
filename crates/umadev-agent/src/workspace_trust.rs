//! Whether the user trusts a project on this machine.
//!
//! A cloned repository controls more than its source: `.umadevrc` and saved run
//! state under `.umadev/` can ask for more autonomy, and every base CLI reads
//! project-level configuration of its own (Claude's `.claude/settings*.json`
//! hooks and permission allow lists, `.mcp.json` servers, `opencode.json`
//! plugins, Codex's `.codex/config.toml`). Opening a project UmaDev has not
//! been told to trust must not let any of that run code or widen what a tier
//! allows. So, like folder trust in an editor, the first time a project is
//! opened the user is asked whether to trust it, and the answer is kept where
//! the repository cannot write: in the user's state directory, one file per
//! project, named by an HMAC of the canonical project path under the
//! installation key (the scheme approval memory uses). Nothing inside the
//! project is read for it.
//!
//! An untrusted project runs at most at [`TrustMode::Guarded`], and its bases
//! are launched with project-level vendor configuration disabled where the
//! vendor supports that. In any project, the tier a repository file may pick is
//! at most Guarded: Auto is only ever a choice the user makes on this machine.
//!
//! Everything here fails closed: without a state directory or installation key
//! no decision can be read or kept, so the project stays untrusted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::checkpoint::store_trust::{
    installation_state_root, installation_tag, project_root_bytes,
};
use crate::TrustMode;

const DECISION_DIR: &str = "workspace-trust";
const NAME_DOMAIN: &[u8] = b"umadev.workspace-trust.v1";
const MAX_DECISION_BYTES: u64 = 4 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct Decision {
    trusted: bool,
}

/// This project's decision file, relative to the state directory.
fn relative_path(project_root: &Path) -> Option<PathBuf> {
    let name = installation_tag(NAME_DOMAIN, &[project_root_bytes(project_root)?.as_slice()])?;
    Some(Path::new(DECISION_DIR).join(format!("{name}.json")))
}

/// The user's recorded decision for `project_root`: `Some(true)` trusted,
/// `Some(false)` explicitly not trusted, `None` never asked (or unreadable).
#[must_use]
pub fn decision(project_root: &Path) -> Option<bool> {
    let relative = relative_path(project_root)?;
    let state = installation_state_root(false)?;
    let bytes = state.read_bounded(&relative, MAX_DECISION_BYTES).ok()?;
    serde_json::from_slice::<Decision>(&bytes)
        .ok()
        .map(|decision| decision.trusted)
}

/// Whether the user has trusted `project_root` on this machine.
#[must_use]
pub fn is_trusted(project_root: &Path) -> bool {
    decision(project_root) == Some(true)
}

/// Record the user's decision for `project_root`, replacing any earlier one.
///
/// # Errors
/// Fails when the project path, the installation key or the state directory
/// is unavailable, or the write fails; nothing is then recorded.
pub fn record(project_root: &Path, trusted: bool) -> std::io::Result<()> {
    let unavailable = || {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no UmaDev state directory to record the project trust decision in",
        )
    };
    let relative = relative_path(project_root).ok_or_else(unavailable)?;
    let state = installation_state_root(true).ok_or_else(unavailable)?;
    let text = serde_json::to_string(&Decision { trusted }).map_err(std::io::Error::other)?;
    state.ensure_dir(Path::new(DECISION_DIR), false)?;
    state.atomic_write(&relative, text.as_bytes(), false)
}

/// The most autonomous tier a project may run at: Auto only when the user
/// trusts it, Guarded otherwise.
#[must_use]
pub const fn tier_ceiling(trusted: bool) -> TrustMode {
    if trusted {
        TrustMode::Auto
    } else {
        TrustMode::Guarded
    }
}

/// `mode`, lowered to Guarded when it asks for Auto that `trusted` does not
/// allow. Plan and Guarded pass through unchanged.
#[must_use]
pub fn cap_tier(mode: TrustMode, trusted: bool) -> TrustMode {
    if mode == TrustMode::Auto && tier_ceiling(trusted) != TrustMode::Auto {
        TrustMode::Guarded
    } else {
        mode
    }
}

/// The tier to resume saved run state at. The tier a saved run recorded is
/// honored only in a trusted project whose run state this installation wrote
/// (or the user adopted after reviewing it); otherwise the repository could
/// have written it, and it is capped at Guarded.
#[must_use]
pub fn resume_tier(project_root: &Path, saved: TrustMode, trusted: bool) -> TrustMode {
    if trusted && crate::run_provenance::is_own(project_root) {
        saved
    } else {
        cap_tier(saved, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_is_undecided_until_the_user_records_a_decision() {
        let project = tempfile::TempDir::new().unwrap();
        assert_eq!(decision(project.path()), None);
        assert!(!is_trusted(project.path()));

        record(project.path(), true).unwrap();
        assert_eq!(decision(project.path()), Some(true));

        // Revoking keeps an explicit "not trusted", so the user is not asked again.
        record(project.path(), false).unwrap();
        assert_eq!(decision(project.path()), Some(false));
        assert!(!is_trusted(project.path()));
    }

    #[test]
    fn the_decision_lives_outside_the_project_and_a_repository_cannot_ship_one() {
        let project = tempfile::TempDir::new().unwrap();
        // Whatever a repository ships under its own tree is never consulted.
        let shipped = project.path().join(".umadev");
        std::fs::create_dir_all(&shipped).unwrap();
        for name in ["workspace-trust.json", "trust.json"] {
            std::fs::write(shipped.join(name), r#"{"trusted":true}"#).unwrap();
        }
        assert_eq!(decision(project.path()), None);

        let before = walk(project.path());
        record(project.path(), true).unwrap();
        assert_eq!(
            walk(project.path()),
            before,
            "recording a decision wrote into the project"
        );
    }

    #[test]
    fn decisions_are_per_project() {
        let trusted = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();
        record(trusted.path(), true).unwrap();
        assert!(is_trusted(trusted.path()));
        assert_eq!(decision(other.path()), None);
    }

    #[test]
    fn only_a_trusted_project_may_run_in_auto() {
        for mode in [TrustMode::Plan, TrustMode::Guarded, TrustMode::Auto] {
            assert_eq!(cap_tier(mode, true), mode);
        }
        assert_eq!(cap_tier(TrustMode::Auto, false), TrustMode::Guarded);
        assert_eq!(cap_tier(TrustMode::Guarded, false), TrustMode::Guarded);
        assert_eq!(cap_tier(TrustMode::Plan, false), TrustMode::Plan);
    }

    #[test]
    fn a_saved_auto_tier_resumes_only_in_a_trusted_project_this_installation_ran() {
        let project = tempfile::TempDir::new().unwrap();
        let state = project.path().join(".umadev");
        std::fs::create_dir_all(&state).unwrap();
        // Run state the repository shipped: never stamped by this installation.
        std::fs::write(state.join("workflow-state.json"), "{}").unwrap();
        assert_eq!(
            resume_tier(project.path(), TrustMode::Auto, true),
            TrustMode::Guarded
        );

        // Once the user adopts it, a trusted project resumes the saved tier...
        assert!(crate::run_provenance::adopt(project.path()));
        assert_eq!(
            resume_tier(project.path(), TrustMode::Auto, true),
            TrustMode::Auto
        );
        // ...and an untrusted one still never runs in Auto.
        assert_eq!(
            resume_tier(project.path(), TrustMode::Auto, false),
            TrustMode::Guarded
        );
        assert_eq!(
            resume_tier(project.path(), TrustMode::Plan, false),
            TrustMode::Plan
        );
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                out.push(path);
            }
        }
        out.sort();
        out
    }
}
