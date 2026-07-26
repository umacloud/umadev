//! Durable task lifecycle projection for a director-owned plan.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::bg_agents::BaseAgentObservation;
use crate::plan_state::{Plan, PlanStep, StepStatus};
use crate::task_lifecycle::{
    AgentTaskLedger, AgentTaskMode, AgentTaskOutcome, AgentTaskRecord, AgentTaskState,
    RunReadiness, TaskLifecycleError,
};

const ROOT_TASK_ID: &str = "director";

/// Failure to project a validated plan into its durable task ledger.
#[derive(Debug, Error)]
pub enum PlanTaskError {
    /// The lifecycle journal rejected an operation or could not persist it.
    #[error(transparent)]
    Lifecycle(#[from] TaskLifecycleError),
    /// The plan cannot be represented as a dependency graph.
    #[error("invalid plan task graph: {0}")]
    InvalidPlan(String),
}

/// Durable, resumable task view for one director plan.
#[derive(Debug)]
pub struct PlanTaskTracker {
    ledger: AgentTaskLedger,
    logical_to_task: BTreeMap<String, String>,
}

impl PlanTaskTracker {
    /// Open an unfinished plan ledger only when it already exists.
    ///
    /// Unlike [`Self::open`], this never mints a scoped pointer or journal. It is
    /// intended for cancellation/settlement paths where the persisted plan may
    /// belong solely to a resident entry task rather than a Director run.
    pub fn open_existing(
        project_root: &Path,
        backend: &str,
        requirement: &str,
        plan: &Plan,
    ) -> Result<Option<Self>, PlanTaskError> {
        let scope = plan_scope(backend, requirement, plan)?;
        let Some(ledger) = AgentTaskLedger::open_scoped_existing(project_root, &scope)? else {
            return Ok(None);
        };
        if ledger.tasks().next().is_none() {
            // A pointer may have been published immediately before a crash.
            // There is no coordinator to settle, so cancellation must treat
            // this as absent rather than failing on a missing `director` task.
            return Ok(None);
        }
        Ok(Some(Self {
            logical_to_task: existing_step_tasks(&ledger, plan),
            ledger,
        }))
    }

    /// Open or create the plan's ledger, recover a prior process, and reconcile
    /// already-settled plan steps without re-running them.
    pub fn open(
        project_root: &Path,
        backend: &str,
        requirement: &str,
        plan: &Plan,
    ) -> Result<Self, PlanTaskError> {
        let scope = plan_scope(backend, requirement, plan)?;
        let mut ledger = AgentTaskLedger::open_scoped(project_root, &scope)?;
        ledger.recover_interrupted()?;
        ensure_root(&mut ledger, requirement)?;

        let mut tracker = Self {
            logical_to_task: existing_step_tasks(&ledger, plan),
            ledger,
        };
        tracker.sync_plan(plan)?;
        tracker.reconcile_settled_steps(plan)?;
        Ok(tracker)
    }

    /// Stable run identifier surfaced to status and recovery views.
    #[must_use]
    pub fn run_id(&self) -> &str {
        self.ledger.run_id()
    }

    /// Current immutable task records in stable task-id order.
    pub fn tasks(&self) -> impl Iterator<Item = &AgentTaskRecord> {
        self.ledger.tasks()
    }

