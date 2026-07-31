//! Persisted director-loop resume state and document-artifact staleness.
//!
//! A fresh base session can reattach to an interrupted run through its persisted
//! plan.  This module owns the small, read-only decision surface for determining
//! whether such a plan is resumable and for reopening steps whose upstream
//! artifacts changed after the plan was saved.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use umadev_spec::Phase;

use crate::plan_state::{self, Plan, StepStatus};

const OPERATIONAL_REVIEW_CHECKPOINT_FILE: &str = "director-operational-review.json";
const MAX_OPERATIONAL_REVIEW_CHECKPOINT_BYTES: u64 = 16 * 1024;
const MAX_ARTIFACT_VERSION_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARTIFACT_VERSION_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACTIVE_ARTIFACTS: usize = 256;
const MAX_CONSECUTIVE_REVIEW_OUTAGES: u8 = 2;

/// Typed evidence retained at an operationally unavailable review boundary.
///
/// Product findings and reviewer-infrastructure failures have different owners:
/// only the former may inform a later repair run, while only the latter may open
/// the review liveness circuit. Keeping them in separate persisted fields prevents
/// a mixed panel from turning a host outage into a source defect after `/continue`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct OperationalReviewEvidence {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) semantic_blocking: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) operational_unavailable: Vec<String>,
}

impl OperationalReviewEvidence {
    const MAX_ITEMS_PER_CHANNEL: usize = 8;
    const MAX_ITEM_CHARS: usize = 240;

    pub(super) fn new(semantic_blocking: &[String], operational_unavailable: &[String]) -> Self {
        Self {
            semantic_blocking: Self::bounded(semantic_blocking),
            operational_unavailable: Self::bounded(operational_unavailable),
        }
    }

    fn bounded(items: &[String]) -> Vec<String> {
        items
            .iter()
            .take(Self::MAX_ITEMS_PER_CHANNEL)
            .map(|item| item.chars().take(Self::MAX_ITEM_CHARS).collect())
            .collect()
    }

    fn merged(prior: Option<&Self>, current: Self) -> Self {
        fn merge_channel(prior: &[String], current: Vec<String>) -> Vec<String> {
            let mut merged = prior.to_vec();
            for item in current {
                if !merged.contains(&item) {
                    merged.push(item);
                }
            }
            merged.truncate(OperationalReviewEvidence::MAX_ITEMS_PER_CHANNEL);
            merged
        }

        match prior {
            Some(prior) => Self {
                semantic_blocking: merge_channel(
                    &prior.semantic_blocking,
                    current.semantic_blocking,
                ),
                operational_unavailable: merge_channel(
                    &prior.operational_unavailable,
                    current.operational_unavailable,
                ),
            },
            None => current,
        }
    }

    pub(super) fn ledger_blockers(&self) -> Vec<String> {
        let mut blockers = self
            .semantic_blocking
            .iter()
            .map(|item| format!("semantic blocker: {item}"))
            .collect::<Vec<_>>();
        blockers.extend(
            self.operational_unavailable
                .iter()
                .map(|item| format!("operational review unavailable: {item}")),
        );
        blockers
    }
}

/// The exact review boundary a recoverable host outage parked.
///
/// A step review and the final whole-build review are deliberately distinct:
/// retrying the latter by reopening an ordinary review step would review once in
/// the scheduler and then immediately review a second time in the final gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(super) enum OperationalReviewCheckpoint {
    /// Retry the named plan review step through the normal scheduler.
    StepReview {
        /// Stable persisted plan-step id.
        step_id: String,
        /// QC-input identity for which the build step's deterministic
        /// acceptance already passed before its reviewer became unavailable.
        #[serde(default)]
        qc_source_fingerprint: Option<String>,
        /// Exact reviewer roster for this boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required_seats: Option<Vec<crate::critics::Seat>>,
        /// A Build step already ran its doer and reached only its
        /// `ReviewClean` acceptance boundary. When the fingerprint still
        /// matches, resume must retry that review rather than re-run the doer.
        #[serde(default)]
        review_only: bool,
        /// Consecutive unavailable review boundaries over the same QC inputs.
        /// One boundary already includes the per-seat fresh-fork retry.
        #[serde(default)]
        consecutive_outages: u8,
        /// Product findings returned by available seats and host-owned outage
        /// evidence returned by unavailable seats, kept as separate channels.
        #[serde(default)]
        evidence: OperationalReviewEvidence,
        /// `auto` has no human retry gate: one boundary (which already includes
        /// the seat's fresh-session retry) settles terminally.
        #[serde(default)]
        terminally_settled: bool,
    },
    /// Skip already-settled plan steps and retry the final whole-build gate.
    FinalGateReview {
        /// QC-input identity (source/tests plus manifests, lockfiles, build
        /// config, and CI) for which every deterministic floor already passed.
        /// When it still matches, resume retries only the unavailable reviewer;
        /// a missing/mismatched fingerprint conservatively re-runs QC.
        #[serde(default)]
        qc_source_fingerprint: Option<String>,
        /// Exact required reviewer roster that was convened at the parked
        /// boundary. A continuation must not silently resize the gate by
        /// re-routing the requirement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required_seats: Option<Vec<crate::critics::Seat>>,
        /// Resident task-ledger run that owns this paused boundary. Older
        /// checkpoints omit it and resume conservatively without ledger reuse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_task_run_id: Option<String>,
        /// Consecutive unavailable review boundaries over the same QC inputs.
        #[serde(default)]
        consecutive_outages: u8,
        /// Product findings returned by available seats and host-owned outage
        /// evidence returned by unavailable seats, kept as separate channels.
        #[serde(default)]
        evidence: OperationalReviewEvidence,
        /// `auto` has no human retry gate: one boundary (which already includes
        /// the seat's fresh-session retry) settles terminally.
        #[serde(default)]
        terminally_settled: bool,
    },
}

