//! Background run registry and native process command routing.

use super::{
    agent_run_status_label, agent_task_status_label, fmt_elapsed, instant_from_age, task_summary,
    unix_now, Action, App, BackgroundTask, ChatRole, PersistedTask, PersistedTasks, TaskStatus,
    TASKS_CAP,
};

const MAX_BACKGROUND_PROCESS_ID_CHARS: usize = 512;
const MAX_PERSISTED_TASK_BYTES: u64 = 1024 * 1024;

fn valid_background_process_id(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && value.chars().count() <= MAX_BACKGROUND_PROCESS_ID_CHARS
}

impl App {
    /// `true` when a workspace-mutating run is live on any execution path.
    #[must_use]
    pub fn has_active_run(&self) -> bool {
        self.is_pipeline_active() || self.agentic_in_flight || self.active_task().is_some()
    }

    /// The live (`Running`) registry task, if any.
    #[must_use]
    pub fn active_task(&self) -> Option<&BackgroundTask> {
        self.tasks.iter().rev().find(|task| task.status.is_active())
    }

    fn active_task_mut(&mut self) -> Option<&mut BackgroundTask> {
        self.tasks
            .iter_mut()
            .rev()
            .find(|task| task.status.is_active())
    }

    /// Ensure that one live task represents the current single-writer run.
    pub fn register_run_task(&mut self, requirement: &str) {
        let summary = task_summary(requirement);
        if let Some(active) = self.active_task_mut() {
            if active.requirement.is_empty() && !summary.is_empty() {
                active.requirement = summary;
                self.persist_tasks();
            }
            return;
        }
        self.task_seq += 1;
        self.tasks.push(BackgroundTask {
            id: format!("t{}", self.task_seq),
            requirement: summary,
            status: TaskStatus::Running,
            started_at: std::time::Instant::now(),
            started_at_unix: unix_now(),
            done: 0,
            total: 0,
        });
        while self.tasks.len() > TASKS_CAP {
            if let Some(position) = self.tasks.iter().position(|task| !task.status.is_active()) {
                self.tasks.remove(position);
            } else {
                break;
            }
        }
        self.persist_tasks();
    }

    #[cfg(test)]
    pub(super) fn tasks_path(&self) -> std::path::PathBuf {
        self.project_root.join(".umadev").join("tasks.json")
    }

    fn persist_tasks(&self) {
        let Some(root) = self.project_filesystem.as_deref() else {
            return;
        };
        let start = self.tasks.len().saturating_sub(TASKS_CAP);
        let rows = self.tasks[start..]
            .iter()
            .map(|task| PersistedTask {
                id: task.id.clone(),
                requirement: task_summary(&task.requirement),
                status: task.status.persist_id().to_string(),
                started_at_unix: task.started_at_unix,
                done: task.done,
                total: task.total,
            })
            .collect();
        let snapshot = PersistedTasks {
            seq: self.task_seq,
            tasks: rows,
        };
        let Ok(body) = serde_json::to_string_pretty(&snapshot) else {
            return;
        };
        let _ = root.atomic_write(
            std::path::Path::new(".umadev/tasks.json"),
            body.as_bytes(),
            true,
        );
    }

    pub(super) fn load_tasks(&mut self) {
        let Some(root) = self.project_filesystem.as_deref() else {
            return;
        };
        let Ok(bytes) = root.read_bounded(
            std::path::Path::new(".umadev/tasks.json"),
            MAX_PERSISTED_TASK_BYTES,
        ) else {
            return;
        };
        let Ok(body) = String::from_utf8(bytes) else {
            return;
        };
        let Ok(snapshot) = serde_json::from_str::<PersistedTasks>(&body) else {
            return;
        };
        let mut max_sequence = self.task_seq.max(snapshot.seq);
        let mut restored = Vec::new();
        for persisted in snapshot.tasks.into_iter().take(TASKS_CAP) {
            let status = match TaskStatus::from_persist_id(&persisted.status) {
                Some(TaskStatus::Running) | None => TaskStatus::Stopped,
                Some(status) => status,
            };
            if let Some(sequence) = persisted
                .id
                .strip_prefix('t')
                .and_then(|value| value.parse::<u64>().ok())
            {
                max_sequence = max_sequence.max(sequence);
            }
            restored.push(BackgroundTask {
                id: persisted.id,
                requirement: task_summary(&persisted.requirement),
                status,
                started_at: instant_from_age(persisted.started_at_unix),
                started_at_unix: persisted.started_at_unix,
                done: persisted.done,
                total: persisted.total,
            });
        }
        if !restored.is_empty() {
            self.tasks = restored;
        }
        self.task_seq = max_sequence;
    }

    pub(super) fn sync_active_task_progress(&mut self) {
        let done = self
            .plan_steps
            .iter()
            .filter(|step| step.status == "done")
            .count();
        let total = self.plan_steps.len();
        if let Some(active) = self.active_task_mut() {
            active.done = done;
            active.total = total;
        }
    }