    /// Start or resume one logical plan step. A terminal earlier attempt creates
    /// a new immutable attempt instead of rewriting history.
    pub fn start_step(
        &mut self,
        plan: &Plan,
        step: &PlanStep,
    ) -> Result<&AgentTaskRecord, PlanTaskError> {
        self.sync_plan(plan)?;
        let current_id =
            self.logical_to_task.get(&step.id).cloned().ok_or_else(|| {
                PlanTaskError::InvalidPlan(format!("missing task for {}", step.id))
            })?;
        let current_state = self
            .ledger
            .task(&current_id)
            .map(|task| task.state)
            .ok_or_else(|| PlanTaskError::InvalidPlan(format!("missing task {current_id}")))?;
        let task_id = if matches!(
            current_state,
            AgentTaskState::Succeeded
                | AgentTaskState::Failed
                | AgentTaskState::Cancelled
                | AgentTaskState::Unavailable
                | AgentTaskState::Superseded
        ) {
            self.queue_retry(plan, step)?
        } else {
            current_id
        };
        match self
            .ledger
            .task(&task_id)
            .map(|task| task.state)
            .ok_or_else(|| PlanTaskError::InvalidPlan(format!("missing task {task_id}")))?
        {
            AgentTaskState::Queued => Ok(self.ledger.start(&task_id)?),
            AgentTaskState::Waiting | AgentTaskState::Interrupted => {
                Ok(self.ledger.resume(&task_id)?)
            }
            AgentTaskState::Running => self
                .ledger
                .task(&task_id)
                .ok_or_else(|| PlanTaskError::InvalidPlan(format!("missing task {task_id}"))),
            state => Err(PlanTaskError::InvalidPlan(format!(
                "task {task_id} cannot start from {state:?}"
            ))),
        }
    }

    /// Settle the current attempt from the same deterministic result that settles
    /// the plan step.
    pub fn settle_step(
        &mut self,
        step: &PlanStep,
        status: StepStatus,
        unavailable: bool,
        summary: &str,
        blockers: Vec<String>,
    ) -> Result<&AgentTaskRecord, PlanTaskError> {
        let task_id =
            self.logical_to_task.get(&step.id).cloned().ok_or_else(|| {
                PlanTaskError::InvalidPlan(format!("missing task for {}", step.id))
            })?;
        let artifacts = step.files.all().map(str::to_string).collect::<Vec<_>>();
        match status {
            StepStatus::Done => Ok(self
                .ledger
                .succeed(&task_id, AgentTaskOutcome::success(summary, artifacts))?),
            StepStatus::Blocked if unavailable => Ok(self
                .ledger
                .unavailable(&task_id, AgentTaskOutcome::blocked(summary, blockers))?),
            StepStatus::Blocked => Ok(self
                .ledger
                .fail(&task_id, AgentTaskOutcome::blocked(summary, blockers))?),
            StepStatus::Pending | StepStatus::Active => Err(PlanTaskError::InvalidPlan(format!(
                "task {} cannot settle as {status:?}",
                step.id
            ))),
        }
    }

    /// Persist base-native child agents underneath the current plan-step attempt
    /// and settle them from the parent's deterministic acceptance result.
    ///
    /// Vendor ids are never written to disk: they only seed an opaque task-id
    /// digest. A terminal signal alone is not treated as success; a child becomes
    /// successful only when the aggregate parent step passes its mechanical gate.
    pub fn settle_base_agents(
        &mut self,
        step: &PlanStep,
        observed: &BaseAgentObservation,
        status: StepStatus,
        unavailable: bool,
        summary: &str,
        blockers: &[String],
    ) -> Result<usize, PlanTaskError> {
        let parent_task_id =
            self.logical_to_task.get(&step.id).cloned().ok_or_else(|| {
                PlanTaskError::InvalidPlan(format!("missing task for {}", step.id))
            })?;
        let mut source_keys = observed
            .agent_ids()
            .map(|id| format!("vendor\0{id}"))
            .collect::<Vec<_>>();
        source_keys
            .extend((0..observed.anonymous_count()).map(|index| format!("anonymous\0{index}")));
        let mut settled = 0;
        for (ordinal, source_key) in source_keys.into_iter().enumerate() {
            let task_id = base_agent_task_id(&parent_task_id, &source_key);
            if self.ledger.task(&task_id).is_none() {
                self.ledger.queue(
                    task_id.clone(),
                    Some(parent_task_id.clone()),
                    "base-native-agent",
                    format!(
                        "delegated child {} observed while executing {}",
                        ordinal.saturating_add(1),
                        step.title
                    ),
                    AgentTaskMode::ParentManaged,
                )?;
            }
            match self
                .ledger
                .task(&task_id)
                .map(|task| task.state)
                .ok_or_else(|| PlanTaskError::InvalidPlan(format!("missing task {task_id}")))?
            {
                AgentTaskState::Queued => {
                    self.ledger.start(&task_id)?;
                }
                AgentTaskState::Waiting | AgentTaskState::Interrupted => {
                    self.ledger.resume(&task_id)?;
                }
                AgentTaskState::Running => {}
                AgentTaskState::Succeeded
                | AgentTaskState::Failed
                | AgentTaskState::Cancelled
                | AgentTaskState::Unavailable
                | AgentTaskState::Superseded => {
                    settled += 1;
                    continue;
                }
            }
            let child_summary = format!("{summary}; base-native child contribution");
            match status {
                StepStatus::Done => {
                    self.ledger.succeed(
                        &task_id,
                        AgentTaskOutcome::success(
                            child_summary,
                            step.files.all().map(str::to_string).collect(),
                        ),
                    )?;
                }
                StepStatus::Blocked if unavailable => {
                    self.ledger.unavailable(
                        &task_id,
                        AgentTaskOutcome::blocked(child_summary, blockers.to_vec()),
                    )?;
                }
                StepStatus::Blocked => {
                    self.ledger.fail(
                        &task_id,
                        AgentTaskOutcome::blocked(child_summary, blockers.to_vec()),
                    )?;
                }
                StepStatus::Pending | StepStatus::Active => {
                    return Err(PlanTaskError::InvalidPlan(format!(
                        "base-native child cannot settle as {status:?}"
                    )))
                }
            }
            settled += 1;
        }
        Ok(settled)
    }

