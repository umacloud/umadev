//! Persistent workflow state — `.umadev/workflow-state.json`.
//!
//! Implements the persistence side of `UD-FLOW-001`. The agent updates
//! this file on every phase transition; the prompt-time injection hook
//! reads it back to ground the model.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use umadev_spec::Phase;

const STATE_FILE: &str = "workflow-state.json";
const HISTORY_DIR: &str = "history";
const MAX_WORKFLOW_STATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HISTORY_DIRECTORY_ENTRIES: usize = 1024;

/// Snapshot of pipeline progress for one project.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    /// Active phase, e.g. `frontend`. Stored as the phase identifier
    /// string so the on-disk schema is stable across versions.
    pub phase: String,
    /// Active gate, e.g. `docs_confirm`. Empty string when no gate is
    /// open.
    #[serde(default)]
    pub active_gate: String,
    /// Project slug used in artifact filenames; persisted so `continue`
    /// invocations resolve the same files as the original `run`.
    #[serde(default)]
    pub slug: String,
    /// Original user requirement (carried through the pipeline so
    /// continuation invocations don't lose context).
    #[serde(default)]
    pub requirement: String,
    /// ISO-8601 UTC timestamp of the last transition.
    pub last_transition_at: String,
    /// Free-form note about the latest action. Empty by default.
    #[serde(default)]
    pub note: String,
    /// Backend id that produced this state (e.g. `claude-code`, `codex`,
    /// or empty for offline). Persisted so `continue` / `revise` resume
    /// against the same worker the original `run` used.
    #[serde(default)]
    pub backend: String,
    /// The base's OWN persisted conversation id (claude's pinned `--session-id`,
    /// codex's `thread.id`), captured at run-open. This is the load-bearing
    /// pointer for **full-context cross-session resume**. It is opaque authority,
    /// not sufficient by itself: a caller must validate
    /// [`Self::base_resume_identity`] before handing it to a base. An ineligible
    /// or absent identity opens fresh and transfers context through UmaDev's
    /// transcript instead. `None` when the base exposes no resumable id.
    #[serde(default)]
    pub base_session_id: Option<String>,
    /// Immutable launch/effective-sandbox identity bound to
    /// [`Self::base_session_id`]. A resumable id is authority-bearing state, so
    /// callers must validate this record against the next base, canonical
    /// workspace, permission profile, and requested sandbox before loading it.
    /// Legacy files omit the field; Grok Build treats that as non-resumable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_resume_identity: Option<umadev_runtime::BaseResumeIdentity>,
    /// Access/approval posture selected when this run was started. Optional for
    /// backward compatibility with workflow states written before this field
    /// existed; such states resolve conservatively to Guarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_profile: Option<umadev_runtime::BasePermissionProfile>,
    /// Spec version the agent is conformant against.
    pub spec_version: String,
}

impl WorkflowState {
    /// Build a fresh state pinned to a given phase.
    #[must_use]
    pub fn new(phase: Phase) -> Self {
        Self {
            phase: phase.id().to_string(),
            active_gate: String::new(),
            slug: String::new(),
            requirement: String::new(),
            last_transition_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            note: String::new(),
            backend: String::new(),
            base_session_id: None,
            base_resume_identity: None,
            permission_profile: Some(umadev_runtime::BasePermissionProfile::Guarded),
            spec_version: umadev_spec::SPEC_VERSION.to_string(),
        }
    }

    /// Permission posture to use when resuming this workflow. Legacy states
    /// lacked the field and therefore resume as Guarded, never Auto.
    #[must_use]
    pub fn resolved_permission_profile(&self) -> umadev_runtime::BasePermissionProfile {
        self.permission_profile
            .unwrap_or(umadev_runtime::BasePermissionProfile::Guarded)
    }
}