impl OperationalReviewCheckpoint {
    /// Old checkpoints predate the counter but necessarily represent one outage.
    pub(super) const fn effective_outages(&self) -> u8 {
        let recorded = match self {
            Self::StepReview {
                consecutive_outages,
                ..
            }
            | Self::FinalGateReview {
                consecutive_outages,
                ..
            } => *consecutive_outages,
        };
        if recorded == 0 {
            1
        } else {
            recorded
        }
    }

    pub(super) const fn circuit_open(&self) -> bool {
        let terminally_settled = match self {
            Self::StepReview {
                terminally_settled, ..
            }
            | Self::FinalGateReview {
                terminally_settled, ..
            } => *terminally_settled,
        };
        terminally_settled || self.effective_outages() >= MAX_CONSECUTIVE_REVIEW_OUTAGES
    }

    pub(super) const fn evidence(&self) -> &OperationalReviewEvidence {
        match self {
            Self::StepReview { evidence, .. } | Self::FinalGateReview { evidence, .. } => evidence,
        }
    }

    pub(super) fn settle_terminally(&mut self) {
        match self {
            Self::StepReview {
                terminally_settled, ..
            }
            | Self::FinalGateReview {
                terminally_settled, ..
            } => *terminally_settled = true,
        }
    }
}

/// A durable terminal review circuit must still route `/continue` into the
/// director. Returning `None` would make hosted callers fail open to a fresh run
/// and replay the task that the circuit just stopped.
pub fn terminal_review_circuit_reason(root: &Path) -> Option<String> {
    let checkpoint = load_operational_review_checkpoint(root)?;
    checkpoint.circuit_open().then(|| {
        format!(
            "required review remained unavailable after {} bounded review boundaries; this run is stopped incomplete and cannot be continued. No source repair was started. Start a new /run or send a new requirement when the reviewer service is available",
            checkpoint.effective_outages()
        )
    })
}

pub(super) fn next_step_review_checkpoint(
    prior: Option<&OperationalReviewCheckpoint>,
    step_id: String,
    qc_source_fingerprint: Option<String>,
    required_seats: Option<Vec<crate::critics::Seat>>,
    review_only: bool,
    evidence: OperationalReviewEvidence,
) -> OperationalReviewCheckpoint {
    let consecutive_outages = match prior {
        Some(
            checkpoint @ OperationalReviewCheckpoint::StepReview {
                step_id: prior_step,
                qc_source_fingerprint: prior_fingerprint,
                required_seats: prior_seats,
                review_only: prior_review_only,
                ..
            },
        ) if prior_step == &step_id
            && prior_fingerprint == &qc_source_fingerprint
            && prior_seats == &required_seats
            && *prior_review_only == review_only =>
        {
            checkpoint.effective_outages().saturating_add(1)
        }
        _ => 1,
    };
    let evidence = OperationalReviewEvidence::merged(
        prior.map(OperationalReviewCheckpoint::evidence),
        evidence,
    );
    OperationalReviewCheckpoint::StepReview {
        step_id,
        qc_source_fingerprint,
        required_seats,
        review_only,
        consecutive_outages,
        evidence,
        terminally_settled: false,
    }
}

pub(super) fn next_final_review_checkpoint(
    prior: Option<&OperationalReviewCheckpoint>,
    qc_source_fingerprint: Option<String>,
    required_seats: Option<Vec<crate::critics::Seat>>,
    entry_task_run_id: Option<String>,
    evidence: OperationalReviewEvidence,
) -> OperationalReviewCheckpoint {
    let consecutive_outages = match prior {
        Some(
            checkpoint @ OperationalReviewCheckpoint::FinalGateReview {
                qc_source_fingerprint: prior_fingerprint,
                required_seats: prior_seats,
                ..
            },
        ) if prior_fingerprint == &qc_source_fingerprint && prior_seats == &required_seats => {
            checkpoint.effective_outages().saturating_add(1)
        }
        _ => 1,
    };
    let evidence = OperationalReviewEvidence::merged(
        prior.map(OperationalReviewCheckpoint::evidence),
        evidence,
    );
    OperationalReviewCheckpoint::FinalGateReview {
        qc_source_fingerprint,
        required_seats,
        entry_task_run_id,
        consecutive_outages,
        evidence,
        terminally_settled: false,
    }
}

/// In-band plan marker for a final-review retry.
///
/// The checkpoint and plan are separate atomically-written files, so a process can
/// die between their renames. Keeping a distinctive cursor in the plan lets the
/// loader reconstruct a missing checkpoint without scheduling an ordinary review
/// and then immediately reviewing again in the final gate.
pub(super) const FINAL_REVIEW_RETRY_STEP_ID: &str = "umadev-final-review-retry";

#[cfg(test)]
fn operational_review_checkpoint_path(root: &Path) -> std::path::PathBuf {
    root.join(".umadev")
        .join(OPERATIONAL_REVIEW_CHECKPOINT_FILE)
}

/// Marker: the repair channel for the CURRENT persisted plan is explicitly
/// CLOSED. Written when the user cancels an operational pause (an explicit "do
/// not come back to this"), it keeps that plan's `Blocked` steps terminal on a
/// later `/continue` — [`reopen_blocked_for_repair`] must never resurrect a run
/// the user deliberately closed. Cleared whenever a NEW plan is synthesized, so
/// a stale marker can never suppress repair-resume for a later, unrelated run.
const PLAN_REPAIR_CLOSED_FILE: &str = "plan-repair-closed.json";
const MAX_PLAN_REPAIR_CLOSED_BYTES: u64 = 4 * 1024;