    /// Park the coordinator at a human gate without losing resumability.
    pub fn wait_for_user(&mut self, detail: &str) -> Result<(), PlanTaskError> {
        match self.root_state()? {
            AgentTaskState::Running => {
                self.ledger.wait(ROOT_TASK_ID, detail)?;
            }
            AgentTaskState::Waiting => {}
            state => {
                return Err(PlanTaskError::InvalidPlan(format!(
                    "director cannot wait from {state:?}"
                )))
            }
        }
        Ok(())
    }

    /// Settle the coordinator and return the mechanically-derived run readiness.
    pub fn finish(
        &mut self,
        clean: bool,
        summary: &str,
        blockers: Vec<String>,
    ) -> Result<RunReadiness, PlanTaskError> {
        let root_state = self.root_state()?;
        if root_state.is_terminal() {
            return Ok(self.ledger.readiness());
        }
        if root_state == AgentTaskState::Waiting {
            self.ledger.resume(ROOT_TASK_ID)?;
        }
        if clean {
            let incomplete = self
                .ledger
                .tasks()
                .filter(|task| task.task_id != ROOT_TASK_ID)
                .filter(|task| {
                    !matches!(
                        task.state,
                        AgentTaskState::Succeeded | AgentTaskState::Superseded
                    )
                })
                .map(|task| format!("{} is {:?}", task.task_id, task.state))
                .collect::<Vec<_>>();
            if incomplete.is_empty() {
                self.ledger
                    .succeed(ROOT_TASK_ID, AgentTaskOutcome::success(summary, Vec::new()))?;
            } else {
                self.cancel_unfinished_children("run ended with an incomplete task ledger")?;
                self.ledger.fail(
                    ROOT_TASK_ID,
                    AgentTaskOutcome::blocked("plan task ledger is incomplete", incomplete),
                )?;
            }
        } else {
            self.cancel_unfinished_children(summary)?;
            self.ledger
                .fail(ROOT_TASK_ID, AgentTaskOutcome::blocked(summary, blockers))?;
        }
        Ok(self.ledger.readiness())
    }