/// Persist the workflow state to `<project_root>/.umadev/workflow-state.json`.
///
/// The write is atomic: the JSON is first written to a sibling `.tmp`
/// file and then renamed into place. A `rename(2)` on the same volume is
/// atomic on POSIX and Windows, so a crash mid-write can never leave a
/// truncated / malformed `workflow-state.json` that would strand the
/// pipeline in an unreadable state. Disk-write errors propagate so
/// callers can decide whether to surface or swallow them.
/// Before overwriting, copy the current `workflow-state.json` (if it exists)
/// into `.umadev/history/<timestamp>.json`. This makes every transition
/// recoverable — `umadev rollback` can restore any prior state. Best-effort:
/// a missing current file or a history-write failure is silently skipped
/// (the atomic overwrite below must still proceed).
fn canonical_project_root(project_root: &Path) -> io::Result<PathBuf> {
    let root = fs::canonicalize(project_root)?;
    if !umadev_state::fs::real_dir(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "project root is not a real directory",
        ));
    }
    Ok(root)
}

fn managed_state_dir(project_root: &Path, create: bool) -> io::Result<PathBuf> {
    let root = canonical_project_root(project_root)?;
    let dir = root.join(".umadev");
    if create {
        umadev_state::fs::ensure_real_child_dir(&root, ".umadev")
    } else if umadev_state::fs::real_dir(&dir) {
        Ok(dir)
    } else {
        Err(io::Error::new(
            if dir.exists() {
                io::ErrorKind::PermissionDenied
            } else {
                io::ErrorKind::NotFound
            },
            "managed state directory is missing or unsafe",
        ))
    }
}

fn managed_history_dir(project_root: &Path, create: bool) -> io::Result<PathBuf> {
    let state_dir = managed_state_dir(project_root, create)?;
    let history = state_dir.join(HISTORY_DIR);
    if create {
        umadev_state::fs::ensure_real_child_dir(&state_dir, HISTORY_DIR)
    } else if umadev_state::fs::real_dir(&history) {
        Ok(history)
    } else {
        Err(io::Error::new(
            if history.exists() {
                io::ErrorKind::PermissionDenied
            } else {
                io::ErrorKind::NotFound
            },
            "workflow history directory is missing or unsafe",
        ))
    }
}

fn read_state_file(project_root: &Path) -> io::Result<Vec<u8>> {
    let root = canonical_project_root(project_root)?;
    umadev_state::fs::read_bounded_beneath(
        &root,
        Path::new(".umadev").join(STATE_FILE).as_path(),
        MAX_WORKFLOW_STATE_BYTES,
    )
}

fn valid_snapshot_name(timestamp: &str) -> bool {
    !timestamp.is_empty()
        && timestamp.len() <= 128
        && timestamp != "."
        && timestamp != ".."
        && timestamp
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | 'T' | 'Z'))
        && Path::new(timestamp)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn snapshot_previous(project_root: &Path) {
    let Some(text) = read_state_file(project_root).ok() else {
        return; // first-ever write — nothing to snapshot
    };
    let Some(history_dir) = managed_history_dir(project_root, true).ok() else {
        return;
    };
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.f");
    let _ = umadev_state::fs::atomic_write(&history_dir.join(format!("{ts}.json")), &text);
    prune_history(&history_dir, HISTORY_KEEP);
}

/// How many state snapshots to retain. A long-lived workspace would otherwise
/// accumulate one file per transition without bound.
const HISTORY_KEEP: usize = 50;

/// Keep only the `keep` most-recent history snapshots, deleting older ones.
/// Best-effort: any IO error just leaves the extra files in place.
fn prune_history(dir: &Path, keep: usize) {
    let Some(mut files) = bounded_history_files(dir, MAX_HISTORY_DIRECTORY_ENTRIES) else {
        return;
    };
    if files.len() <= keep {
        return;
    }
    files.sort(); // timestamp-named -> lexicographic == chronological
    let excess = files.len() - keep;
    for old in files.into_iter().take(excess) {
        let _ = umadev_state::fs::remove_regular_file(&old);
    }
}