/// Close the repair channel for the current plan (best-effort, bounded reason).
pub(super) fn mark_plan_repair_closed(root: &Path, reason: &str) {
    let Ok(dir) = crate::bounded_fs::ensure_real_dir_beneath(root, Path::new(".umadev")) else {
        return;
    };
    let reason = umadev_governance::redaction::redact_text(reason);
    let body =
        serde_json::json!({ "reason": reason.chars().take(240).collect::<String>() }).to_string();
    let _ = umadev_state::fs::atomic_write(&dir.join(PLAN_REPAIR_CLOSED_FILE), body.as_bytes());
}

/// Reopen the repair channel — called when a fresh plan replaces the closed one.
pub(super) fn clear_plan_repair_closed(root: &Path) {
    if let Some(dir) = existing_umadev_dir(root) {
        let _ = umadev_state::fs::remove_regular_file(&dir.join(PLAN_REPAIR_CLOSED_FILE));
    }
}

fn plan_repair_closed(root: &Path) -> bool {
    umadev_state::fs::read_bounded_beneath(
        root,
        Path::new(".umadev/plan-repair-closed.json"),
        MAX_PLAN_REPAIR_CLOSED_BYTES,
    )
    .is_ok()
}

fn existing_umadev_dir(root: &Path) -> Option<std::path::PathBuf> {
    let canonical_root = std::fs::canonicalize(root).ok()?;
    if !umadev_state::fs::real_dir(&canonical_root) {
        return None;
    }
    let dir = canonical_root.join(".umadev");
    umadev_state::fs::real_dir(&dir).then_some(dir)
}

pub(super) fn save_operational_review_checkpoint(
    root: &Path,
    checkpoint: &OperationalReviewCheckpoint,
) -> std::io::Result<()> {
    let dir = crate::bounded_fs::ensure_real_dir_beneath(root, Path::new(".umadev"))?;
    let body = serde_json::to_vec_pretty(checkpoint)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if body.len() > usize::try_from(MAX_OPERATIONAL_REVIEW_CHECKPOINT_BYTES).unwrap_or(usize::MAX) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "operational review checkpoint exceeds the managed file limit",
        ));
    }
    umadev_state::fs::atomic_write(
        &dir.join(OPERATIONAL_REVIEW_CHECKPOINT_FILE),
        body.as_slice(),
    )
}

pub(super) fn load_operational_review_checkpoint(
    root: &Path,
) -> Option<OperationalReviewCheckpoint> {
    let body = umadev_state::fs::read_bounded_beneath(
        root,
        Path::new(".umadev/director-operational-review.json"),
        MAX_OPERATIONAL_REVIEW_CHECKPOINT_BYTES,
    )
    .ok()?;
    serde_json::from_slice(&body).ok()
}

pub(super) fn clear_operational_review_checkpoint(root: &Path) {
    if let Some(dir) = existing_umadev_dir(root) {
        let _ =
            umadev_state::fs::remove_regular_file(&dir.join(OPERATIONAL_REVIEW_CHECKPOINT_FILE));
    }
}

/// Whether `plan` still has work left to drive — at least one non-terminal step.
fn plan_has_incomplete_step(plan: &Plan) -> bool {
    plan.steps
        .iter()
        .any(|step| matches!(step.status, StepStatus::Pending | StepStatus::Active))
}

/// REPAIR-RESUME: reopen every `Blocked` step so a Blocked-settled run is
/// resumable *as a repair* instead of being neither resumable nor terminal.
///
/// Before this, a run that settled with blocked steps had no `Pending`/`Active`
/// work, so [`load_resumable_plan`] returned `None` — while every user-facing
/// affordance still advertised `/continue`. `/continue` then silently degraded
/// into a brand-new full run (fresh plan synthesis, completed work redone) that
/// hit the same wall — the reported "一直这样子" loop. Reopening the blocked
/// steps (their stranded dependents were blocked by the same sweep, so the whole
/// chain reopens together) makes `/continue` re-drive exactly the failed work,
/// and the settle path has already written each step's WHY into the bounded
/// run-notes, so the repair attempt is INFORMED — the next directives carry the
/// blockers instead of re-deriving the same mistake blind.
///
/// The caller gates this on the operational-review circuit being CLOSED: an open
/// circuit blocked the steps precisely so the run is NOT resumable, and undoing
/// that here would resurrect a terminally-refused run.
fn reopen_blocked_for_repair(plan: &mut Plan) {
    for step in &mut plan.steps {
        if step.status == StepStatus::Blocked {
            step.status = StepStatus::Pending;
        }
    }
}

/// Reset interrupted work so a fresh session can schedule it again.
fn reset_active_to_pending(plan: &mut Plan) {
    for step in &mut plan.steps {
        if step.status == StepStatus::Active {
            step.status = StepStatus::Pending;
        }
    }
}

fn final_review_retry_step(depends_on: Vec<String>) -> plan_state::PlanStep {
    plan_state::PlanStep {
        id: FINAL_REVIEW_RETRY_STEP_ID.to_string(),
        title: "Retry final whole-build review".to_string(),
        seat: crate::critics::Seat::QaEngineer,
        kind: plan_state::StepKind::Review,
        depends_on,
        acceptance: plan_state::AcceptanceSpec::ReviewClean,
        evidence: Vec::new(),
        files: plan_state::StepFiles::default(),
        status: StepStatus::Pending,
    }
}

