//! Durable lifecycle for UmaDev-owned agent tasks.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use umadev_governance::redaction::redact_text;

const SCHEMA_VERSION: u8 = 1;
const MAX_TASKS: usize = 256;
const MAX_OBJECTIVE_CHARS: usize = 2_048;
const MAX_DETAIL_CHARS: usize = 2_048;
const MAX_RESULT_CHARS: usize = 4_096;
const MAX_ARTIFACTS: usize = 64;
const MAX_BLOCKERS: usize = 32;
const SCOPE_SCHEMA_VERSION: u8 = 1;
const ENTRY_TASK_ID: &str = "entry";

static RUN_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether a task may mutate the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskMode {
    /// The task owns the single workspace-writer slot.
    Writer,
    /// The task may inspect but must not modify the workspace.
    ReadOnly,
    /// The task runs inside its active parent's base session and inherits that
    /// parent's workspace ownership. It is not a second independent writer.
    ParentManaged,
}

/// Durable state of one agent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskState {
    /// Created but not started.
    Queued,
    /// Currently executing.
    Running,
    /// Paused for a dependency or external answer.
    Waiting,
    /// Finished with verified success.
    Succeeded,
    /// Finished with a known failure.
    Failed,
    /// Stopped by the user or coordinator.
    Cancelled,
    /// Could not run or produce a trustworthy result.
    Unavailable,
    /// Replaced by an explicit re-plan before completion.
    Superseded,
    /// Was active when its owning process ended.
    Interrupted,
}

impl AgentTaskState {
    /// Whether no further transition is allowed.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Unavailable | Self::Superseded
        )
    }

    /// Whether the task currently owns runtime work.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Waiting)
    }
}

/// Bounded result persisted when a task settles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTaskOutcome {
    /// Human-readable result summary.
    pub summary: String,
    /// Workspace-relative artifacts produced or verified.
    pub artifacts: Vec<String>,
    /// Concrete reasons the task did not succeed.
    pub blockers: Vec<String>,
}

impl AgentTaskOutcome {
    /// Construct a successful outcome.
    #[must_use]
    pub fn success(summary: impl Into<String>, artifacts: Vec<String>) -> Self {
        Self {
            summary: summary.into(),
            artifacts,
            blockers: Vec::new(),
        }
    }

    /// Construct a failed or unavailable outcome.
    #[must_use]
    pub fn blocked(summary: impl Into<String>, blockers: Vec<String>) -> Self {
        Self {
            summary: summary.into(),
            artifacts: Vec::new(),
            blockers,
        }
    }
}

/// Current materialized view of one task's event history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTaskRecord {
    /// Owning run identifier.
    pub run_id: String,
    /// Stable task identifier within the run.
    pub task_id: String,
    /// Optional parent task identifier.
    pub parent_task_id: Option<String>,
    /// Tasks that must succeed before this task may start.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Assigned team role.
    pub role: String,
    /// Bounded task objective.
    pub objective: String,
    /// Workspace access mode.
    pub mode: AgentTaskMode,
    /// Current state.
    pub state: AgentTaskState,
    /// Monotonic per-task revision.
    pub revision: u32,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 last-transition time.
    pub updated_at: String,
    /// Bounded wait, cancel, or recovery detail.
    pub detail: String,
    /// Terminal result when one exists.
    pub outcome: Option<AgentTaskOutcome>,
}

/// Whether a tracked run may publish completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunReadiness {
    /// No tasks were registered.
    NotTracked,
    /// At least one task is queued, running, or waiting.
    InProgress,
    /// Every task finished successfully.
    Succeeded,
    /// One or more tasks failed, were cancelled, became unavailable, or were interrupted.
    Blocked(Vec<String>),
}

/// Read-only projection used by status surfaces such as `/tasks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunSnapshot {
    /// Stable run identifier.
    pub run_id: String,
    /// Pointer creation time used for newest-first ordering.
    pub created_at: String,
    /// Mechanically-derived run state.
    pub readiness: RunReadiness,
    /// Current task records, including immutable prior attempts.
    pub tasks: Vec<AgentTaskRecord>,
}