    pub(super) fn mark_active_task(&mut self, status: TaskStatus) {
        if let Some(active) = self.active_task_mut() {
            active.status = status;
            self.persist_tasks();
        }
    }

    fn render_tasks(&self) -> String {
        let agent_runs = umadev_agent::task_lifecycle::recent_agent_runs(&self.project_root, 3);
        if self.tasks.is_empty() && agent_runs.is_empty() {
            return umadev_i18n::t(self.lang, "tasks.empty").to_string();
        }
        let mut body = umadev_i18n::t(self.lang, "tasks.header").to_string();
        for task in self.tasks.iter().rev() {
            let label = umadev_i18n::t(self.lang, task.status.label_key());
            let progress = if task.total > 0 {
                format!(" · {}/{}", task.done, task.total)
            } else {
                String::new()
            };
            let elapsed = fmt_elapsed(task.started_at.elapsed().as_secs());
            let requirement = if task.requirement.is_empty() {
                umadev_i18n::t(self.lang, "tasks.untitled").to_string()
            } else {
                task.requirement.clone()
            };
            body.push_str(&format!(
                "\n  [{label}] {} · {requirement}{progress} · {elapsed}",
                task.id
            ));
        }
        for run in agent_runs {
            let run_label = agent_run_status_label(self.lang, &run.readiness);
            body.push_str(&format!(
                "\n\n{} {} [{run_label}]",
                umadev_i18n::t(self.lang, "tasks.agent.header"),
                run.run_id
            ));
            let shown = run.tasks.len().min(16);
            for task in run.tasks.iter().take(shown) {
                let label = agent_task_status_label(self.lang, task.state);
                let branch = if task.parent_task_id.is_some() {
                    "    |-"
                } else {
                    "  "
                };
                body.push_str(&format!(
                    "\n{branch} [{label}] {} · {}",
                    task.role,
                    task_summary(&task.objective)
                ));
                if let Some(blocker) = task
                    .outcome
                    .as_ref()
                    .and_then(|outcome| outcome.blockers.first())
                {
                    body.push_str(&format!("\n       ! {}", task_summary(blocker)));
                }
            }
            if run.tasks.len() > shown {
                body.push_str(&format!(
                    "\n    ... +{} {}",
                    run.tasks.len() - shown,
                    umadev_i18n::t(self.lang, "tasks.agent.more")
                ));
            }
        }
        body.push('\n');
        body.push_str(umadev_i18n::t(self.lang, "tasks.actions_hint"));
        body
    }

    /// `/tasks [stop|resume]` controls Director and agent-team runs.
    pub(super) fn slash_tasks(&mut self, argument: &str) -> Action {
        let subcommand = argument
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match subcommand.as_str() {
            "" => {
                self.push(ChatRole::System, self.render_tasks());
                Action::None
            }
            "stop" | "cancel" if self.has_interruptible_work() => Action::Cancel,
            "stop" | "cancel" => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "tasks.none_active"),
                );
                Action::None
            }
            "resume" | "continue" if self.has_active_run() => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "tasks.already_running"),
                );
                Action::None
            }
            "resume" | "continue"
                if !self.finished && umadev_agent::has_resumable_run(&self.project_root) =>
            {
                if self.reject_director_execution_in_plan() {
                    return Action::None;
                }
                let requirement = self.resume_run_requirement();
                if self.reject_replayed_host_git_operation(&requirement) {
                    return Action::None;
                }
                self.push_resume_separator();
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, "continue.resuming"),
                );
                Action::ResumeRun(requirement)
            }
            "resume" | "continue" => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "tasks.nothing_to_resume"),
                );
                Action::None
            }
            _ => {
                self.push(ChatRole::System, umadev_i18n::t(self.lang, "tasks.usage"));
                Action::None
            }
        }
    }

    /// `/processes [stop <id>]` controls processes owned by the current base session.
    pub(super) fn slash_processes(&mut self, argument: &str) -> Action {
        let mut parts = argument.split_whitespace();
        match parts.next().map(str::to_ascii_lowercase).as_deref() {
            None => Action::ListBackgroundProcesses,
            Some("list") => {
                if parts.next().is_none() {
                    Action::ListBackgroundProcesses
                } else {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "processes.usage"),
                    );
                    Action::None
                }
            }
            Some("stop" | "kill") => {
                let Some(task_id) = parts.next() else {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "processes.usage"),
                    );
                    return Action::None;
                };
                if parts.next().is_some() || !valid_background_process_id(task_id) {
                    self.push(
                        ChatRole::System,
                        umadev_i18n::t(self.lang, "processes.invalid_id"),
                    );
                    Action::None
                } else {
                    Action::StopBackgroundProcess(task_id.to_string())
                }
            }
            Some(_) => {
                self.push(
                    ChatRole::System,
                    umadev_i18n::t(self.lang, "processes.usage"),
                );
                Action::None
            }
        }
    }
}