fn reconcile_plan_with_operational_checkpoint(
    plan: &mut Plan,
    checkpoint: &OperationalReviewCheckpoint,
) {
    if checkpoint.circuit_open() {
        for step in &mut plan.steps {
            if matches!(step.status, StepStatus::Pending | StepStatus::Active) {
                step.status = StepStatus::Blocked;
            }
        }
        return;
    }
    match checkpoint {
        OperationalReviewCheckpoint::StepReview { step_id, .. } => {
            if let Some(step) = plan.steps.iter_mut().find(|step| step.id == *step_id) {
                if matches!(step.status, StepStatus::Done | StepStatus::Blocked) {
                    step.status = StepStatus::Pending;
                }
            } else {
                let mut step = final_review_retry_step(Vec::new());
                step.id.clone_from(step_id);
                step.title = format!("Retry interrupted review `{step_id}`");
                plan.steps.push(step);
            }
        }
        OperationalReviewCheckpoint::FinalGateReview { .. } => {
            let dependencies = plan
                .steps
                .iter()
                .filter(|step| step.kind != plan_state::StepKind::Review)
                .map(|step| step.id.clone())
                .collect::<Vec<_>>();
            if let Some(step) = plan
                .steps
                .iter_mut()
                .find(|step| step.id == FINAL_REVIEW_RETRY_STEP_ID)
            {
                step.status = StepStatus::Pending;
                step.depends_on = dependencies;
            } else {
                plan.steps.push(final_review_retry_step(dependencies));
            }
        }
    }
}

/// Resolve the logical review checkpoint from either transaction half.
///
/// This is read-only: an in-band final cursor is sufficient to recover a lost
/// checkpoint file, so status/resume probes never rewrite project state.
pub(super) fn operational_review_checkpoint_for_plan(
    root: &Path,
    plan: &Plan,
) -> Option<OperationalReviewCheckpoint> {
    load_operational_review_checkpoint(root).or_else(|| {
        plan.steps
            .iter()
            .any(|step| {
                step.id == FINAL_REVIEW_RETRY_STEP_ID
                    && matches!(step.status, StepStatus::Pending | StepStatus::Active)
            })
            .then_some(OperationalReviewCheckpoint::FinalGateReview {
                // The crash lost the verified-tree receipt. Reconstruct only the
                // boundary; the resume conservatively re-runs deterministic QC.
                qc_source_fingerprint: None,
                required_seats: None,
                entry_task_run_id: None,
                consecutive_outages: 0,
                evidence: OperationalReviewEvidence::default(),
                terminally_settled: false,
            })
    })
}

/// Load a persisted plan when it contains work or owns an open confirmation gate.
///
/// The plan and operational-review checkpoint are reconciled as one logical
/// transaction. Either file may be the one whose atomic rename landed before a
/// crash: a checkpoint can recreate its missing plan cursor, while the distinctive
/// final-review cursor can recreate a missing checkpoint.
pub(super) fn load_resumable_plan(root: &Path) -> Option<Plan> {
    let mut checkpoint = load_operational_review_checkpoint(root);
    let mut plan = match plan_state::load(root) {
        Some(plan) => plan,
        None if checkpoint.is_some() => Plan {
            steps: Vec::new(),
            risks: Vec::new(),
            open_questions: Vec::new(),
        },
        None => return None,
    };
    invalidate_stale_steps(root, &mut plan);
    checkpoint = operational_review_checkpoint_for_plan(root, &plan);
    let circuit_open = checkpoint
        .as_ref()
        .is_some_and(OperationalReviewCheckpoint::circuit_open);
    if let Some(checkpoint) = checkpoint.as_ref() {
        reconcile_plan_with_operational_checkpoint(&mut plan, checkpoint);
    }
    // REPAIR-RESUME: with the circuit closed AND the repair channel not explicitly
    // closed (a user-cancelled pause), blocked steps are re-drivable work (see
    // `reopen_blocked_for_repair`); otherwise they stay terminal.
    if !circuit_open && !plan_repair_closed(root) {
        reopen_blocked_for_repair(&mut plan);
    }
    let parked_at_gate = crate::state::read_workflow_state(root)
        .and_then(|state| crate::gates::Gate::from_id(&state.active_gate))
        .is_some();
    if !plan_has_incomplete_step(&plan) && !parked_at_gate {
        return None;
    }
    reset_active_to_pending(&mut plan);
    Some(plan)
}

/// Canonical artifact name for an `output/<slug>-<kind>.md` file.
fn artifact_name_from_filename(filename: &str) -> Option<&'static str> {
    let stem = filename.strip_suffix(".md")?;
    if stem.ends_with("-architecture") {
        Some("architecture")
    } else if stem.ends_with("-prd") {
        Some("prd")
    } else if stem.ends_with("-uiux") {
        Some("uiux")
    } else {
        None
    }
}

fn artifact_kind_from_name(name: &str) -> Option<crate::critics::ArtifactKind> {
    use crate::critics::ArtifactKind as A;
    match name {
        "architecture" => Some(A::Architecture),
        "prd" => Some(A::Prd),
        "uiux" => Some(A::Uiux),
        _ => None,
    }
}

/// The concrete document files the current plan declares as part of its active
/// blackboard. Historical `output/*-{prd,architecture,uiux}.md` files that no
/// current step names are deliberately absent. Both typed evidence and the step's
/// file surface participate; a path repeated across them is read once.
fn active_artifact_paths(plan: &Plan) -> BTreeMap<String, BTreeSet<String>> {
    let mut active = BTreeMap::<String, BTreeSet<String>>::new();
    let mut insert = |raw: &str| {
        // Plan paths are specified with `/`, but tolerate a persisted Windows
        // separator so the same plan remains portable across hosts.
        let path = raw.trim().replace('\\', "/");
        let Some(filename) = path.rsplit('/').next().filter(|name| !name.is_empty()) else {
            return;
        };
        let Some(kind) = artifact_name_from_filename(filename) else {
            return;
        };
        active.entry(kind.to_string()).or_default().insert(path);
    };

    for step in &plan.steps {
        for path in step.files.all() {
            insert(path);
        }
        for evidence in &step.evidence {
            match evidence {
                crate::plan_state::EvidenceContract::FileExists { path }
                | crate::plan_state::EvidenceContract::FileContains { path, .. } => insert(path),
                _ => {}
            }
        }
    }
    active
}