/// Durable lifecycle validation, persistence, or replay failure.
#[derive(Debug, Error)]
pub enum TaskLifecycleError {
    /// An identifier is unsafe for use in the journal path.
    #[error("invalid {field}: {value}")]
    InvalidId {
        /// Identifier kind.
        field: &'static str,
        /// Rejected value, bounded for display.
        value: String,
    },
    /// The task id is already registered.
    #[error("agent task already exists: {0}")]
    DuplicateTask(String),
    /// No task has the requested id.
    #[error("agent task not found: {0}")]
    TaskNotFound(String),
    /// A child referenced an unknown parent.
    #[error("parent agent task not found: {0}")]
    ParentNotFound(String),
    /// A parent-managed child was started without a live parent turn.
    #[error("parent agent task is not active: {0}")]
    ParentNotActive(String),
    /// A parent-managed child did not inherit a real writer owner.
    #[error("parent-managed agent task {task_id} requires a writer parent")]
    ParentManagedNeedsWriter {
        /// Child task whose ownership is invalid.
        task_id: String,
    },
    /// A parent attempted to settle before its managed child settled.
    #[error("agent task {task_id} still owns unfinished parent-managed child {child_task_id}")]
    UnfinishedManagedChild {
        /// Parent task attempting to leave its owning state.
        task_id: String,
        /// Child that must settle first.
        child_task_id: String,
    },
    /// A task referenced an unknown dependency.
    #[error("agent task dependency not found: {0}")]
    DependencyNotFound(String),
    /// One or more dependencies have not succeeded.
    #[error("agent task dependencies are not ready: {0}")]
    DependenciesNotReady(String),
    /// The per-run task bound was reached.
    #[error("agent task limit reached ({MAX_TASKS})")]
    TaskLimit,
    /// The requested state transition is not legal.
    #[error("illegal agent task transition for {task_id}: {from:?} -> {to:?}")]
    IllegalTransition {
        /// Task being transitioned.
        task_id: String,
        /// Current state.
        from: AgentTaskState,
        /// Requested state.
        to: AgentTaskState,
    },
    /// Another writer task is active.
    #[error("writer task {active_task_id} is already active")]
    WriterBusy {
        /// Active writer task id.
        active_task_id: String,
    },
    /// A successful result incorrectly included blockers.
    #[error("successful task outcome must not contain blockers")]
    SuccessHasBlockers,
    /// A success, failure, or unavailable transition omitted its result.
    #[error("terminal task outcome is missing")]
    MissingOutcome,
    /// An artifact path escaped the workspace-relative contract.
    #[error("invalid artifact path: {0}")]
    InvalidArtifact(String),
    /// Immutable events could not be replayed consistently.
    #[error("task lifecycle journal is corrupt: {0}")]
    CorruptJournal(String),
    /// Journal I/O failed.
    #[error("task lifecycle I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Event serialization failed.
    #[error("task lifecycle serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskEvent {
    version: u8,
    run_id: String,
    sequence: u64,
    at: String,
    body: TaskEventBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScopedRunPointer {
    version: u8,
    scope_sha256: String,
    run_id: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaskEventBody {
    Created {
        record: AgentTaskRecord,
    },
    Transition {
        task_id: String,
        revision: u32,
        from: AgentTaskState,
        to: AgentTaskState,
        detail: String,
        outcome: Option<AgentTaskOutcome>,
    },
}

/// Append-only, replayable ledger for one run's agent tasks.
#[derive(Debug)]
pub struct AgentTaskLedger {
    root: PathBuf,
    run_id: String,
    sequence: u64,
    tasks: BTreeMap<String, AgentTaskRecord>,
}

impl AgentTaskLedger {
    /// Open the newest unfinished ledger for a logical scope without creating a
    /// pointer or journal when none exists. Cancellation/recovery callers use
    /// this read-before-write surface so inspecting a resident-only pause can
    /// never fabricate a Director ledger.
    pub(crate) fn open_scoped_existing(
        project_root: &Path,
        scope_key: &str,
    ) -> Result<Option<Self>, TaskLifecycleError> {
        let scope_sha256 = hex_digest(scope_key.as_bytes());
        let parent = project_root.join(".umadev").join("agent-tasks");
        let prefix = format!("scope-{}-", &scope_sha256[..20]);
        let mut pointers = match std::fs::read_dir(&parent) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix))
                        && path
                            .extension()
                            .is_some_and(|extension| extension == "json")
                })
                .filter_map(|path| {
                    let pointer =
                        serde_json::from_slice::<ScopedRunPointer>(&std::fs::read(&path).ok()?)
                            .ok()?;
                    (pointer.version == SCOPE_SCHEMA_VERSION
                        && pointer.scope_sha256 == scope_sha256
                        && validate_id("run id", &pointer.run_id).is_ok())
                    .then_some(pointer)
                })
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        pointers.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        for pointer in pointers {
            let ledger = Self::open(project_root, pointer.run_id)?;
            if ledger.tasks.is_empty()
                || ledger.tasks.values().any(|task| {
                    matches!(
                        task.state,
                        AgentTaskState::Queued
                            | AgentTaskState::Running
                            | AgentTaskState::Waiting
                            | AgentTaskState::Interrupted
                    )
                })
            {
                return Ok(Some(ledger));
            }
        }
        Ok(None)
    }

    /// Open the newest resumable ledger for a logical plan, or mint a new run.
    ///
    /// Only a SHA-256 digest of `scope_key` is persisted. This keeps requirements
    /// and credentials out of pointer metadata while making a gate/crash resume
    /// deterministic across processes.
    pub fn open_scoped(project_root: &Path, scope_key: &str) -> Result<Self, TaskLifecycleError> {
        let scope_sha256 = hex_digest(scope_key.as_bytes());
        if let Some(ledger) = Self::open_scoped_existing(project_root, scope_key)? {
            return Ok(ledger);
        }
        let parent = project_root.join(".umadev").join("agent-tasks");
        let run_id = mint_agent_run_id(&scope_sha256);
        let pointer = ScopedRunPointer {
            version: SCOPE_SCHEMA_VERSION,
            scope_sha256,
            run_id: run_id.clone(),
            created_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        publish_scoped_pointer(&parent, &pointer)?;
        Self::open(project_root, run_id)
    }

    /// Open and replay a run ledger, or create an empty one when absent.
    pub fn open(
        project_root: &Path,
        run_id: impl Into<String>,
    ) -> Result<Self, TaskLifecycleError> {
        let run_id = run_id.into();
        validate_id("run id", &run_id)?;
        let root = project_root
            .join(".umadev")
            .join("agent-tasks")
            .join(&run_id);
        let mut ledger = Self {
            root,
            run_id,
            sequence: 0,
            tasks: BTreeMap::new(),
        };
        ledger.load()?;
        Ok(ledger)
    }

    /// Stable identifier of the tracked run.
    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Materialized tasks in stable id order.
    pub fn tasks(&self) -> impl Iterator<Item = &AgentTaskRecord> {
        self.tasks.values()
    }

    /// Look up one task.
    #[must_use]
    pub fn task(&self, task_id: &str) -> Option<&AgentTaskRecord> {
        self.tasks.get(task_id)
    }

    /// Register a queued task and durably link it to an optional parent.
    pub fn queue(
        &mut self,
        task_id: impl Into<String>,
        parent_task_id: Option<String>,
        role: impl Into<String>,
        objective: impl Into<String>,
        mode: AgentTaskMode,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.queue_with_dependencies(task_id, parent_task_id, Vec::new(), role, objective, mode)
    }

    /// Register a queued task with explicit predecessor tasks.
    pub fn queue_with_dependencies(
        &mut self,
        task_id: impl Into<String>,
        parent_task_id: Option<String>,
        depends_on: Vec<String>,
        role: impl Into<String>,
        objective: impl Into<String>,
        mode: AgentTaskMode,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        let task_id = task_id.into();
        validate_id("task id", &task_id)?;
        if self.tasks.contains_key(&task_id) {
            return Err(TaskLifecycleError::DuplicateTask(task_id));
        }
        if self.tasks.len() >= MAX_TASKS {
            return Err(TaskLifecycleError::TaskLimit);
        }
        if let Some(parent) = &parent_task_id {
            validate_id("parent task id", parent)?;
            if !self.tasks.contains_key(parent) {
                return Err(TaskLifecycleError::ParentNotFound(parent.clone()));
            }
        }
        if mode == AgentTaskMode::ParentManaged
            && parent_task_id
                .as_deref()
                .and_then(|parent| self.tasks.get(parent))
                .map(|parent| parent.mode)
                != Some(AgentTaskMode::Writer)
        {
            return Err(TaskLifecycleError::ParentManagedNeedsWriter {
                task_id: task_id.clone(),
            });
        }
        let mut unique_dependencies = Vec::with_capacity(depends_on.len());
        for dependency in depends_on {
            validate_id("dependency task id", &dependency)?;
            if !self.tasks.contains_key(&dependency) {
                return Err(TaskLifecycleError::DependencyNotFound(dependency));
            }
            if !unique_dependencies.contains(&dependency) {
                unique_dependencies.push(dependency);
            }
        }
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let record = AgentTaskRecord {
            run_id: self.run_id.clone(),
            task_id: task_id.clone(),
            parent_task_id,
            depends_on: unique_dependencies,
            role: sanitize(&role.into(), 128),
            objective: sanitize(&objective.into(), MAX_OBJECTIVE_CHARS),
            mode,
            state: AgentTaskState::Queued,
            revision: 0,
            created_at: now.clone(),
            updated_at: now.clone(),
            detail: String::new(),
            outcome: None,
        };
        let sequence = self.sequence.saturating_add(1);
        let event = TaskEvent {
            version: SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            sequence,
            at: now,
            body: TaskEventBody::Created {
                record: record.clone(),
            },
        };
        self.append(&event)?;
        self.sequence = sequence;
        self.tasks.insert(task_id.clone(), record);
        self.tasks.get(&task_id).ok_or_else(|| {
            TaskLifecycleError::CorruptJournal("inserted task disappeared".to_string())
        })
    }

    /// Start a queued task, enforcing the single-writer invariant.
    pub fn start(&mut self, task_id: &str) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.ensure_dependencies_ready(task_id)?;
        self.ensure_parent_active(task_id)?;
        self.ensure_writer_available(task_id)?;
        self.transition(task_id, AgentTaskState::Running, "", None)
    }

    /// Mark a running task as waiting with an explicit reason.
    pub fn wait(
        &mut self,
        task_id: &str,
        detail: impl Into<String>,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.transition(task_id, AgentTaskState::Waiting, &detail.into(), None)
    }

    /// Resume a waiting or interrupted task.
    pub fn resume(&mut self, task_id: &str) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.ensure_dependencies_ready(task_id)?;
        self.ensure_parent_active(task_id)?;
        self.ensure_writer_available(task_id)?;
        self.transition(task_id, AgentTaskState::Running, "resumed", None)
    }

    /// Settle a running task with verified success.
    pub fn succeed(
        &mut self,
        task_id: &str,
        outcome: AgentTaskOutcome,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        if !outcome.blockers.is_empty() {
            return Err(TaskLifecycleError::SuccessHasBlockers);
        }
        self.transition(task_id, AgentTaskState::Succeeded, "", Some(outcome))
    }

    /// Settle a task with a known failure.
    pub fn fail(
        &mut self,
        task_id: &str,
        outcome: AgentTaskOutcome,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.transition(task_id, AgentTaskState::Failed, "", Some(outcome))
    }

    /// Settle a task that could not produce a trustworthy result.
    pub fn unavailable(
        &mut self,
        task_id: &str,
        outcome: AgentTaskOutcome,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.transition(task_id, AgentTaskState::Unavailable, "", Some(outcome))
    }

    /// Retire work that an explicit re-plan replaced.
    pub fn supersede(
        &mut self,
        task_id: &str,
        detail: impl Into<String>,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.transition(task_id, AgentTaskState::Superseded, &detail.into(), None)
    }

    /// Cancel a queued, running, waiting, or interrupted task.
    pub fn cancel(
        &mut self,
        task_id: &str,
        detail: impl Into<String>,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.transition(task_id, AgentTaskState::Cancelled, &detail.into(), None)
    }

    /// Mark live work as interrupted without claiming failure or success.
    pub fn interrupt(
        &mut self,
        task_id: &str,
        detail: impl Into<String>,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        self.transition(task_id, AgentTaskState::Interrupted, &detail.into(), None)
    }

    /// Convert active tasks left by a previous process into resumable interruptions.
    pub fn recover_interrupted(&mut self) -> Result<usize, TaskLifecycleError> {
        let mut active = self
            .tasks
            .values()
            .filter(|task| task.state.is_active())
            .map(|task| {
                (
                    task.mode != AgentTaskMode::ParentManaged,
                    task.task_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        active.sort();
        for (_, task_id) in &active {
            self.transition(
                task_id,
                AgentTaskState::Interrupted,
                "process ended before the task reached a terminal outcome",
                None,
            )?;
        }
        Ok(active.len())
    }

    /// Compute whether the run can truthfully publish success.
    #[must_use]
    pub fn readiness(&self) -> RunReadiness {
        if self.tasks.is_empty() {
            return RunReadiness::NotTracked;
        }
        let mut blocked = Vec::new();
        let mut in_progress = false;
        for task in self.tasks.values() {
            match task.state {
                AgentTaskState::Succeeded | AgentTaskState::Superseded => {}
                AgentTaskState::Failed
                | AgentTaskState::Cancelled
                | AgentTaskState::Unavailable
                | AgentTaskState::Interrupted => blocked.push(format!(
                    "{} ({}) is {}{}",
                    task.task_id,
                    task.role,
                    state_id(task.state),
                    task.outcome
                        .as_ref()
                        .filter(|outcome| !outcome.summary.is_empty())
                        .map_or_else(String::new, |outcome| format!(": {}", outcome.summary))
                )),
                AgentTaskState::Queued | AgentTaskState::Running | AgentTaskState::Waiting => {
                    in_progress = true;
                }
            }
        }
        if !blocked.is_empty() {
            RunReadiness::Blocked(blocked)
        } else if in_progress {
            RunReadiness::InProgress
        } else {
            RunReadiness::Succeeded
        }
    }

    fn transition(
        &mut self,
        task_id: &str,
        to: AgentTaskState,
        detail: &str,
        outcome: Option<AgentTaskOutcome>,
    ) -> Result<&AgentTaskRecord, TaskLifecycleError> {
        let current = self
            .tasks
            .get(task_id)
            .ok_or_else(|| TaskLifecycleError::TaskNotFound(task_id.to_string()))?;
        let from = current.state;
        if !legal_transition(from, to) {
            return Err(TaskLifecycleError::IllegalTransition {
                task_id: task_id.to_string(),
                from,
                to,
            });
        }
        self.ensure_managed_children_can_exit(task_id, to)?;
        if matches!(
            to,
            AgentTaskState::Succeeded | AgentTaskState::Failed | AgentTaskState::Unavailable
        ) && outcome.is_none()
        {
            return Err(TaskLifecycleError::MissingOutcome);
        }
        let outcome = outcome.map(normalize_outcome).transpose()?;
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let revision = current.revision.saturating_add(1);
        let sequence = self.sequence.saturating_add(1);
        let event = TaskEvent {
            version: SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            sequence,
            at: now.clone(),
            body: TaskEventBody::Transition {
                task_id: task_id.to_string(),
                revision,
                from,
                to,
                detail: sanitize(detail, MAX_DETAIL_CHARS),
                outcome: outcome.clone(),
            },
        };
        self.append(&event)?;
        self.sequence = sequence;
        let task = self.tasks.get_mut(task_id).ok_or_else(|| {
            TaskLifecycleError::CorruptJournal(format!(
                "task {task_id} disappeared during transition"
            ))
        })?;
        task.state = to;
        task.revision = revision;
        task.updated_at = now;
        task.detail = sanitize(detail, MAX_DETAIL_CHARS);
        task.outcome = outcome;
        Ok(task)
    }

    fn ensure_writer_available(&self, task_id: &str) -> Result<(), TaskLifecycleError> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| TaskLifecycleError::TaskNotFound(task_id.to_string()))?;
        if task.mode == AgentTaskMode::Writer {
            if let Some(active) = self.tasks.values().find(|candidate| {
                candidate.task_id != task_id
                    && candidate.mode == AgentTaskMode::Writer
                    && candidate.state.is_active()
            }) {
                return Err(TaskLifecycleError::WriterBusy {
                    active_task_id: active.task_id.clone(),
                });
            }
        }
        Ok(())
    }

    fn ensure_parent_active(&self, task_id: &str) -> Result<(), TaskLifecycleError> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| TaskLifecycleError::TaskNotFound(task_id.to_string()))?;
        if task.mode != AgentTaskMode::ParentManaged {
            return Ok(());
        }
        let parent = task
            .parent_task_id
            .as_deref()
            .ok_or_else(|| TaskLifecycleError::ParentNotFound(task_id.to_string()))?;
        if self
            .tasks
            .get(parent)
            .is_some_and(|record| record.state.is_active())
        {
            Ok(())
        } else {
            Err(TaskLifecycleError::ParentNotActive(parent.to_string()))
        }
    }

    fn ensure_managed_children_can_exit(
        &self,
        task_id: &str,
        to: AgentTaskState,
    ) -> Result<(), TaskLifecycleError> {
        let unfinished = self.tasks.values().find(|candidate| {
            candidate.parent_task_id.as_deref() == Some(task_id)
                && candidate.mode == AgentTaskMode::ParentManaged
                && if to == AgentTaskState::Interrupted {
                    candidate.state.is_active()
                } else if to.is_terminal() {
                    !candidate.state.is_terminal()
                } else {
                    false
                }
        });
        if let Some(child) = unfinished {
            Err(TaskLifecycleError::UnfinishedManagedChild {
                task_id: task_id.to_string(),
                child_task_id: child.task_id.clone(),
            })
        } else {
            Ok(())
        }
    }

    fn ensure_dependencies_ready(&self, task_id: &str) -> Result<(), TaskLifecycleError> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| TaskLifecycleError::TaskNotFound(task_id.to_string()))?;
        let pending = task
            .depends_on
            .iter()
            .filter(|dependency| {
                self.tasks
                    .get(*dependency)
                    .is_none_or(|record| record.state != AgentTaskState::Succeeded)
            })
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            Ok(())
        } else {
            Err(TaskLifecycleError::DependenciesNotReady(pending.join(", ")))
        }
    }

    fn append(&self, event: &TaskEvent) -> Result<(), TaskLifecycleError> {
        std::fs::create_dir_all(&self.root)?;
        let body = serde_json::to_vec(event)?;
        let name = format!("{:020}.json", event.sequence);
        let final_path = self.root.join(&name);
        if final_path.exists() {
            return Err(TaskLifecycleError::CorruptJournal(format!(
                "duplicate event sequence {}",
                event.sequence
            )));
        }
        let temp_sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.root.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            temp_sequence
        ));
        let mut temp = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        temp.write_all(&body)?;
        temp.sync_all()?;
        drop(temp);
        let publish = std::fs::hard_link(&temp_path, &final_path);
        let _ = std::fs::remove_file(&temp_path);
        publish?;
        Ok(())
    }

    fn load(&mut self) -> Result<(), TaskLifecycleError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let event: TaskEvent =
                serde_json::from_slice(&std::fs::read(&path)?).map_err(|error| {
                    TaskLifecycleError::CorruptJournal(format!("{}: {error}", path.display()))
                })?;
            self.apply_loaded(event, &path)?;
        }
        Ok(())
    }

    fn apply_loaded(&mut self, event: TaskEvent, path: &Path) -> Result<(), TaskLifecycleError> {
        if event.version != SCHEMA_VERSION || event.run_id != self.run_id {
            return Err(TaskLifecycleError::CorruptJournal(format!(
                "{} has an incompatible schema or run id",
                path.display()
            )));
        }
        if event.sequence != self.sequence.saturating_add(1) {
            return Err(TaskLifecycleError::CorruptJournal(format!(
                "{} breaks the event sequence",
                path.display()
            )));
        }
        self.sequence = event.sequence;
        match event.body {
            TaskEventBody::Created { record } => {
                if record.run_id != self.run_id
                    || record.state != AgentTaskState::Queued
                    || record.revision != 0
                    || self.tasks.contains_key(&record.task_id)
                    || self.tasks.len() >= MAX_TASKS
                    || validate_id("task id", &record.task_id).is_err()
                    || record
                        .parent_task_id
                        .as_deref()
                        .is_some_and(|parent| validate_id("parent task id", parent).is_err())
                    || record
                        .depends_on
                        .iter()
                        .any(|dependency| validate_id("dependency task id", dependency).is_err())
                    || record.role.chars().count() > 128
                    || record.objective.chars().count() > MAX_OBJECTIVE_CHARS
                    || record.detail.chars().count() > MAX_DETAIL_CHARS
                    || sanitize(&record.role, 128) != record.role
                    || sanitize(&record.objective, MAX_OBJECTIVE_CHARS) != record.objective
                    || sanitize(&record.detail, MAX_DETAIL_CHARS) != record.detail
                    || record.outcome.is_some()
                {
                    return Err(TaskLifecycleError::CorruptJournal(format!(
                        "{} has an invalid create event",
                        path.display()
                    )));
                }
                if let Some(parent) = &record.parent_task_id {
                    let Some(parent_record) = self.tasks.get(parent) else {
                        return Err(TaskLifecycleError::CorruptJournal(format!(
                            "{} references a missing parent",
                            path.display()
                        )));
                    };
                    if record.mode == AgentTaskMode::ParentManaged
                        && parent_record.mode != AgentTaskMode::Writer
                    {
                        return Err(TaskLifecycleError::CorruptJournal(format!(
                            "{} gives a parent-managed task no writer parent",
                            path.display()
                        )));
                    }
                } else if record.mode == AgentTaskMode::ParentManaged {
                    return Err(TaskLifecycleError::CorruptJournal(format!(
                        "{} gives a parent-managed task no parent",
                        path.display()
                    )));
                }
                for dependency in &record.depends_on {
                    if !self.tasks.contains_key(dependency) {
                        return Err(TaskLifecycleError::CorruptJournal(format!(
                            "{} references a missing dependency",
                            path.display()
                        )));
                    }
                }
                self.tasks.insert(record.task_id.clone(), record);
            }
            TaskEventBody::Transition {
                task_id,
                revision,
                from,
                to,
                detail,
                outcome,
            } => {
                let task = self.tasks.get(&task_id).ok_or_else(|| {
                    TaskLifecycleError::CorruptJournal(format!(
                        "{} transitions a missing task",
                        path.display()
                    ))
                })?;
                if task.state != from
                    || revision != task.revision.saturating_add(1)
                    || !legal_transition(from, to)
                    || terminal_outcome_invalid(to, outcome.as_ref())
                    || detail.chars().count() > MAX_DETAIL_CHARS
                    || sanitize(&detail, MAX_DETAIL_CHARS) != detail
                    || outcome.as_ref().is_some_and(|value| {
                        normalize_outcome(value.clone()).ok().as_ref() != Some(value)
                    })
                {
                    return Err(TaskLifecycleError::CorruptJournal(format!(
                        "{} has an invalid transition",
                        path.display()
                    )));
                }
                if self.ensure_managed_children_can_exit(&task_id, to).is_err() {
                    return Err(TaskLifecycleError::CorruptJournal(format!(
                        "{} settles a parent before its managed child",
                        path.display()
                    )));
                }
                let task = self.tasks.get_mut(&task_id).ok_or_else(|| {
                    TaskLifecycleError::CorruptJournal(format!(
                        "{} transitions a missing task {task_id}",
                        path.display()
                    ))
                })?;
                task.state = to;
                task.revision = revision;
                task.updated_at = event.at;
                task.detail = detail;
                task.outcome = outcome;
            }
        }
        Ok(())
    }
}