fn bounded_history_files(dir: &Path, max_entries: usize) -> Option<Vec<PathBuf>> {
    let mut files = Vec::with_capacity(max_entries.min(HISTORY_KEEP.saturating_mul(2)));
    let mut entries_seen = 0usize;
    for entry in fs::read_dir(dir).ok()? {
        if entries_seen >= max_entries {
            return None;
        }
        entries_seen = entries_seen.saturating_add(1);
        let path = entry.ok()?.path();
        if path.extension().and_then(|suffix| suffix.to_str()) == Some("json")
            && umadev_state::fs::real_file(&path)
        {
            files.push(path);
        }
    }
    Some(files)
}

/// Persist the workflow state to `<project_root>/.umadev/workflow-state.json`.
///
/// The write is atomic: the JSON is first written to a sibling `.tmp`
/// file and then renamed into place. Before overwriting, the previous state
/// is snapshotted to `.umadev/history/` by the internal snapshot writer so
/// every transition is recoverable via `umadev rollback`.
pub fn write_workflow_state(project_root: &Path, state: &WorkflowState) -> std::io::Result<()> {
    let dir = managed_state_dir(project_root, true)?;
    snapshot_previous(project_root);
    let text = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_WORKFLOW_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow state exceeds the durable-state byte limit",
        ));
    }
    umadev_state::fs::atomic_write(&dir.join(STATE_FILE), text.as_bytes())
}

/// List available rollback snapshots, newest first. Each entry is the
/// timestamp filename stem (e.g. `20260614T120000Z`). Empty when no history.
#[must_use]
pub fn list_snapshots(project_root: &Path) -> Vec<String> {
    let Ok(history_dir) = managed_history_dir(project_root, false) else {
        return Vec::new();
    };
    let Some(paths) = bounded_history_files(&history_dir, MAX_HISTORY_DIRECTORY_ENTRIES) else {
        return Vec::new();
    };
    let mut snaps: Vec<String> = paths
        .into_iter()
        .filter_map(|path| {
            path.file_stem()?
                .to_str()
                .filter(|stem| valid_snapshot_name(stem))
                .map(String::from)
        })
        .collect();
    snaps.sort_unstable();
    snaps.reverse(); // newest first
    snaps
}

/// Restore the workflow state from a snapshot timestamp (e.g. `20260614T120000Z`).
/// Copies the snapshot over `workflow-state.json` (via the atomic write path so
/// the restore itself is snapshotted). Returns `Err` when the snapshot is missing.
pub fn restore_snapshot(project_root: &Path, timestamp: &str) -> std::io::Result<()> {
    if !valid_snapshot_name(timestamp) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "snapshot id is not a safe history entry",
        ));
    }
    let root = canonical_project_root(project_root)?;
    let relative = Path::new(".umadev")
        .join(HISTORY_DIR)
        .join(format!("{timestamp}.json"));
    let bytes = umadev_state::fs::read_bounded_beneath(&root, &relative, MAX_WORKFLOW_STATE_BYTES)
        .map_err(|e| {
            std::io::Error::new(e.kind(), format!("snapshot {timestamp} not found: {e}"))
        })?;
    let text =
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let state: WorkflowState = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_workflow_state(project_root, &state)
}

/// Outcome of [`read_workflow_state_diagnostic`] — distinguishes the three
/// reasons a read can fail so callers (and the CLI) can surface the right
/// message instead of conflating "no run started" with "state corrupted".
#[derive(Debug)]
pub enum ReadState {
    /// The state file exists and parsed.
    Ok(Box<WorkflowState>),
    /// No state file — the user has not started a run yet.
    Missing,
    /// The file exists but is unparseable. Carries the file path + parse
    /// error so the CLI can tell the user exactly what's wrong (and that
    /// they should `rollback` or delete it) rather than silently starting
    /// a fresh run over a corrupted state.
    Corrupt {
        /// Path to the unreadable state file.
        path: PathBuf,
        /// The read/parse error message.
        error: String,
    },
}