/// Read the active plan's current document-artifact versions. Multiple explicitly
/// active files of one semantic kind are folded in lexical path order, producing a
/// deterministic aggregate independent of directory enumeration or step order.
/// A relevant artifact that cannot be read completely makes the snapshot
/// unavailable; it is never silently omitted and mistaken for a complete version
/// set. A legacy plan with no concrete document references returns an empty set,
/// so unrelated historical output can never make it false-stale.
pub(super) fn current_artifact_versions(
    root: &Path,
    plan: &Plan,
) -> io::Result<Vec<(String, String)>> {
    let active = active_artifact_paths(plan);
    let active_count = active.values().map(BTreeSet::len).sum::<usize>();
    if active_count > MAX_ACTIVE_ARTIFACTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "active artifact path budget exhausted",
        ));
    }

    let mut versions = Vec::with_capacity(active.len());
    let mut budget = crate::bounded_fs::Utf8ReadBudget::new(
        MAX_ARTIFACT_VERSION_TOTAL_BYTES,
        MAX_ARTIFACT_VERSION_BYTES,
    );
    for (kind, paths) in active {
        let mut members = Vec::with_capacity(paths.len());
        for path in paths {
            let member_version = match budget.read_utf8_beneath(root, Path::new(&path)) {
                Ok(content) => crate::critics::artifact_version(&content),
                // Absence is a deterministic state, not an incomplete snapshot.
                // Recording it lets a later deletion or first materialization
                // compare honestly without scanning unrelated output files.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    crate::critics::artifact_version(&format!("missing active artifact: {path}"))
                }
                Err(error) => return Err(error),
            };
            members.push((path, member_version));
        }
        let version = if members.len() == 1 {
            members.pop().expect("one active artifact member").1
        } else {
            // Length-prefix each component so unusual, but valid, file names cannot
            // produce an ambiguous aggregate. `members` is already path-sorted.
            let mut aggregate = String::new();
            for (path, member_version) in members {
                aggregate.push_str(&path.len().to_string());
                aggregate.push(':');
                aggregate.push_str(&path);
                aggregate.push_str(&member_version.len().to_string());
                aggregate.push(':');
                aggregate.push_str(&member_version);
            }
            crate::critics::artifact_version(&aggregate)
        };
        versions.push((kind, version));
    }
    Ok(versions)
}

/// Record the artifact versions the persisted plan was built against.
pub(super) fn record_artifact_versions(root: &Path) {
    let Some(plan) = plan_state::load(root) else {
        return;
    };
    let Ok(current) = current_artifact_versions(root, &plan) else {
        return;
    };
    if current.is_empty() {
        return;
    }
    crate::critics::write_artifact_versions(root, &current.into_iter().collect());
}

/// Re-open steps whose upstream document artifacts changed since the last save.
pub(super) fn invalidate_stale_steps(root: &Path, plan: &mut Plan) {
    let Ok(current) = current_artifact_versions(root, plan) else {
        use crate::critics::ArtifactKind as A;
        plan.invalidate_stale(&[
            A::Prd,
            A::Architecture,
            A::Uiux,
            A::ApiContract,
            A::DataModel,
            A::DesignTokens,
            A::Acceptance,
        ]);
        return;
    };
    if current.is_empty() {
        return;
    }
    let stale = crate::critics::stale_artifacts(root, &current);
    if stale.is_empty() {
        return;
    }

    use crate::critics::ArtifactKind as A;
    let mut kinds = Vec::new();
    for name in &stale {
        let Some(kind) = artifact_kind_from_name(name) else {
            continue;
        };
        kinds.push(kind);
        // Typed contracts are derived from these source documents, so they become
        // stale with their source and must reopen their dependent plan steps too.
        match kind {
            A::Architecture => {
                kinds.push(A::ApiContract);
                kinds.push(A::DataModel);
            }
            A::Uiux => kinds.push(A::DesignTokens),
            A::Prd => kinds.push(A::Acceptance),
            _ => {}
        }
    }
    plan.invalidate_stale(&kinds);
}

/// Whether `/continue` must route into the Director: either an incomplete plan
/// or a terminal review circuit that must return `Failed` instead of replaying.
#[must_use]
pub fn has_resumable_director_plan(root: &Path) -> bool {
    terminal_review_circuit_reason(root).is_some() || load_resumable_plan(root).is_some()
}

/// Whether `reason` is a RUN-TIME-BUDGET-exhaustion reason — the terminal string a
/// budget-stopped build carries ("run time budget exhausted …", from
/// `plan_incomplete_reason` / the single-turn twin, both wrapped by
/// `qc_incomplete_reason`). This is the string-matching fallback for the surfaces
/// that see only a reason string (not the typed
/// [`crate::director_loop::DirectorLoopOutcome::PausedAtBudget`]); prefer keying off
/// the typed outcome wherever the flow has it. A budget reason is DISTINCT from a
/// transient (429 / network), auth, or generic failure — none of those contain this
/// marker — so it never mis-classifies a real failure as a resumable budget pause.
/// Pure.
#[must_use]
pub fn is_budget_pause_reason(reason: &str) -> bool {
    reason.contains("run time budget exhausted")
}