/// One durable writer task for a mutating product entry such as quick, redo, or
/// a resident scoped edit. Director plans use [`crate::plan_tasks::PlanTaskTracker`]
/// instead because they own a real dependency graph.
#[derive(Debug)]
pub struct EntryTaskTracker {
    ledger: AgentTaskLedger,
    parked: bool,
    settled: bool,
}

impl EntryTaskTracker {
    /// Open the resumable scope, recover a prior process, and start its writer.
    pub fn begin(
        project_root: &Path,
        scope_key: &str,
        role: &str,
        objective: &str,
    ) -> Result<Self, TaskLifecycleError> {
        let mut ledger = AgentTaskLedger::open_scoped(project_root, scope_key)?;
        ledger.recover_interrupted()?;
        if ledger.task(ENTRY_TASK_ID).is_none() {
            ledger.queue(ENTRY_TASK_ID, None, role, objective, AgentTaskMode::Writer)?;
        }
        let entry_state = ledger
            .task(ENTRY_TASK_ID)
            .map(|task| task.state)
            .ok_or_else(|| {
                TaskLifecycleError::CorruptJournal("entry task is missing after queue".to_string())
            })?;
        match entry_state {
            AgentTaskState::Queued => {
                ledger.start(ENTRY_TASK_ID)?;
            }
            AgentTaskState::Waiting | AgentTaskState::Interrupted => {
                ledger.resume(ENTRY_TASK_ID)?;
            }
            AgentTaskState::Running => {}
            state => {
                return Err(TaskLifecycleError::CorruptJournal(format!(
                    "scoped entry task is unexpectedly terminal: {state:?}"
                )));
            }
        }
        Ok(Self {
            ledger,
            parked: false,
            settled: false,
        })
    }