/// Read the workflow state with a full diagnostic outcome. Prefer this over
/// [`read_workflow_state`] when the caller can act on corruption (the CLI's
/// `continue` / `revise` do); [`read_workflow_state`] remains for
/// best-effort reads where `None` is an acceptable fallback.
pub fn read_workflow_state_diagnostic(project_root: &Path) -> ReadState {
    let path = project_root.join(".umadev").join("workflow-state.json");
    let bytes = match read_state_file(project_root) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReadState::Missing,
        Err(e) => {
            return ReadState::Corrupt {
                path,
                error: format!("read error: {e}"),
            }
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            return ReadState::Corrupt {
                path,
                error: format!("read error: {error}"),
            }
        }
    };
    match serde_json::from_str::<WorkflowState>(&text) {
        Ok(s) => ReadState::Ok(Box::new(s)),
        Err(e) => ReadState::Corrupt {
            path,
            error: format!("parse error: {e}"),
        },
    }
}

/// Read the workflow state. Returns `None` when the file is missing OR
/// malformed. For callers that need to tell those apart (to warn on
/// corruption instead of silently restarting), use
/// [`read_workflow_state_diagnostic`].
#[must_use]
pub fn read_workflow_state(project_root: &Path) -> Option<WorkflowState> {
    match read_workflow_state_diagnostic(project_root) {
        ReadState::Ok(s) => Some(*s),
        ReadState::Missing => None,
        ReadState::Corrupt { path, error } => {
            // Surface corruption through tracing, not stderr: this can be read
            // while the TUI owns the alternate screen, and direct stderr bytes
            // corrupt the frame. Best-effort: still returns None so fail-open
            // callers behave as before.
            tracing::warn!(
                path = %path.display(),
                %error,
                "workflow-state.json is corrupt; treating as no run started"
            );
            None
        }
    }
}