/// A ONE-LINE localized discoverability hint to emit when a director run stops with a
/// still-resumable plan on disk AND the stop was either a **transient** base failure
/// (a rate limit / an overloaded base / a network blip — [`crate::base_error::is_transient`])
/// OR a **run-time-budget** exhaustion ([`is_budget_pause_reason`]): the plan was
/// saved and `/continue` picks up the unfinished steps.
///
/// Without it a rate-limited or budget-stopped run reads as "it just stopped": the
/// saved plan is invisible unless the user happens to know `/continue` exists.
/// Returns `Some` only when a resumable plan exists AND the reason is transient or a
/// budget pause; a hard failure (auth / context / a non-zero exit) or a run with
/// nothing left to resume yields `None` so no misleading "you can continue" line is
/// shown. A budget pause fills the hint with the plan's `done/total` step counts so
/// the user sees exactly where the run parked.
///
/// Fail-open by construction: classification is a pure scan of `reason` and the
/// resumable check is best-effort file IO — an unclassifiable reason or an
/// unreadable plan simply yields `None` (the stop is never blocked). Pure aside from
/// the read-only plan probe.
#[must_use]
pub fn transient_resume_hint(reason: &str, root: &Path) -> Option<String> {
    // A terminal review receipt can coexist with a mechanically incomplete
    // plan. It is nevertheless not resumable: never let a coincidental 429 or
    // timeout string advertise `/continue` after the bounded circuit opened.
    if terminal_review_circuit_reason(root).is_some()
        || crate::continuous::legacy_operational_review_terminal_reason(root).is_some()
    {
        return None;
    }
    // The plan must still be resumable for EITHER hint — probe once and read its
    // progress for the budget-pause variant (done/total).
    let plan = load_resumable_plan(root)?;
    let failure = crate::base_error::classify(None, None, Some(reason.trim()));
    if crate::base_error::is_transient(&failure) {
        return Some(umadev_i18n::tl("run.transient_resume_hint").to_string());
    }
    if is_budget_pause_reason(reason) {
        let (done, total) = plan.progress();
        return Some(umadev_i18n::tlf(
            "run.budget_pause_resume_hint",
            &[&done.to_string(), &total.to_string()],
        ));
    }
    None
}