    fn cancel_unfinished_children(&mut self, detail: &str) -> Result<(), PlanTaskError> {
        let mut unfinished = self
            .ledger
            .tasks()
            .filter(|task| task.task_id != ROOT_TASK_ID && !task.state.is_terminal())
            .map(|task| {
                (
                    task.mode != AgentTaskMode::ParentManaged,
                    task.task_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        unfinished.sort();
        for (_, task_id) in unfinished {
            self.ledger.cancel(&task_id, detail)?;
        }
        Ok(())
    }

    fn root_state(&self) -> Result<AgentTaskState, PlanTaskError> {
        self.ledger
            .task(ROOT_TASK_ID)
            .map(|task| task.state)
            .ok_or_else(|| PlanTaskError::InvalidPlan("director task is missing".into()))
    }

    fn sync_plan(&mut self, plan: &Plan) -> Result<(), PlanTaskError> {
        let active_ids = plan
            .steps
            .iter()
            .map(|step| step.id.as_str())
            .collect::<BTreeSet<_>>();
        let removed = self
            .logical_to_task
            .iter()
            .filter(|(logical_id, _)| !active_ids.contains(logical_id.as_str()))
            .map(|(logical_id, task_id)| (logical_id.clone(), task_id.clone()))
            .collect::<Vec<_>>();
        for (logical_id, task_id) in removed {
            if self.ledger.task(&task_id).is_some_and(|task| {
                matches!(
                    task.state,
                    AgentTaskState::Queued
                        | AgentTaskState::Running
                        | AgentTaskState::Waiting
                        | AgentTaskState::Interrupted
                )
            }) {
                self.ledger
                    .supersede(&task_id, "replaced by a validated re-plan")?;
            }
            self.logical_to_task.remove(&logical_id);
        }
        self.queue_missing_steps(plan)
    }

    fn queue_missing_steps(&mut self, plan: &Plan) -> Result<(), PlanTaskError> {
        let mut remaining = plan
            .steps
            .iter()
            .filter(|step| !self.logical_to_task.contains_key(&step.id))
            .map(|step| step.id.clone())
            .collect::<BTreeSet<_>>();
        while !remaining.is_empty() {
            let before = remaining.len();
            for step in &plan.steps {
                if !remaining.contains(&step.id)
                    || !step
                        .depends_on
                        .iter()
                        .all(|dependency| self.logical_to_task.contains_key(dependency))
                {
                    continue;
                }
                let task_id = step_task_id(&step.id, 1);
                let dependencies = step
                    .depends_on
                    .iter()
                    .filter_map(|dependency| self.logical_to_task.get(dependency).cloned())
                    .collect::<Vec<_>>();
                self.ledger.queue_with_dependencies(
                    task_id.clone(),
                    Some(ROOT_TASK_ID.into()),
                    dependencies,
                    step.seat.role_id(),
                    &step.title,
                    task_mode(step),
                )?;
                self.logical_to_task.insert(step.id.clone(), task_id);
                remaining.remove(&step.id);
            }
            if remaining.len() == before {
                return Err(PlanTaskError::InvalidPlan(format!(
                    "unresolved or cyclic steps: {}",
                    remaining.into_iter().collect::<Vec<_>>().join(", ")
                )));
            }
        }
        Ok(())
    }

    fn reconcile_settled_steps(&mut self, plan: &Plan) -> Result<(), PlanTaskError> {
        for step in &plan.steps {
            let task_id = self.logical_to_task.get(&step.id).cloned().ok_or_else(|| {
                PlanTaskError::InvalidPlan(format!("missing queued task for {}", step.id))
            })?;
            let state = self
                .ledger
                .task(&task_id)
                .map(|task| task.state)
                .ok_or_else(|| PlanTaskError::InvalidPlan(format!("missing task {task_id}")))?;
            match (step.status, state) {
                (StepStatus::Done, AgentTaskState::Queued) => {
                    self.ledger.start(&task_id)?;
                    self.ledger.succeed(
                        &task_id,
                        AgentTaskOutcome::success(
                            "restored from the persisted accepted plan step",
                            step.files.all().map(str::to_string).collect(),
                        ),
                    )?;
                }
                (StepStatus::Done, AgentTaskState::Interrupted | AgentTaskState::Waiting) => {
                    self.ledger.resume(&task_id)?;
                    self.ledger.succeed(
                        &task_id,
                        AgentTaskOutcome::success(
                            "restored from the persisted accepted plan step",
                            step.files.all().map(str::to_string).collect(),
                        ),
                    )?;
                }
                (StepStatus::Blocked, AgentTaskState::Queued) => {
                    self.ledger.unavailable(
                        &task_id,
                        AgentTaskOutcome::blocked(
                            "persisted plan step is blocked",
                            vec!["no verified successful attempt".into()],
                        ),
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn queue_retry(&mut self, plan: &Plan, step: &PlanStep) -> Result<String, PlanTaskError> {
        let base = step_task_base(&step.id);
        let attempt = self
            .ledger
            .tasks()
            .filter(|task| task.task_id.starts_with(&base))
            .count()
            .saturating_add(1);
        let task_id = step_task_id(&step.id, attempt);
        let dependencies = step
            .depends_on
            .iter()
            .map(|dependency| {
                self.logical_to_task
                    .get(dependency)
                    .cloned()
                    .ok_or_else(|| {
                        PlanTaskError::InvalidPlan(format!(
                            "{} depends on missing {dependency}",
                            step.id
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !plan.steps.iter().any(|candidate| candidate.id == step.id) {
            return Err(PlanTaskError::InvalidPlan(format!(
                "step {} is not in the active plan",
                step.id
            )));
        }
        self.ledger.queue_with_dependencies(
            task_id.clone(),
            Some(ROOT_TASK_ID.into()),
            dependencies,
            step.seat.role_id(),
            &step.title,
            task_mode(step),
        )?;
        self.logical_to_task
            .insert(step.id.clone(), task_id.clone());
        Ok(task_id)
    }
}

fn ensure_root(ledger: &mut AgentTaskLedger, requirement: &str) -> Result<(), PlanTaskError> {
    if ledger.task(ROOT_TASK_ID).is_none() {
        ledger.queue(
            ROOT_TASK_ID,
            None,
            "director",
            requirement,
            AgentTaskMode::ReadOnly,
        )?;
    }
    let root_state = ledger
        .task(ROOT_TASK_ID)
        .map(|task| task.state)
        .ok_or_else(|| PlanTaskError::InvalidPlan("director root task is missing".to_string()))?;
    match root_state {
        AgentTaskState::Queued => {
            ledger.start(ROOT_TASK_ID)?;
        }
        AgentTaskState::Waiting | AgentTaskState::Interrupted => {
            ledger.resume(ROOT_TASK_ID)?;
        }
        AgentTaskState::Running => {}
        state => {
            return Err(PlanTaskError::InvalidPlan(format!(
                "director task is already terminal: {state:?}"
            )))
        }
    }
    Ok(())
}

fn existing_step_tasks(ledger: &AgentTaskLedger, plan: &Plan) -> BTreeMap<String, String> {
    plan.steps
        .iter()
        .filter_map(|step| {
            let base = step_task_base(&step.id);
            ledger
                .tasks()
                .filter(|task| task.task_id.starts_with(&base))
                .map(|task| task.task_id.clone())
                .max()
                .map(|task_id| (step.id.clone(), task_id))
        })
        .collect()
}

fn task_mode(step: &PlanStep) -> AgentTaskMode {
    let _ = step;
    // A review step may run its bounded repair turn on the main base session.
    // Record the composite step at its maximum capability; individual critics
    // remain isolated read-only work.
    AgentTaskMode::Writer
}

fn step_task_base(logical_id: &str) -> String {
    let digest = Sha256::digest(format!("plan-step-v1\0{logical_id}").as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("step-{suffix}-")
}

fn step_task_id(logical_id: &str, attempt: usize) -> String {
    format!("{}{:03}", step_task_base(logical_id), attempt)
}

fn base_agent_task_id(parent_task_id: &str, source_key: &str) -> String {
    let digest =
        Sha256::digest(format!("base-native-agent-v1\0{parent_task_id}\0{source_key}").as_bytes());
    let suffix = digest[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("native-{suffix}")
}

fn plan_scope(backend: &str, requirement: &str, plan: &Plan) -> Result<String, PlanTaskError> {
    let mut identity = plan.clone();
    for step in &mut identity.steps {
        step.status = StepStatus::Pending;
    }
    let plan_json = serde_json::to_string(&identity)
        .map_err(|error| PlanTaskError::InvalidPlan(error.to_string()))?;
    Ok(format!(
        "director-plan-v1\0{backend}\0{requirement}\0{plan_json}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critics::Seat;
    use crate::plan_state::{AcceptanceSpec, StepFiles, StepKind};
    use umadev_runtime::{BackgroundTaskSignal, SessionEvent};

    fn step(id: &str, kind: StepKind, depends_on: &[&str]) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: format!("work {id}"),
            seat: if kind == StepKind::Build {
                Seat::BackendEngineer
            } else {
                Seat::QaEngineer
            },
            kind,
            depends_on: depends_on.iter().map(|value| (*value).into()).collect(),
            acceptance: if kind == StepKind::Build {
                AcceptanceSpec::SourcePresent
            } else {
                AcceptanceSpec::ReviewClean
            },
            evidence: Vec::new(),
            files: if kind == StepKind::Build {
                StepFiles {
                    create: vec![format!("src/{id}.rs")],
                    modify: Vec::new(),
                }
            } else {
                StepFiles::default()
            },
            status: StepStatus::Pending,
        }
    }

    fn plan() -> Plan {
        Plan {
            steps: vec![
                step("api", StepKind::Build, &[]),
                step("review", StepKind::Review, &["api"]),
            ],
            risks: Vec::new(),
            open_questions: Vec::new(),
        }
    }

    #[test]
    fn tracks_parent_dependency_and_single_writer_to_clean_finish() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan();
        let mut tracker = PlanTaskTracker::open(temp.path(), "codex", "build API", &plan).unwrap();
        let review_task_id = tracker.logical_to_task.get("review").unwrap();
        assert_eq!(
            tracker.ledger.task(review_task_id).unwrap().mode,
            AgentTaskMode::Writer,
            "a composite review step may invoke the main-session repair turn"
        );
        tracker.start_step(&plan, &plan.steps[0]).unwrap();
        tracker
            .settle_step(&plan.steps[0], StepStatus::Done, false, "green", vec![])
            .unwrap();
        tracker.start_step(&plan, &plan.steps[1]).unwrap();
        tracker
            .settle_step(
                &plan.steps[1],
                StepStatus::Done,
                false,
                "review passed",
                vec![],
            )
            .unwrap();
        assert_eq!(
            tracker.finish(true, "delivered", vec![]).unwrap(),
            RunReadiness::Succeeded
        );
    }

    #[test]
    fn open_existing_does_not_turn_an_empty_crash_pointer_into_a_fake_plan_run() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan();
        let scope = plan_scope("codex", "build API", &plan).unwrap();
        let empty = AgentTaskLedger::open_scoped(temp.path(), &scope).unwrap();
        assert!(empty.tasks().next().is_none());
        let entries_before = std::fs::read_dir(temp.path().join(".umadev/agent-tasks"))
            .unwrap()
            .count();

        assert!(
            PlanTaskTracker::open_existing(temp.path(), "codex", "build API", &plan)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            std::fs::read_dir(temp.path().join(".umadev/agent-tasks"))
                .unwrap()
                .count(),
            entries_before,
            "the settlement probe is read-only and never mints a ledger"
        );
    }

    #[test]
    fn failed_finish_terminalizes_unstarted_and_active_children() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan();
        let mut tracker =
            PlanTaskTracker::open(temp.path(), "grok-build", "build API", &plan).unwrap();
        let failed_run = tracker.run_id().to_string();
        tracker.start_step(&plan, &plan.steps[0]).unwrap();
        let parent_task_id = tracker.logical_to_task.get("api").unwrap().clone();
        tracker
            .ledger
            .queue(
                "native-cleanup-test",
                Some(parent_task_id),
                "base-native-agent",
                "delegated work",
                AgentTaskMode::ParentManaged,
            )
            .unwrap();
        tracker.ledger.start("native-cleanup-test").unwrap();

        assert!(matches!(
            tracker
                .finish(false, "base stopped", vec!["transport ended".into()])
                .unwrap(),
            RunReadiness::Blocked(_)
        ));
        assert!(tracker.tasks().all(|task| task.state.is_terminal()));

        let reopened =
            PlanTaskTracker::open(temp.path(), "grok-build", "build API", &plan).unwrap();
        assert_ne!(reopened.run_id(), failed_run);
    }

    #[test]
    fn crash_resume_and_retry_keep_immutable_attempts() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan();
        let run_id = {
            let mut tracker =
                PlanTaskTracker::open(temp.path(), "claude-code", "build API", &plan).unwrap();
            tracker.start_step(&plan, &plan.steps[0]).unwrap();
            tracker.run_id().to_string()
        };
        let mut resumed =
            PlanTaskTracker::open(temp.path(), "claude-code", "build API", &plan).unwrap();
        assert_eq!(resumed.run_id(), run_id);
        resumed.start_step(&plan, &plan.steps[0]).unwrap();
        resumed
            .settle_step(
                &plan.steps[0],
                StepStatus::Blocked,
                true,
                "base unavailable",
                vec!["transport ended".into()],
            )
            .unwrap();
        resumed.start_step(&plan, &plan.steps[0]).unwrap();
        assert_eq!(
            resumed
                .tasks()
                .filter(|task| task.role == "backend-engineer")
                .count(),
            2
        );
    }

    #[test]
    fn human_gate_reopens_same_run() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan();
        let run_id = {
            let mut tracker =
                PlanTaskTracker::open(temp.path(), "opencode", "build API", &plan).unwrap();
            tracker.wait_for_user("preview confirmation").unwrap();
            tracker.run_id().to_string()
        };
        let reopened = PlanTaskTracker::open(temp.path(), "opencode", "build API", &plan).unwrap();
        assert_eq!(reopened.run_id(), run_id);
        assert_eq!(
            reopened.ledger.task(ROOT_TASK_ID).unwrap().state,
            AgentTaskState::Running
        );
    }

    #[test]
    fn base_native_children_are_hashed_parented_and_settled_by_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan();
        let step = &plan.steps[0];
        let mut tracker =
            PlanTaskTracker::open(temp.path(), "claude-code", "build API", &plan).unwrap();
        tracker.start_step(&plan, step).unwrap();

        let raw_vendor_id = "account@example.test/session-secret/child-7";
        let mut observed = crate::bg_agents::BgAgentTracker::new();
        observed.observe(&SessionEvent::BackgroundTask(
            BackgroundTaskSignal::Started {
                id: raw_vendor_id.into(),
            },
        ));
        observed.observe(&SessionEvent::BackgroundTask(
            BackgroundTaskSignal::Finished {
                id: raw_vendor_id.into(),
            },
        ));
        let observation = observed.observation();
        assert_eq!(
            tracker
                .settle_base_agents(
                    step,
                    &observation,
                    StepStatus::Done,
                    false,
                    "mechanical acceptance passed",
                    &[],
                )
                .unwrap(),
            1
        );
        tracker
            .settle_step(step, StepStatus::Done, false, "green", vec![])
            .unwrap();

        let child = tracker
            .tasks()
            .find(|task| task.role == "base-native-agent")
            .unwrap();
        assert_eq!(child.mode, AgentTaskMode::ParentManaged);
        assert_eq!(child.state, AgentTaskState::Succeeded);
        assert!(child
            .parent_task_id
            .as_deref()
            .is_some_and(|id| id.starts_with("step-")));
        assert!(!child.task_id.contains("account"));

        let journal = std::fs::read_dir(
            temp.path()
                .join(".umadev/agent-tasks")
                .join(tracker.run_id()),
        )
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect::<String>();
        assert!(!journal.contains(raw_vendor_id));
        assert!(!journal.contains("account@example.test"));
    }
}