    /// Reattach to the exact durable entry run that owns a persisted execution
    /// boundary.
    ///
    /// This is intentionally run-id based instead of scope based. A resident
    /// turn can park at a final-review outage and then hand execution to the
    /// director's `/continue` path; reopening by a freshly classified scope
    /// could mint or select a different ledger and leave the original writer
    /// permanently `Waiting`.
    pub fn resume_exact(project_root: &Path, run_id: &str) -> Result<Self, TaskLifecycleError> {
        let mut ledger = AgentTaskLedger::open(project_root, run_id)?;
        ledger.recover_interrupted()?;
        let entry_state = ledger
            .task(ENTRY_TASK_ID)
            .map(|task| task.state)
            .ok_or_else(|| {
                TaskLifecycleError::CorruptJournal(format!(
                    "entry task is missing from persisted run {run_id}"
                ))
            })?;
        match entry_state {
            AgentTaskState::Queued => {
                ledger.start(ENTRY_TASK_ID)?;
            }
            AgentTaskState::Waiting | AgentTaskState::Interrupted => {
                ledger.resume(ENTRY_TASK_ID)?;
            }
            AgentTaskState::Running => {}
            state => {
                return Err(TaskLifecycleError::CorruptJournal(format!(
                    "persisted entry task is unexpectedly terminal: {state:?}"
                )));
            }
        }
        Ok(Self {
            ledger,
            parked: false,
            settled: false,
        })
    }