/// Whether `root` contains any run state that a fresh session can resume.
#[must_use]
pub fn has_resumable_run(root: &Path) -> bool {
    // A clean final workflow state is the canonical terminal receipt. Check it
    // BEFORE the plan: an interrupted write can leave stale Pending/Active rows in
    // `.umadev/plan.json` even though finalization already committed delivery. Letting
    // that stale plan win re-opened the quality/review loop whenever the user merely
    // asked for progress in a later session.
    if let Some(state) = crate::state::read_workflow_state(root) {
        let clean_delivery = state.active_gate.trim().is_empty()
            && state.phase.eq_ignore_ascii_case(Phase::Delivery.id())
            && state.note.trim() == super::DIRECTOR_COMPLETE_NOTE;
        if clean_delivery {
            return false;
        }
    }
    // The legacy fixed pipeline already consumed its one review-only retry.
    // This is a terminal receipt, not an interrupted phase: advertising it as
    // resumable makes the TUI dispatch `/continue` back into a fresh writer
    // session and recreates the outage loop. A deliberate new `/run` replaces
    // the workflow state and therefore clears this receipt.
    if crate::continuous::legacy_operational_review_terminal_reason(root).is_some() {
        return false;
    }
    if terminal_review_circuit_reason(root).is_some() {
        // User-facing status and slash routing must not advertise a terminal
        // circuit as resumable. `has_resumable_director_plan` intentionally stays
        // true for programmatic callers so they route into the inner Failed guard
        // instead of falling back to a fresh run.
        return false;
    }
    if load_resumable_plan(root).is_some() {
        return true;
    }
    if let Some(state) = crate::state::read_workflow_state(root) {
        // A parked gate is always resumable. The phase-based fallback is scoped to
        // runs with NO director plan on disk (the legacy continuous engine, which
        // persists only workflow-state): when a plan EXISTS, `load_resumable_plan`
        // above is authoritative — advertising `/continue` off the phase alone for
        // a settled director plan was the "resume silently restarts from scratch"
        // trap (the plan had nothing to resume, so the resume path fell open to a
        // brand-new run).
        if !state.active_gate.trim().is_empty()
            || (state.phase != Phase::Delivery.id() && plan_state::load(root).is_none())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critics::Seat;
    use crate::plan_state::{AcceptanceSpec, PlanStep, StepFiles, StepKind};

    /// Build a minimal one-step plan with the given status and persist it under
    /// `root` so the resumable/hint probes have real `.umadev/plan.json` to read.
    fn save_plan(root: &Path, status: StepStatus) {
        let plan = Plan {
            steps: vec![PlanStep {
                id: "s1".to_string(),
                title: "build the thing".to_string(),
                seat: Seat::BackendEngineer,
                kind: StepKind::Build,
                depends_on: Vec::new(),
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: Vec::new(),
                files: StepFiles::default(),
                status,
            }],
            risks: Vec::new(),
            open_questions: Vec::new(),
        };
        plan_state::save(&plan, root).expect("persist test plan");
    }

    #[test]
    fn clearing_a_missing_operational_checkpoint_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        clear_operational_review_checkpoint(temp.path());
        clear_operational_review_checkpoint(temp.path());
        assert!(!operational_review_checkpoint_path(temp.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn clearing_an_operational_checkpoint_never_follows_a_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let managed = operational_review_checkpoint_path(temp.path());
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        let outside = temp.path().join("outside-review-receipt.json");
        std::fs::write(&outside, b"keep").unwrap();
        symlink(&outside, &managed).unwrap();

        clear_operational_review_checkpoint(temp.path());

        assert!(std::fs::symlink_metadata(&managed)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn resume_state_never_follows_a_linked_managed_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let checkpoint = OperationalReviewCheckpoint::FinalGateReview {
            qc_source_fingerprint: None,
            required_seats: None,
            entry_task_run_id: None,
            consecutive_outages: 2,
            evidence: OperationalReviewEvidence::default(),
            terminally_settled: false,
        };
        let outside_checkpoint = outside.path().join(OPERATIONAL_REVIEW_CHECKPOINT_FILE);
        std::fs::write(
            &outside_checkpoint,
            serde_json::to_vec_pretty(&checkpoint).unwrap(),
        )
        .unwrap();
        let outside_closed = outside.path().join(PLAN_REPAIR_CLOSED_FILE);
        std::fs::write(&outside_closed, b"outside").unwrap();
        symlink(outside.path(), root.path().join(".umadev")).unwrap();

        assert!(load_operational_review_checkpoint(root.path()).is_none());
        assert!(save_operational_review_checkpoint(root.path(), &checkpoint).is_err());
        mark_plan_repair_closed(root.path(), "cancelled");
        clear_plan_repair_closed(root.path());
        clear_operational_review_checkpoint(root.path());
        assert_eq!(std::fs::read(&outside_closed).unwrap(), b"outside");
        assert_eq!(
            serde_json::from_slice::<OperationalReviewCheckpoint>(
                &std::fs::read(&outside_checkpoint).unwrap()
            )
            .unwrap(),
            checkpoint
        );
    }

    #[test]
    fn transient_hint_fires_only_for_transient_reason_with_resumable_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A resumable plan (one still-active step) + a rate-limit reason → hint.
        save_plan(root, StepStatus::Active);
        let hint = transient_resume_hint(
            "API Error: Request rejected (429) · exceeded the 5-hour usage quota",
            root,
        );
        assert_eq!(
            hint.as_deref(),
            Some(umadev_i18n::tl("run.transient_resume_hint")),
            "a rate-limit abort with a resumable plan surfaces the /continue hint"
        );
    }

    #[test]
    fn blocked_settled_plan_reopens_as_a_repair_resume() {
        // The reported "一直这样子" loop: a Blocked settle left no Pending/Active
        // work, so the plan was "not resumable" — while every affordance still
        // advertised /continue, which then silently fell open to a brand-new run.
        // A Blocked step (circuit CLOSED) now reopens as Pending, so /continue
        // re-drives exactly the failed work instead of restarting from scratch.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_plan(root, StepStatus::Blocked);
        let plan =
            load_resumable_plan(root).expect("a blocked-settled plan is resumable as repair");
        assert_eq!(
            plan.steps[0].status,
            StepStatus::Pending,
            "the blocked step reopens for the repair attempt"
        );
        assert!(
            has_resumable_director_plan(root),
            "the resumability probe agrees with the loader"
        );
    }

    #[test]
    fn a_closed_repair_channel_keeps_blocked_steps_terminal_until_a_fresh_plan() {
        // The user explicitly cancelled the paused run — that closed the repair
        // channel, so its Blocked steps must not resurrect on /continue…
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_plan(root, StepStatus::Blocked);
        mark_plan_repair_closed(root, "cancelled by user");
        assert!(
            load_resumable_plan(root).is_none(),
            "a deliberately cancelled run stays terminal"
        );
        // …while a fresh plan synthesis reopens the channel for the NEW run.
        clear_plan_repair_closed(root);
        assert!(
            load_resumable_plan(root).is_some(),
            "clearing the marker restores repair-resume for a new plan"
        );
    }

    #[test]
    fn open_review_circuit_keeps_a_blocked_plan_terminal() {
        // The deliberate exception: an OPEN operational-review circuit blocked the
        // steps precisely so the run is NOT resumable — repair-reopening would
        // resurrect a terminally-refused run.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_plan(root, StepStatus::Blocked);
        save_operational_review_checkpoint(
            root,
            &OperationalReviewCheckpoint::FinalGateReview {
                qc_source_fingerprint: None,
                required_seats: None,
                entry_task_run_id: None,
                consecutive_outages: 2,
                evidence: OperationalReviewEvidence::default(),
                terminally_settled: false,
            },
        )
        .unwrap();
        assert!(
            load_resumable_plan(root).is_none(),
            "an open circuit never reopens blocked steps"
        );
    }

    #[test]
    fn phase_fallback_never_advertises_resume_over_a_settled_director_plan() {
        // has_resumable_run's phase fallback exists for the legacy continuous
        // engine (workflow-state only, NO plan.json). When a director plan exists,
        // the loader is authoritative: an all-Done plan with a mid phase must NOT
        // advertise /continue (that was the silent-restart trap)...
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_plan(root, StepStatus::Done);
        let state = crate::state::WorkflowState::new(Phase::Backend);
        crate::state::write_workflow_state(root, &state).unwrap();
        assert!(
            !has_resumable_run(root),
            "a settled director plan is not resumable off the phase alone"
        );
        // ...while the SAME workflow state with no plan on disk (legacy continuous)
        // keeps its resume offer.
        let tmp2 = tempfile::tempdir().unwrap();
        let root2 = tmp2.path();
        crate::state::write_workflow_state(root2, &state).unwrap();
        assert!(
            has_resumable_run(root2),
            "the legacy continuous engine (no plan.json) keeps the phase fallback"
        );
    }

    #[test]
    fn terminal_review_circuit_never_surfaces_a_transient_resume_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_plan(root, StepStatus::Active);
        save_operational_review_checkpoint(
            root,
            &OperationalReviewCheckpoint::FinalGateReview {
                qc_source_fingerprint: None,
                required_seats: None,
                entry_task_run_id: None,
                consecutive_outages: 2,
                evidence: OperationalReviewEvidence::default(),
                terminally_settled: false,
            },
        )
        .unwrap();

        assert!(
            transient_resume_hint("429 too many requests", root).is_none(),
            "a terminal review circuit cannot advertise /continue even when the latest error text is transient"
        );
    }

    #[test]
    fn transient_hint_is_none_for_hard_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_plan(root, StepStatus::Active);
        // Auth is a HARD failure — retrying is futile, so no "you can continue" line.
        assert!(
            transient_resume_hint("Error 401 Unauthorized: invalid api key", root).is_none(),
            "a hard auth failure never claims the run is resumable"
        );
    }

    #[test]
    fn transient_hint_is_none_without_a_resumable_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // No plan on disk at all → nothing to resume.
        assert!(
            transient_resume_hint("429 too many requests", root).is_none(),
            "a transient reason with no saved plan surfaces no hint"
        );
        // An all-done plan is not resumable either.
        save_plan(root, StepStatus::Done);
        assert!(
            transient_resume_hint("429 too many requests", root).is_none(),
            "a completed plan has nothing left to /continue"
        );
    }

    #[test]
    fn is_budget_pause_reason_matches_only_the_budget_reasons() {
        // The two terminal budget strings (plan-path + single-turn twin) both carry
        // the "run time budget exhausted" marker.
        assert!(is_budget_pause_reason(
            "director build incomplete: run time budget exhausted; 2 plan step(s) unfinished"
        ));
        assert!(is_budget_pause_reason(
            "director build incomplete: run time budget exhausted before auto-QC cleared"
        ));
        // A transient / auth / generic failure is NOT a budget pause.
        assert!(!is_budget_pause_reason(
            "API Error: Request rejected (429) · exceeded the 5-hour usage quota"
        ));
        assert!(!is_budget_pause_reason(
            "Error 401 Unauthorized: invalid api key"
        ));
        assert!(!is_budget_pause_reason(
            "director build incomplete: auto-QC settled without a clean verdict"
        ));
    }

    #[test]
    fn resume_hint_fires_for_a_budget_pause_with_done_total() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A resumable plan (one still-pending step) + a budget reason → the budget
        // resume hint, filled with the plan's done/total (0/1 here).
        save_plan(root, StepStatus::Pending);
        let hint = transient_resume_hint(
            "director build incomplete: run time budget exhausted; 1 plan step(s) unfinished",
            root,
        )
        .expect("a budget pause with a resumable plan surfaces the /continue hint");
        assert_eq!(
            hint,
            umadev_i18n::tlf("run.budget_pause_resume_hint", &["0", "1"]),
            "the budget hint carries done/total from the persisted plan"
        );
    }

    #[test]
    fn budget_resume_hint_is_none_without_a_resumable_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A budget reason but no saved plan → no hint (nothing to resume).
        assert!(
            transient_resume_hint("run time budget exhausted", root).is_none(),
            "a budget reason with no saved plan surfaces no hint"
        );
    }

    #[test]
    fn final_checkpoint_reopens_a_fully_done_plan_after_a_crash_window() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_plan(root, StepStatus::Done);
        save_operational_review_checkpoint(
            root,
            &OperationalReviewCheckpoint::FinalGateReview {
                qc_source_fingerprint: Some("tree-a".to_string()),
                required_seats: None,
                entry_task_run_id: None,
                consecutive_outages: 1,
                evidence: OperationalReviewEvidence::default(),
                terminally_settled: false,
            },
        )
        .unwrap();

        let plan = load_resumable_plan(root)
            .expect("the checkpoint half of the transaction recreates a resume cursor");
        assert!(plan.steps.iter().any(|step| {
            step.id == FINAL_REVIEW_RETRY_STEP_ID && step.status == StepStatus::Pending
        }));
    }

    #[test]
    fn in_band_final_cursor_recovers_a_missing_checkpoint_without_rewriting_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let plan = Plan {
            steps: vec![final_review_retry_step(Vec::new())],
            risks: Vec::new(),
            open_questions: Vec::new(),
        };
        plan_state::save(&plan, root).unwrap();

        assert!(load_operational_review_checkpoint(root).is_none());
        let loaded = load_resumable_plan(root).expect("the in-band cursor is resumable");
        assert!(matches!(
            operational_review_checkpoint_for_plan(root, &loaded),
            Some(OperationalReviewCheckpoint::FinalGateReview {
                qc_source_fingerprint: None,
                ..
            })
        ));
        assert!(
            load_operational_review_checkpoint(root).is_none(),
            "a read-only resume/status probe does not rewrite project state"
        );
    }

    #[test]
    fn final_checkpoint_without_a_plan_still_has_a_safe_resume_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        save_operational_review_checkpoint(
            root,
            &OperationalReviewCheckpoint::FinalGateReview {
                qc_source_fingerprint: None,
                required_seats: None,
                entry_task_run_id: None,
                consecutive_outages: 1,
                evidence: OperationalReviewEvidence::default(),
                terminally_settled: false,
            },
        )
        .unwrap();

        let plan = load_resumable_plan(root).expect("a checkpoint-only crash remains resumable");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].id, FINAL_REVIEW_RETRY_STEP_ID);
        assert_eq!(plan.steps[0].status, StepStatus::Pending);
    }

    #[test]
    fn old_unit_final_checkpoint_deserializes_conservatively() {
        let checkpoint: OperationalReviewCheckpoint =
            serde_json::from_str(r#"{"kind":"final-gate-review"}"#).unwrap();
        assert!(matches!(
            checkpoint,
            OperationalReviewCheckpoint::FinalGateReview {
                qc_source_fingerprint: None,
                ..
            }
        ));
    }
}