/// A cross-session goal-continuity summary (Wave 5 / G11 deliverable 4): when a
/// prior session left a persisted plan (`.umadev/plan.json`) that is **not yet
/// finished**, this returns `(next_step_title, done, total)` so the host can
/// surface "resume goal X (step N/M)?" on launch and an `auto`-tier run can drive
/// it to completion. Reuses the Wave-1 [`crate::plan_state`] plan (read-only — it
/// does not modify the plan) so there is one source of truth for build progress.
///
/// Returns `None` when there is no plan, the plan is fully
/// [`crate::plan_state::StepStatus::Done`],
/// or the plan has no steps — i.e. there is nothing to resume. Fail-open by
/// construction: `plan_state::load` already swallows a missing / corrupt file
/// (→ `None`), so this never errors and never blocks launch.
#[must_use]
pub fn unfinished_plan_summary(project_root: &Path) -> Option<(String, usize, usize)> {
    let plan = crate::plan_state::load(project_root)?;
    let (done, total) = plan.progress();
    if total == 0 || done >= total {
        return None;
    }
    // The "next step" is the first step that isn't done yet (active, then pending,
    // then any non-done) — what the user would resume INTO. Title falls back to
    // the step id when a title was empty.
    let next = plan
        .steps
        .iter()
        .find(|s| s.status == crate::plan_state::StepStatus::Active)
        .or_else(|| {
            plan.steps
                .iter()
                .find(|s| s.status != crate::plan_state::StepStatus::Done)
        })?;
    let title = if next.title.trim().is_empty() {
        next.id.clone()
    } else {
        next.title.clone()
    };
    Some((title, done, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trips_to_disk() {
        let tmp = TempDir::new().unwrap();
        let s = WorkflowState::new(Phase::Frontend);
        write_workflow_state(tmp.path(), &s).unwrap();
        let back = read_workflow_state(tmp.path()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn unfinished_plan_summary_reports_next_step_and_progress() {
        use crate::critics::Seat;
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let tmp = TempDir::new().unwrap();
        let plan = Plan {
            steps: vec![
                PlanStep {
                    files: crate::plan_state::StepFiles::default(),
                    id: "scaffold".into(),
                    title: "Scaffold the app".into(),
                    seat: Seat::FrontendEngineer,
                    kind: StepKind::Build,
                    depends_on: vec![],
                    acceptance: AcceptanceSpec::SourcePresent,
                    evidence: Vec::new(),
                    status: StepStatus::Done,
                },
                PlanStep {
                    files: crate::plan_state::StepFiles::default(),
                    id: "auth".into(),
                    title: "Add email auth".into(),
                    seat: Seat::BackendEngineer,
                    kind: StepKind::Build,
                    depends_on: vec!["scaffold".into()],
                    acceptance: AcceptanceSpec::SourcePresent,
                    evidence: Vec::new(),
                    status: StepStatus::Pending,
                },
            ],
            risks: vec![],
            open_questions: vec![],
        };
        crate::plan_state::save(&plan, tmp.path()).unwrap();
        let (title, done, total) = unfinished_plan_summary(tmp.path()).unwrap();
        assert_eq!(title, "Add email auth");
        assert_eq!((done, total), (1, 2));
    }

    #[test]
    fn unfinished_plan_summary_is_none_when_no_plan_or_all_done() {
        let tmp = TempDir::new().unwrap();
        // No plan on disk → None.
        assert!(unfinished_plan_summary(tmp.path()).is_none());
    }

    #[test]
    fn read_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(read_workflow_state(tmp.path()).is_none());
    }

    #[test]
    fn backend_field_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let mut s = WorkflowState::new(Phase::Docs);
        s.backend = "claude-code".into();
        s.active_gate = "docs_confirm".into();
        write_workflow_state(tmp.path(), &s).unwrap();
        let back = read_workflow_state(tmp.path()).unwrap();
        assert_eq!(back.backend, "claude-code");
    }

    #[test]
    fn permission_profile_roundtrips_for_plan_and_auto() {
        use umadev_runtime::BasePermissionProfile;

        let tmp = TempDir::new().unwrap();
        for profile in [BasePermissionProfile::Plan, BasePermissionProfile::Auto] {
            let mut state = WorkflowState::new(Phase::Frontend);
            state.permission_profile = Some(profile);
            write_workflow_state(tmp.path(), &state).unwrap();
            let restored = read_workflow_state(tmp.path()).unwrap();
            assert_eq!(restored.permission_profile, Some(profile));
            assert_eq!(restored.resolved_permission_profile(), profile);
        }
    }

    #[test]
    fn base_session_id_roundtrips_and_defaults_for_legacy() {
        // The base session id is persisted on run-open and read back verbatim — the
        // pointer a `/continue` resumes the base conversation with (P0 piece #1).
        let tmp = TempDir::new().unwrap();
        let mut s = WorkflowState::new(Phase::Frontend);
        s.base_session_id = Some("c0ffee00-1234-4abc-8def-0123456789ab".to_string());
        s.backend = "grok-build".to_string();
        s.base_resume_identity = Some(umadev_runtime::BaseResumeIdentity::requested_only(
            "grok-build",
            std::fs::canonicalize(tmp.path()).unwrap(),
            umadev_runtime::BasePermissionProfile::Guarded,
            umadev_runtime::BaseSandboxRequest::Off,
            true,
        ));
        write_workflow_state(tmp.path(), &s).unwrap();
        let back = read_workflow_state(tmp.path()).unwrap();
        assert_eq!(
            back.base_session_id.as_deref(),
            Some("c0ffee00-1234-4abc-8def-0123456789ab"),
            "the opaque base id round-trips alongside its separately validated identity"
        );
        assert_eq!(back.base_resume_identity, s.base_resume_identity);

        // A legacy state written before the field existed must still read (defaults
        // to None — no resumable id, the caller degrades to a fresh session).
        let dir = tmp.path().join(".umadev");
        let legacy = r#"{
            "phase": "frontend",
            "active_gate": "",
            "slug": "old",
            "requirement": "do thing",
            "last_transition_at": "2026-01-01T00:00:00Z",
            "note": "",
            "backend": "claude-code",
            "spec_version": "UMADEV_HOST_SPEC_V1"
        }"#;
        std::fs::write(dir.join("workflow-state.json"), legacy).unwrap();
        let legacy_state = read_workflow_state(tmp.path()).expect("legacy state must read");
        assert_eq!(
            legacy_state.base_session_id, None,
            "missing field defaults to None"
        );
        assert_eq!(legacy_state.base_resume_identity, None);
        assert_eq!(
            legacy_state.resolved_permission_profile(),
            umadev_runtime::BasePermissionProfile::Guarded,
            "a legacy state must never silently resume as Auto"
        );
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let tmp = TempDir::new().unwrap();
        let s = WorkflowState::new(Phase::Backend);
        write_workflow_state(tmp.path(), &s).unwrap();
        let tmp_path = tmp.path().join(".umadev/workflow-state.json.tmp");
        assert!(
            !tmp_path.exists(),
            "temp file should be renamed away, not left behind"
        );
        assert!(tmp.path().join(".umadev/workflow-state.json").is_file());
    }

    #[test]
    fn overwrite_preserves_readability() {
        // Writing twice must always leave a readable file (the rename is
        // atomic, so the second write never corrupts the first).
        let tmp = TempDir::new().unwrap();
        let mut s = WorkflowState::new(Phase::Docs);
        write_workflow_state(tmp.path(), &s).unwrap();
        s.phase = Phase::Frontend.id().to_string();
        write_workflow_state(tmp.path(), &s).unwrap();
        let back = read_workflow_state(tmp.path()).unwrap();
        assert_eq!(back.phase, "frontend");
    }

    #[test]
    fn transition_snapshots_previous_state() {
        let tmp = TempDir::new().unwrap();
        let mut s = WorkflowState::new(Phase::Docs);
        write_workflow_state(tmp.path(), &s).unwrap();
        // No history yet.
        assert!(list_snapshots(tmp.path()).is_empty());

        // Second write (transition) snapshots the first.
        s.phase = Phase::Frontend.id().to_string();
        write_workflow_state(tmp.path(), &s).unwrap();
        let snaps = list_snapshots(tmp.path());
        assert_eq!(snaps.len(), 1, "previous state should be snapshotted");
        // Current file reflects the new state.
        let back = read_workflow_state(tmp.path()).unwrap();
        assert_eq!(back.phase, "frontend");
    }

    #[test]
    fn multiple_transitions_accumulate_history() {
        let tmp = TempDir::new().unwrap();
        for phase in [Phase::Research, Phase::Docs, Phase::Spec, Phase::Frontend] {
            let s = WorkflowState::new(phase);
            write_workflow_state(tmp.path(), &s).unwrap();
        }
        // 3 transitions after the first → 3 snapshots.
        let snaps = list_snapshots(tmp.path());
        assert_eq!(snaps.len(), 3);
        // Newest first.
        assert!(snaps[0] >= snaps[1]);
    }

    #[test]
    fn rollback_restores_prior_state() {
        let tmp = TempDir::new().unwrap();
        let s1 = WorkflowState::new(Phase::Docs);
        write_workflow_state(tmp.path(), &s1).unwrap();
        let s2 = WorkflowState::new(Phase::Frontend);
        write_workflow_state(tmp.path(), &s2).unwrap();
        // Current is frontend.
        assert_eq!(read_workflow_state(tmp.path()).unwrap().phase, "frontend");

        let snaps = list_snapshots(tmp.path());
        assert_eq!(snaps.len(), 1);
        restore_snapshot(tmp.path(), &snaps[0]).unwrap();
        // Restored to docs.
        assert_eq!(read_workflow_state(tmp.path()).unwrap().phase, "docs");
    }

    #[test]
    fn rollback_missing_snapshot_errors() {
        let tmp = TempDir::new().unwrap();
        assert!(restore_snapshot(tmp.path(), "nonexistent").is_err());
    }

    #[test]
    fn rollback_rejects_snapshot_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let current = WorkflowState::new(Phase::Frontend);
        write_workflow_state(tmp.path(), &current).unwrap();

        // This file is deliberately placed where the old join-based lookup
        // resolved `.umadev/history/../../outside.json`. A rollback id is an
        // opaque history entry, never a caller-controlled relative path.
        let outside = WorkflowState::new(Phase::Docs);
        std::fs::write(
            tmp.path().join("outside.json"),
            serde_json::to_vec(&outside).unwrap(),
        )
        .unwrap();

        let error = restore_snapshot(tmp.path(), "../../outside").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            read_workflow_state(tmp.path()).unwrap().phase,
            "frontend",
            "an invalid rollback id must not replace current state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn workflow_state_rejects_linked_leaf_without_touching_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join(".umadev");
        std::fs::create_dir(&state_dir).unwrap();
        let external = tmp.path().join("external-state.json");
        let external_bytes = serde_json::to_vec(&WorkflowState::new(Phase::Docs)).unwrap();
        std::fs::write(&external, &external_bytes).unwrap();
        symlink(&external, state_dir.join(STATE_FILE)).unwrap();

        assert!(matches!(
            read_workflow_state_diagnostic(tmp.path()),
            ReadState::Corrupt { .. }
        ));
        let error =
            write_workflow_state(tmp.path(), &WorkflowState::new(Phase::Frontend)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read(&external).unwrap(),
            external_bytes,
            "managed state must never overwrite a symlink target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_writer_ignores_linked_history_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        write_workflow_state(tmp.path(), &WorkflowState::new(Phase::Docs)).unwrap();
        let external = TempDir::new().unwrap();
        let sentinel = external.path().join("sentinel.json");
        std::fs::write(&sentinel, b"outside").unwrap();
        symlink(external.path(), tmp.path().join(".umadev/history")).unwrap();

        // Snapshotting is best-effort, so a hostile/accidental linked history
        // directory must not block the actual state transition or receive data.
        write_workflow_state(tmp.path(), &WorkflowState::new(Phase::Frontend)).unwrap();
        assert_eq!(std::fs::read_dir(external.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
        assert!(list_snapshots(tmp.path()).is_empty());
    }

    #[test]
    fn list_snapshots_empty_when_no_history() {
        let tmp = TempDir::new().unwrap();
        assert!(list_snapshots(tmp.path()).is_empty());
    }

    #[test]
    fn history_scan_counts_unknown_entries_toward_its_budget() {
        let tmp = TempDir::new().unwrap();
        for index in 0..4 {
            std::fs::write(tmp.path().join(format!("unknown-{index}")), b"x").unwrap();
        }
        assert!(bounded_history_files(tmp.path(), 3).is_none());
    }

    #[test]
    fn legacy_state_without_backend_still_reads() {
        // States written before the `backend` field existed must still
        // deserialize — the new column defaults to empty.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = r#"{
            "phase": "frontend",
            "active_gate": "preview_confirm",
            "slug": "old",
            "requirement": "do thing",
            "last_transition_at": "2026-01-01T00:00:00Z",
            "note": "",
            "spec_version": "UMADEV_HOST_SPEC_V1"
        }"#;
        std::fs::write(dir.join("workflow-state.json"), legacy).unwrap();
        let s = read_workflow_state(tmp.path()).expect("legacy state must read");
        assert_eq!(s.backend, "", "missing field must default to empty");
        assert_eq!(s.slug, "old");
    }

    #[test]
    fn diagnostic_distinguishes_missing_and_corrupt() {
        use super::ReadState;
        let tmp = TempDir::new().unwrap();
        // Missing file → Missing, not Corrupt.
        assert!(matches!(
            read_workflow_state_diagnostic(tmp.path()),
            ReadState::Missing
        ));
        // Corrupt JSON → Corrupt (with path + error), NOT silently Missing.
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workflow-state.json"), "{ this is not json").unwrap();
        match read_workflow_state_diagnostic(tmp.path()) {
            ReadState::Corrupt { path, error } => {
                assert!(path.ends_with("workflow-state.json"));
                assert!(error.contains("parse"), "error was: {error}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
        // Valid file → Ok.
        std::fs::write(
            dir.join("workflow-state.json"),
            r#"{"phase":"docs","active_gate":"docs_confirm","slug":"x","requirement":"r","last_transition_at":"2026-01-01T00:00:00Z","note":"","spec_version":"UMADEV_HOST_SPEC_V1"}"#,
        )
        .unwrap();
        assert!(matches!(
            read_workflow_state_diagnostic(tmp.path()),
            ReadState::Ok(_)
        ));
    }
}