    /// Cancel the exact persisted entry run if it is still live.
    ///
    /// A fresh run or explicit user cancellation must settle the resident writer
    /// that owns an operational-review checkpoint before erasing that pointer.
    /// Terminal runs are already settled and therefore make this idempotent.
    pub fn cancel_exact(
        project_root: &Path,
        run_id: &str,
        detail: &str,
    ) -> Result<(), TaskLifecycleError> {
        let mut ledger = AgentTaskLedger::open(project_root, run_id)?;
        ledger.recover_interrupted()?;
        let state = ledger
            .task(ENTRY_TASK_ID)
            .map(|task| task.state)
            .ok_or_else(|| {
                TaskLifecycleError::CorruptJournal(format!(
                    "entry task is missing from persisted run {run_id}"
                ))
            })?;
        if state.is_terminal() {
            return Ok(());
        }
        ledger.cancel(ENTRY_TASK_ID, detail)?;
        Ok(())
    }

    /// Stable run id shown by `/tasks`.
    #[must_use]
    pub fn run_id(&self) -> &str {
        self.ledger.run_id()
    }

    /// Pause at a real external gate and keep the scope resumable.
    pub fn wait(&mut self, detail: &str) -> Result<(), TaskLifecycleError> {
        self.ledger.wait(ENTRY_TASK_ID, detail)?;
        self.parked = true;
        Ok(())
    }

    /// Settle only after the caller's mechanical success checks pass.
    pub fn succeed(
        &mut self,
        summary: &str,
        artifacts: Vec<String>,
    ) -> Result<(), TaskLifecycleError> {
        self.ledger
            .succeed(ENTRY_TASK_ID, AgentTaskOutcome::success(summary, artifacts))?;
        self.settled = true;
        Ok(())
    }

    /// Settle a known execution or verification failure.
    pub fn fail(&mut self, summary: &str, blockers: Vec<String>) -> Result<(), TaskLifecycleError> {
        self.ledger
            .fail(ENTRY_TASK_ID, AgentTaskOutcome::blocked(summary, blockers))?;
        self.settled = true;
        Ok(())
    }

    /// Settle an explicitly stopped entry.
    pub fn cancel(&mut self, detail: &str) -> Result<(), TaskLifecycleError> {
        self.ledger.cancel(ENTRY_TASK_ID, detail)?;
        self.settled = true;
        Ok(())
    }

    /// Current mechanically-derived state.
    #[must_use]
    pub fn state(&self) -> AgentTaskState {
        self.ledger
            .task(ENTRY_TASK_ID)
            .map_or(AgentTaskState::Unavailable, |task| task.state)
    }
}

impl Drop for EntryTaskTracker {
    fn drop(&mut self) {
        if self.settled || self.parked {
            return;
        }
        if matches!(
            self.state(),
            AgentTaskState::Running | AgentTaskState::Waiting
        ) {
            let _ = self.ledger.interrupt(
                ENTRY_TASK_ID,
                "execution scope ended before a terminal outcome was recorded",
            );
        }
    }
}

/// Redact credentials before a task summary reaches a secondary status store.
#[must_use]
pub fn redact_task_text(value: &str) -> String {
    redact_text(value)
}

/// Mint an opaque, path-safe run id without storing the seed.
#[must_use]
pub fn mint_agent_run_id(seed: &str) -> String {
    let sequence = RUN_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let digest = Sha256::digest(
        format!(
            "agent-run-v1\0{}\0{stamp}\0{sequence}\0{seed}",
            std::process::id()
        )
        .as_bytes(),
    );
    let mut encoded = String::with_capacity(24);
    for byte in digest.iter().take(10) {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    format!("ar1-{encoded}")
}

/// Load the newest durable agent runs for a read-only status surface.
/// Corrupt pointers or ledgers are omitted; execution paths still report those
/// errors through [`AgentTaskLedger::open_scoped`].
#[must_use]
pub fn recent_agent_runs(project_root: &Path, limit: usize) -> Vec<AgentRunSnapshot> {
    if limit == 0 {
        return Vec::new();
    }
    let parent = project_root.join(".umadev").join("agent-tasks");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut pointers = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("scope-"))
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| {
            serde_json::from_slice::<ScopedRunPointer>(&std::fs::read(path).ok()?).ok()
        })
        .filter(|pointer| {
            pointer.version == SCOPE_SCHEMA_VERSION
                && pointer.scope_sha256.len() == 64
                && validate_id("run id", &pointer.run_id).is_ok()
        })
        .collect::<Vec<_>>();
    pointers.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    let mut seen = std::collections::BTreeSet::new();
    pointers
        .into_iter()
        .filter(|pointer| seen.insert(pointer.run_id.clone()))
        .filter_map(|pointer| {
            let ledger = AgentTaskLedger::open(project_root, pointer.run_id.clone()).ok()?;
            Some(AgentRunSnapshot {
                run_id: pointer.run_id,
                created_at: pointer.created_at,
                readiness: ledger.readiness(),
                tasks: ledger.tasks().cloned().collect(),
            })
        })
        .take(limit.min(32))
        .collect()
}

fn validate_id(field: &'static str, value: &str) -> Result<(), TaskLifecycleError> {
    let redacted = redact_text(value);
    let valid = redacted == value
        && !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(TaskLifecycleError::InvalidId {
            field,
            value: clip(&redacted, 128),
        })
    }
}

fn normalize_outcome(
    mut outcome: AgentTaskOutcome,
) -> Result<AgentTaskOutcome, TaskLifecycleError> {
    outcome.summary = sanitize(&outcome.summary, MAX_RESULT_CHARS);
    outcome.artifacts.truncate(MAX_ARTIFACTS);
    outcome.blockers.truncate(MAX_BLOCKERS);
    for artifact in &mut outcome.artifacts {
        if !valid_artifact_path(artifact) {
            return Err(TaskLifecycleError::InvalidArtifact(sanitize(artifact, 256)));
        }
        *artifact = sanitize(artifact, 512);
        if !valid_artifact_path(artifact) {
            return Err(TaskLifecycleError::InvalidArtifact(artifact.clone()));
        }
    }
    for blocker in &mut outcome.blockers {
        *blocker = sanitize(blocker, MAX_DETAIL_CHARS);
    }
    Ok(outcome)
}

fn valid_artifact_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn legal_transition(from: AgentTaskState, to: AgentTaskState) -> bool {
    use AgentTaskState::{
        Cancelled, Failed, Interrupted, Queued, Running, Succeeded, Superseded, Unavailable,
        Waiting,
    };
    matches!(
        (from, to),
        (Queued, Running | Cancelled | Unavailable | Superseded)
            | (
                Running,
                Waiting | Succeeded | Failed | Cancelled | Unavailable | Superseded | Interrupted
            )
            | (
                Waiting,
                Running | Failed | Cancelled | Unavailable | Superseded | Interrupted
            )
            | (
                Interrupted,
                Running | Failed | Cancelled | Unavailable | Superseded
            )
    )
}

fn terminal_outcome_invalid(state: AgentTaskState, outcome: Option<&AgentTaskOutcome>) -> bool {
    match state {
        AgentTaskState::Succeeded => outcome.is_none_or(|result| {
            !result.blockers.is_empty()
                || result.artifacts.len() > MAX_ARTIFACTS
                || result
                    .artifacts
                    .iter()
                    .any(|path| !valid_artifact_path(path))
        }),
        AgentTaskState::Failed | AgentTaskState::Unavailable => outcome.is_none_or(|result| {
            result.artifacts.len() > MAX_ARTIFACTS
                || result.blockers.len() > MAX_BLOCKERS
                || result
                    .artifacts
                    .iter()
                    .any(|path| !valid_artifact_path(path))
        }),
        AgentTaskState::Queued
        | AgentTaskState::Running
        | AgentTaskState::Waiting
        | AgentTaskState::Cancelled
        | AgentTaskState::Superseded
        | AgentTaskState::Interrupted => outcome.is_some(),
    }
}

fn state_id(state: AgentTaskState) -> &'static str {
    match state {
        AgentTaskState::Queued => "queued",
        AgentTaskState::Running => "running",
        AgentTaskState::Waiting => "waiting",
        AgentTaskState::Succeeded => "succeeded",
        AgentTaskState::Failed => "failed",
        AgentTaskState::Cancelled => "cancelled",
        AgentTaskState::Unavailable => "unavailable",
        AgentTaskState::Superseded => "superseded",
        AgentTaskState::Interrupted => "interrupted",
    }
}

fn publish_scoped_pointer(
    parent: &Path,
    pointer: &ScopedRunPointer,
) -> Result<(), TaskLifecycleError> {
    std::fs::create_dir_all(parent)?;
    let final_path = parent.join(format!(
        "scope-{}-{}.json",
        &pointer.scope_sha256[..20],
        pointer.run_id
    ));
    let temp_sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".scope-{}.{}.tmp",
        std::process::id(),
        temp_sequence
    ));
    let mut temp = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)?;
    temp.write_all(&serde_json::to_vec(pointer)?)?;
    temp.sync_all()?;
    drop(temp);
    let publish = std::fs::hard_link(&temp_path, &final_path);
    let _ = std::fs::remove_file(&temp_path);
    publish?;
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn clip(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut out = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

fn sanitize(value: &str, max_chars: usize) -> String {
    clip(&redact_text(value), max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(root: &Path) -> AgentTaskLedger {
        AgentTaskLedger::open(root, "run-1").unwrap()
    }

    #[test]
    fn one_writer_but_parallel_readers() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue(
                "write-a",
                None,
                "backend",
                "build API",
                AgentTaskMode::Writer,
            )
            .unwrap();
        ledger
            .queue(
                "write-b",
                None,
                "frontend",
                "build UI",
                AgentTaskMode::Writer,
            )
            .unwrap();
        ledger
            .queue("review-a", None, "qa", "review", AgentTaskMode::ReadOnly)
            .unwrap();
        ledger
            .queue(
                "review-b",
                None,
                "security",
                "review",
                AgentTaskMode::ReadOnly,
            )
            .unwrap();

        ledger.start("write-a").unwrap();
        assert!(matches!(
            ledger.start("write-b"),
            Err(TaskLifecycleError::WriterBusy { .. })
        ));
        ledger.start("review-a").unwrap();
        ledger.start("review-b").unwrap();
    }

    #[test]
    fn parent_managed_child_inherits_the_live_parent_writer_slot() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue("parent", None, "builder", "build", AgentTaskMode::Writer)
            .unwrap();
        ledger
            .queue(
                "child",
                Some("parent".into()),
                "base-native-agent",
                "delegated work",
                AgentTaskMode::ParentManaged,
            )
            .unwrap();
        assert!(matches!(
            ledger.start("child"),
            Err(TaskLifecycleError::ParentNotActive(parent)) if parent == "parent"
        ));

        ledger.start("parent").unwrap();
        ledger.start("child").unwrap();
        assert!(matches!(
            ledger.succeed("parent", AgentTaskOutcome::success("early", vec![])),
            Err(TaskLifecycleError::UnfinishedManagedChild {
                task_id,
                child_task_id
            }) if task_id == "parent" && child_task_id == "child"
        ));
        ledger
            .succeed("child", AgentTaskOutcome::success("accepted", vec![]))
            .unwrap();
        ledger
            .succeed("parent", AgentTaskOutcome::success("accepted", vec![]))
            .unwrap();
        assert_eq!(ledger.readiness(), RunReadiness::Succeeded);
    }

    #[test]
    fn parent_managed_child_requires_a_writer_parent() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue(
                "review",
                None,
                "reviewer",
                "inspect",
                AgentTaskMode::ReadOnly,
            )
            .unwrap();

        assert!(matches!(
            ledger.queue(
                "orphan",
                None,
                "base-native-agent",
                "delegated work",
                AgentTaskMode::ParentManaged,
            ),
            Err(TaskLifecycleError::ParentManagedNeedsWriter { task_id })
                if task_id == "orphan"
        ));
        assert!(matches!(
            ledger.queue(
                "unsafe-child",
                Some("review".into()),
                "base-native-agent",
                "delegated work",
                AgentTaskMode::ParentManaged,
            ),
            Err(TaskLifecycleError::ParentManagedNeedsWriter { task_id })
                if task_id == "unsafe-child"
        ));
    }

    #[test]
    fn crash_recovery_interrupts_managed_child_before_writer_parent() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue("parent", None, "builder", "build", AgentTaskMode::Writer)
            .unwrap();
        ledger
            .queue(
                "child",
                Some("parent".into()),
                "base-native-agent",
                "delegated work",
                AgentTaskMode::ParentManaged,
            )
            .unwrap();
        ledger.start("parent").unwrap();
        ledger.start("child").unwrap();

        assert_eq!(ledger.recover_interrupted().unwrap(), 2);
        assert_eq!(
            ledger.task("child").unwrap().state,
            AgentTaskState::Interrupted
        );
        assert_eq!(
            ledger.task("parent").unwrap().state,
            AgentTaskState::Interrupted
        );
        let reopened = AgentTaskLedger::open(temp.path(), ledger.run_id()).unwrap();
        assert_eq!(
            reopened.task("child").unwrap().state,
            AgentTaskState::Interrupted
        );
        assert_eq!(
            reopened.task("parent").unwrap().state,
            AgentTaskState::Interrupted
        );
    }

    #[test]
    fn successful_run_requires_every_task_to_succeed() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue("do", None, "worker", "implement", AgentTaskMode::Writer)
            .unwrap();
        ledger
            .queue(
                "review",
                Some("do".into()),
                "qa",
                "verify",
                AgentTaskMode::ReadOnly,
            )
            .unwrap();
        assert_eq!(ledger.readiness(), RunReadiness::InProgress);
        ledger.start("do").unwrap();
        ledger
            .succeed(
                "do",
                AgentTaskOutcome::success("implemented", vec!["src/lib.rs".into()]),
            )
            .unwrap();
        ledger.start("review").unwrap();
        ledger
            .succeed("review", AgentTaskOutcome::success("verified", vec![]))
            .unwrap();
        assert_eq!(ledger.readiness(), RunReadiness::Succeeded);
    }

    #[test]
    fn dependencies_are_mechanical_start_conditions() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue("api", None, "backend", "build API", AgentTaskMode::Writer)
            .unwrap();
        ledger
            .queue_with_dependencies(
                "ui",
                None,
                vec!["api".into()],
                "frontend",
                "build UI",
                AgentTaskMode::Writer,
            )
            .unwrap();
        assert!(matches!(
            ledger.start("ui"),
            Err(TaskLifecycleError::DependenciesNotReady(_))
        ));
        ledger.start("api").unwrap();
        ledger
            .succeed("api", AgentTaskOutcome::success("done", vec![]))
            .unwrap();
        ledger.start("ui").unwrap();
    }

    #[test]
    fn unavailable_or_interrupted_never_looks_complete() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue("review", None, "qa", "verify", AgentTaskMode::ReadOnly)
            .unwrap();
        ledger.start("review").unwrap();
        assert_eq!(ledger.recover_interrupted().unwrap(), 1);
        assert!(matches!(ledger.readiness(), RunReadiness::Blocked(_)));
        ledger.resume("review").unwrap();
        ledger
            .unavailable(
                "review",
                AgentTaskOutcome::blocked("fork unavailable", vec!["no fork".into()]),
            )
            .unwrap();
        assert!(matches!(ledger.readiness(), RunReadiness::Blocked(_)));
    }

    #[test]
    fn journal_replays_parent_result_and_wait_resume() {
        let temp = tempfile::tempdir().unwrap();
        {
            let mut ledger = ledger(temp.path());
            ledger
                .queue(
                    "root",
                    None,
                    "director",
                    "coordinate",
                    AgentTaskMode::ReadOnly,
                )
                .unwrap();
            ledger
                .queue(
                    "child",
                    Some("root".into()),
                    "qa",
                    "inspect",
                    AgentTaskMode::ReadOnly,
                )
                .unwrap();
            ledger.start("child").unwrap();
            ledger.wait("child", "waiting for test output").unwrap();
            ledger.resume("child").unwrap();
            ledger
                .fail(
                    "child",
                    AgentTaskOutcome::blocked("tests failed", vec!["case 4".into()]),
                )
                .unwrap();
        }
        let reopened = ledger(temp.path());
        let child = reopened.task("child").unwrap();
        assert_eq!(child.parent_task_id.as_deref(), Some("root"));
        assert_eq!(child.state, AgentTaskState::Failed);
        assert_eq!(child.revision, 4);
        assert_eq!(child.outcome.as_ref().unwrap().blockers, ["case 4"]);
    }

    #[test]
    fn corrupt_journal_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".umadev/agent-tasks/run-1");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("00000000000000000001.json"), b"not-json").unwrap();
        assert!(matches!(
            AgentTaskLedger::open(temp.path(), "run-1"),
            Err(TaskLifecycleError::CorruptJournal(_))
        ));
    }

    #[test]
    fn ids_and_artifact_paths_are_confined() {
        let temp = tempfile::tempdir().unwrap();
        assert!(AgentTaskLedger::open(temp.path(), "../escape").is_err());
        let mut ledger = ledger(temp.path());
        ledger
            .queue("do", None, "worker", "implement", AgentTaskMode::Writer)
            .unwrap();
        ledger.start("do").unwrap();
        assert!(matches!(
            ledger.succeed(
                "do",
                AgentTaskOutcome::success("done", vec!["../secret".into()])
            ),
            Err(TaskLifecycleError::InvalidArtifact(_))
        ));
    }

    #[test]
    fn run_ids_are_opaque_and_unique() {
        let first = mint_agent_run_id("same");
        let second = mint_agent_run_id("same");
        assert!(first.starts_with("ar1-"));
        assert_ne!(first, second);
        validate_id("run id", &first).unwrap();
    }

    #[test]
    fn journal_never_persists_common_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = ledger(temp.path());
        ledger
            .queue(
                "safe-task",
                None,
                "worker",
                concat!("use api_key=sk-", "live-objective-secret"),
                AgentTaskMode::Writer,
            )
            .unwrap();
        ledger.start("safe-task").unwrap();
        ledger
            .fail(
                "safe-task",
                AgentTaskOutcome::blocked(
                    concat!("Authorization: Bearer ", "result-secret-value"),
                    vec![concat!("password=", "hunter2-secret").into()],
                ),
            )
            .unwrap();

        let journal = temp.path().join(".umadev/agent-tasks/run-1");
        let persisted = std::fs::read_dir(journal)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect::<String>();
        assert!(!persisted.contains(concat!("sk-", "live-objective-secret")));
        assert!(!persisted.contains("result-secret-value"));
        assert!(!persisted.contains("hunter2-secret"));
        assert!(persisted.contains("[redacted]"));
    }

    #[test]
    fn scoped_open_reuses_only_resumable_runs_without_persisting_scope() {
        let temp = tempfile::tempdir().unwrap();
        let scope = concat!("feature checkout api_key=sk-", "scope-secret-value");
        let first_id = {
            let mut first = AgentTaskLedger::open_scoped(temp.path(), scope).unwrap();
            first
                .queue("work", None, "worker", "build", AgentTaskMode::Writer)
                .unwrap();
            first.run_id().to_string()
        };
        let reopened = AgentTaskLedger::open_scoped(temp.path(), scope).unwrap();
        assert_eq!(reopened.run_id(), first_id);
        drop(reopened);

        let mut completed = AgentTaskLedger::open_scoped(temp.path(), scope).unwrap();
        completed.start("work").unwrap();
        completed
            .succeed("work", AgentTaskOutcome::success("done", vec![]))
            .unwrap();
        drop(completed);
        let next = AgentTaskLedger::open_scoped(temp.path(), scope).unwrap();
        assert_ne!(next.run_id(), first_id);

        let pointer_text = std::fs::read_dir(temp.path().join(".umadev/agent-tasks"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_file())
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect::<String>();
        assert!(!pointer_text.contains(concat!("sk-", "scope-secret-value")));
        assert!(!pointer_text.contains("feature checkout"));
    }

    #[test]
    fn recent_runs_exposes_bounded_read_only_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let mut ledger = AgentTaskLedger::open_scoped(temp.path(), "scope-a").unwrap();
        ledger
            .queue("work", None, "worker", "build", AgentTaskMode::Writer)
            .unwrap();
        let run_id = ledger.run_id().to_string();
        drop(ledger);

        let snapshots = recent_agent_runs(temp.path(), 1);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].run_id, run_id);
        assert_eq!(snapshots[0].tasks.len(), 1);
        assert_eq!(snapshots[0].readiness, RunReadiness::InProgress);
        assert!(recent_agent_runs(temp.path(), 0).is_empty());
    }

    #[test]
    fn entry_tracker_recovers_interruption_waits_and_mints_after_success() {
        let temp = tempfile::tempdir().unwrap();
        let first_run = {
            let tracker = EntryTaskTracker::begin(
                temp.path(),
                "quick\0checkout",
                "quick-edit",
                "apply and verify a scoped edit",
            )
            .unwrap();
            assert_eq!(tracker.state(), AgentTaskState::Running);
            tracker.run_id().to_string()
        };
        let mut resumed = EntryTaskTracker::begin(
            temp.path(),
            "quick\0checkout",
            "quick-edit",
            "apply and verify a scoped edit",
        )
        .unwrap();
        assert_eq!(resumed.run_id(), first_run);
        assert_eq!(resumed.state(), AgentTaskState::Running);
        resumed.wait("awaiting user confirmation").unwrap();
        drop(resumed);

        let mut after_gate = EntryTaskTracker::begin(
            temp.path(),
            "quick\0checkout",
            "quick-edit",
            "apply and verify a scoped edit",
        )
        .unwrap();
        assert_eq!(after_gate.run_id(), first_run);
        after_gate
            .succeed("verified", vec!["src/checkout.rs".into()])
            .unwrap();
        drop(after_gate);

        let next = EntryTaskTracker::begin(
            temp.path(),
            "quick\0checkout",
            "quick-edit",
            "apply and verify a scoped edit",
        )
        .unwrap();
        assert_ne!(next.run_id(), first_run);
    }

    #[test]
    fn entry_tracker_resumes_and_settles_the_exact_parked_run() {
        let temp = tempfile::tempdir().unwrap();
        let run_id = {
            let mut tracker = EntryTaskTracker::begin(
                temp.path(),
                "resident\0review-outage",
                "resident-edit",
                "apply a scoped edit",
            )
            .unwrap();
            let run_id = tracker.run_id().to_string();
            tracker.wait("required reviewer unavailable").unwrap();
            run_id
        };

        let mut resumed = EntryTaskTracker::resume_exact(temp.path(), &run_id).unwrap();
        assert_eq!(resumed.run_id(), run_id);
        assert_eq!(resumed.state(), AgentTaskState::Running);
        resumed.succeed("final review passed", Vec::new()).unwrap();
        drop(resumed);

        let ledger = AgentTaskLedger::open(temp.path(), &run_id).unwrap();
        assert_eq!(
            ledger.task(ENTRY_TASK_ID).map(|task| task.state),
            Some(AgentTaskState::Succeeded)
        );
        assert!(EntryTaskTracker::resume_exact(temp.path(), &run_id).is_err());
    }

    #[test]
    fn entry_tracker_cancels_the_exact_parked_run_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let run_id = {
            let mut tracker = EntryTaskTracker::begin(
                temp.path(),
                "resident\0cancelled-review",
                "resident-edit",
                "apply a scoped edit",
            )
            .unwrap();
            let run_id = tracker.run_id().to_string();
            tracker.wait("required reviewer unavailable").unwrap();
            run_id
        };

        EntryTaskTracker::cancel_exact(temp.path(), &run_id, "user cancelled").unwrap();
        EntryTaskTracker::cancel_exact(temp.path(), &run_id, "already cancelled").unwrap();
        let ledger = AgentTaskLedger::open(temp.path(), &run_id).unwrap();
        assert_eq!(
            ledger.task(ENTRY_TASK_ID).map(|task| task.state),
            Some(AgentTaskState::Cancelled)
        );
    }

    #[test]
    fn entry_tracker_redacts_objective_and_never_persists_scope_text() {
        let temp = tempfile::tempdir().unwrap();
        let mut tracker = EntryTaskTracker::begin(
            temp.path(),
            concat!("quick api_key=sk-", "scope-secret"),
            "quick-edit",
            concat!("update api_key=sk-", "objective-secret"),
        )
        .unwrap();
        let run_id = tracker.run_id().to_string();
        tracker
            .fail(
                "failed",
                vec![concat!("password=", "hunter2-secret").into()],
            )
            .unwrap();
        drop(tracker);

        let persisted = std::fs::read_dir(temp.path().join(".umadev/agent-tasks"))
            .unwrap()
            .filter_map(Result::ok)
            .flat_map(|entry| {
                if entry.path().is_dir() {
                    std::fs::read_dir(entry.path())
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                        .map(|nested| nested.path())
                        .collect::<Vec<_>>()
                } else {
                    vec![entry.path()]
                }
            })
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .collect::<String>();
        assert!(persisted.contains(&run_id));
        assert!(!persisted.contains(concat!("sk-", "scope-secret")));
        assert!(!persisted.contains(concat!("sk-", "objective-secret")));
        assert!(!persisted.contains("hunter2-secret"));
    }
}
