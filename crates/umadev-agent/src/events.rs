//! Engine event stream — the channel the orchestrator talks to a UI on.
//!
//! The CLI does not need events (it prints a final report), so it
//! passes a [`NullSink`]. The TUI (M3) passes a [`ChannelSink`] and
//! renders frames as events arrive. Tests use [`RecordingSink`] to
//! assert the exact event sequence a run produces.
//!
//! Events are *observational*: emitting one never changes pipeline
//! behavior, and a sink that drops every event is always valid.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use umadev_spec::Phase;

use crate::gates::Gate;

/// One thing the engine did, surfaced to whatever UI is watching.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum EngineEvent {
    /// The pipeline run has begun.
    PipelineStarted {
        /// Project slug.
        slug: String,
        /// The free-form requirement driving the run.
        requirement: String,
    },
    /// A phase has started executing.
    PhaseStarted {
        /// The phase that just began.
        phase: Phase,
    },
    /// A phase wrote an artifact to disk.
    ArtifactWritten {
        /// The phase that produced it.
        phase: Phase,
        /// Absolute path of the written file.
        path: PathBuf,
    },
    /// A phase finished.
    PhaseCompleted {
        /// The phase that finished.
        phase: Phase,
    },
    /// The pipeline paused at a confirmation gate awaiting user input.
    GateOpened {
        /// Which gate is now open.
        gate: Gate,
    },
    /// A block of execution finished — either paused at a gate, or the
    /// whole pipeline completed when `paused_at` is `None`.
    BlockCompleted {
        /// The phase the engine settled on.
        final_phase: Phase,
        /// The gate it paused at, or `None` when delivery completed.
        paused_at: Option<Gate>,
    },
    /// A host backend was probed for availability (TUI startup).
    BackendProbed {
        /// Stable backend id (`claude-code` / `codex` / `opencode`).
        backend_id: String,
        /// `true` when the host CLI is installed and reachable.
        ready: bool,
        /// Human-readable detail (version string or failure reason).
        detail: String,
    },
    /// Verify is starting after a code-producing phase. The runner
    /// inspected the workspace and is about to run the build command.
    VerifyStarted {
        /// The phase whose output is being verified.
        phase: Phase,
        /// Human-readable command string (e.g. `cargo check --quiet`).
        command: String,
    },
    /// Verify ran but the workspace had no recognised project manifest
    /// (no `package.json` / `Cargo.toml` / `pyproject.toml`).
    VerifySkipped {
        /// The phase that was being verified.
        phase: Phase,
        /// Why it was skipped (always "no recognised manifest" today).
        reason: String,
    },
    /// Verify completed successfully — the build / install command
    /// returned exit code 0.
    VerifyPassed {
        /// The phase that was verified.
        phase: Phase,
        /// Wall-clock duration of the command, milliseconds.
        duration_ms: u64,
    },
    /// Verify ran and the build / install command failed. The runner
    /// will fall back to the next attempt or surface the failure to
    /// the user (depending on policy).
    VerifyFailed {
        /// The phase that was verified.
        phase: Phase,
        /// Exit code from the build command.
        exit_code: i32,
        /// Truncated stderr (≤ 8 KiB) for the UI to show.
        stderr: String,
    },
    /// One chunk of output produced by the host CLI (Claude Code /
    /// Codex). Emitted per non-empty line so a UI can render the host's
    /// response as it scrolls past, instead of dumping the whole thing
    /// at the end of the phase.
    HostOutput {
        /// The phase that produced this chunk.
        phase: Phase,
        /// One line of host stdout (already ANSI-stripped).
        line: String,
    },
    /// A human-readable progress note (free-form).
    Note(String),
    /// A **transient, in-place status line** — NOT a transcript entry. Used by
    /// the long-phase heartbeat for its periodic "still working (mm:ss)" beats:
    /// a UI overwrites a single status field with `Some(text)` and clears it on
    /// `None`, so a multi-second wait shows ONE live-updating line in the status
    /// bar instead of stacking a new transcript row every few seconds. Purely
    /// cosmetic and fully fail-open — a sink that drops it loses nothing but the
    /// in-place reassurance (the rotating spinner + phase timer already prove
    /// motion). The CLI/headless paths ignore it entirely.
    TransientStatus(Option<String>),
    /// A sub-task within a phase has started. Emitted when a phase fans out
    /// into parallel work (e.g. backend implementation running concurrently
    /// with a source-scan quality check). `task_id` groups start/completed pairs.
    SubTaskStarted {
        /// The parent phase.
        phase: Phase,
        /// Stable sub-task identifier (e.g. `backend.implement`).
        task_id: String,
        /// Human-readable label shown in the UI.
        label: String,
    },
    /// A sub-task finished. `ok` is false when the sub-task failed but the
    /// phase continues (the phase-level failure surfaces via the normal
    /// PhaseCompleted / quality-gate path).
    SubTaskCompleted {
        /// The parent phase.
        phase: Phase,
        /// Matches the `task_id` from the corresponding SubTaskStarted.
        task_id: String,
        /// Whether the sub-task succeeded.
        ok: bool,
    },
    /// **Real-time streaming event** from the worker (Claude Code / Codex / OpenCode).
    /// Emitted for each JSONL line parsed from `--output-format stream-json`
    /// (claude) or `--json` (codex). The TUI shows these live so the user
    /// sees the worker's tool calls and text deltas as they happen — instead
    /// of staring at a spinner for 3 minutes.
    WorkerStream {
        /// What kind of stream event this is.
        event: umadev_runtime::StreamEvent,
    },
    /// **The router decided how to handle this turn** (Wave 1, L1). Emitted once,
    /// before any work, so a UI can render an "intent card" — what UmaDev decided
    /// (chat vs build), how deep, who it'll convene, the rough budget, and a
    /// one-line reason. Pure summary strings so the event stays cheap + `Eq` (the
    /// full typed `RoutePlan` lives in `router`).
    IntentDecided {
        /// Route class id (`chat` / `explain` / `quick_edit` / `debug` / `build`).
        class: String,
        /// Depth id (`fast` / `standard` / `deep`).
        depth: String,
        /// The seats UmaDev will convene (role ids), in order. Empty for a fast turn.
        team: Vec<String>,
        /// Rough expected tool-call ceiling for this turn.
        est_tool_calls: u32,
        /// A one-line human rationale (`RoutePlan::rationale`).
        rationale: String,
    },
    /// **A plan was synthesised and is now owned by UmaDev** (Wave 1, L2). Emitted
    /// after the planning turn so a UI can render the live checklist that replaces
    /// the frozen phase bar. Each entry is a compact `id · title (seat)` summary.
    PlanPosted {
        /// One summary line per step (`Plan::step_summaries`).
        steps: Vec<String>,
        /// `done / total` at post time (always `0 / N` for a fresh plan).
        done: usize,
        /// Total step count.
        total: usize,
    },
    /// **A plan step changed status** (Wave 1, L2) — drives the checklist ticking
    /// off live (`[x] scaffold · [~] auth route · [ ] login form`).
    PlanStepStatus {
        /// The step id (matches a `PlanPosted` summary's leading id).
        id: String,
        /// Human-readable step title.
        title: String,
        /// New status id (`pending` / `active` / `done` / `blocked`).
        status: String,
    },
    /// **A reviewing seat returned a structured verdict** (Wave 1, L1/L2). Replaces
    /// the bland team `Note` so a UI can render a collapsible team-review panel with
    /// each seat's accept/blocking/advisory. Advisory only — never drives the loop.
    CriticVerdict {
        /// The reviewing seat's role id (e.g. `architect`).
        seat: String,
        /// Whether the seat accepts the artifacts as-is.
        accepts: bool,
        /// Must-fix findings the seat raised (may be empty).
        blocking: Vec<String>,
        /// Nice-to-have notes (may be empty).
        advisory: Vec<String>,
    },
}

impl EngineEvent {
    /// Build an [`EngineEvent::IntentDecided`] from a router [`crate::router::RoutePlan`]
    /// — flattens the typed plan into the cheap summary the UI renders, so the
    /// conversion lives in ONE place (the director just calls this).
    #[must_use]
    pub fn intent_decided(route: &crate::router::RoutePlan) -> Self {
        Self::IntentDecided {
            class: route.class.as_str().to_string(),
            depth: route.depth.as_str().to_string(),
            team: route.team.iter().map(|s| s.role_id().to_string()).collect(),
            est_tool_calls: route.est_budget.max_tool_calls,
            rationale: route.rationale(),
        }
    }

    /// Build an [`EngineEvent::PlanPosted`] from an owned [`crate::plan_state::Plan`].
    #[must_use]
    pub fn plan_posted(plan: &crate::plan_state::Plan) -> Self {
        let (done, total) = plan.progress();
        Self::PlanPosted {
            steps: plan.step_summaries(),
            done,
            total,
        }
    }

    /// Build an [`EngineEvent::PlanStepStatus`] for one step transition.
    #[must_use]
    pub fn plan_step_status(
        id: impl Into<String>,
        title: impl Into<String>,
        status: crate::plan_state::StepStatus,
    ) -> Self {
        Self::PlanStepStatus {
            id: id.into(),
            title: title.into(),
            status: status.as_str().to_string(),
        }
    }

    /// Build an [`EngineEvent::CriticVerdict`] from a seat's [`crate::critics::RoleVerdict`].
    #[must_use]
    pub fn critic_verdict(verdict: &crate::critics::RoleVerdict) -> Self {
        Self::CriticVerdict {
            seat: verdict.role.clone(),
            accepts: verdict.accepts,
            blocking: verdict.blocking.clone(),
            advisory: verdict.advisory.clone(),
        }
    }
}

/// Anything that consumes [`EngineEvent`]s. Implementations must be
/// cheap to call and never block the engine.
pub trait EventSink: Send + Sync {
    /// Receive one event. Must not panic.
    fn emit(&self, event: EngineEvent);
}

/// Drops every event. The default for headless / CLI runs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: EngineEvent) {}
}

/// Forwards events into an async channel — the TUI's input.
#[derive(Debug, Clone)]
pub struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<EngineEvent>,
}

impl ChannelSink {
    /// Build a sink plus the receiver the UI loop should poll.
    #[must_use]
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<EngineEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

impl EventSink for ChannelSink {
    fn emit(&self, event: EngineEvent) {
        // A closed receiver just means the UI went away; drop silently.
        let _ = self.tx.send(event);
    }
}

/// Captures every event in memory — for tests.
#[derive(Debug, Default, Clone)]
pub struct RecordingSink {
    events: Arc<Mutex<Vec<EngineEvent>>>,
}

impl RecordingSink {
    /// Build an empty recording sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot every event captured so far.
    #[must_use]
    pub fn events(&self) -> Vec<EngineEvent> {
        // The EventSink contract (see trait docs) is "never panic". A
        // poisoned mutex means some other thread panicked while holding
        // the lock — we still want to recover the buffer rather than
        // propagate the panic, so ignore the poison guard.
        match self.events.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Count events matching a predicate.
    #[must_use]
    pub fn count(&self, pred: impl Fn(&EngineEvent) -> bool) -> usize {
        self.events().iter().filter(|e| pred(e)).count()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: EngineEvent) {
        // Recover from a poisoned mutex instead of panicking — the sink
        // must never break the pipeline (see trait "never panic" contract).
        let mut g = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.push(event);
    }
}

/// Convenience: a no-op sink behind an `Arc` for the default runner.
#[must_use]
pub fn null_sink() -> Arc<dyn EventSink> {
    Arc::new(NullSink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_sink_drops_everything() {
        let sink = NullSink;
        sink.emit(EngineEvent::Note("ignored".into()));
        // nothing to assert — just must not panic
    }

    #[test]
    fn recording_sink_captures_in_order() {
        let sink = RecordingSink::new();
        sink.emit(EngineEvent::PhaseStarted {
            phase: Phase::Research,
        });
        sink.emit(EngineEvent::PhaseCompleted {
            phase: Phase::Research,
        });
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], EngineEvent::PhaseStarted { .. }));
        assert!(matches!(events[1], EngineEvent::PhaseCompleted { .. }));
    }

    #[test]
    fn recording_sink_count_filters() {
        let sink = RecordingSink::new();
        sink.emit(EngineEvent::ArtifactWritten {
            phase: Phase::Docs,
            path: "a.md".into(),
        });
        sink.emit(EngineEvent::ArtifactWritten {
            phase: Phase::Docs,
            path: "b.md".into(),
        });
        sink.emit(EngineEvent::Note("x".into()));
        assert_eq!(
            sink.count(|e| matches!(e, EngineEvent::ArtifactWritten { .. })),
            2
        );
    }

    #[tokio::test]
    async fn channel_sink_delivers_to_receiver() {
        let (sink, mut rx) = ChannelSink::new();
        sink.emit(EngineEvent::Note("hello".into()));
        let got = rx.recv().await.unwrap();
        assert_eq!(got, EngineEvent::Note("hello".into()));
    }

    #[tokio::test]
    async fn channel_sink_tolerates_dropped_receiver() {
        let (sink, rx) = ChannelSink::new();
        drop(rx);
        // Must not panic even though nobody is listening.
        sink.emit(EngineEvent::Note("nobody home".into()));
    }

    // ── Wave 1 visible-event constructors flatten the typed primitives ──

    #[test]
    fn intent_decided_flattens_route_plan() {
        let route = crate::router::RoutePlan {
            class: crate::router::RouteClass::Build,
            kind: crate::planner::TaskKind::Greenfield,
            depth: crate::router::Depth::Deep,
            team: vec![
                crate::critics::Seat::Architect,
                crate::critics::Seat::BackendEngineer,
            ],
            scope: vec![],
            needs_clarify: None,
            est_budget: crate::router::Budget::for_route(
                crate::router::RouteClass::Build,
                crate::router::Depth::Deep,
            ),
            confidence: 0.9,
        };
        let ev = EngineEvent::intent_decided(&route);
        let EngineEvent::IntentDecided {
            class,
            depth,
            team,
            est_tool_calls,
            rationale,
        } = ev
        else {
            panic!("wrong variant");
        };
        assert_eq!(class, "build");
        assert_eq!(depth, "deep");
        assert_eq!(team, vec!["architect", "backend-engineer"]);
        assert!(est_tool_calls > 0);
        assert!(!rationale.is_empty());
    }

    #[test]
    fn plan_posted_and_step_status_carry_summaries() {
        let plan = crate::plan_state::Plan {
            steps: vec![crate::plan_state::PlanStep {
                id: "scaffold".to_string(),
                title: "Scaffold the app".to_string(),
                seat: crate::critics::Seat::FrontendEngineer,
                kind: crate::plan_state::StepKind::Build,
                depends_on: vec![],
                acceptance: crate::plan_state::AcceptanceSpec::SourcePresent,
                status: crate::plan_state::StepStatus::Pending,
            }],
            risks: vec![],
            open_questions: vec![],
        };
        let posted = EngineEvent::plan_posted(&plan);
        let EngineEvent::PlanPosted { steps, done, total } = posted else {
            panic!("wrong variant");
        };
        assert_eq!((done, total), (0, 1));
        assert_eq!(steps.len(), 1);
        assert!(steps[0].starts_with("scaffold"));

        let st = EngineEvent::plan_step_status(
            "scaffold",
            "Scaffold the app",
            crate::plan_state::StepStatus::Active,
        );
        assert_eq!(
            st,
            EngineEvent::PlanStepStatus {
                id: "scaffold".into(),
                title: "Scaffold the app".into(),
                status: "active".into(),
            }
        );
    }

    #[test]
    fn critic_verdict_carries_seat_findings() {
        let v = crate::critics::RoleVerdict {
            role: "architect".to_string(),
            accepts: false,
            blocking: vec!["missing API table".to_string()],
            advisory: vec!["consider rate limiting".to_string()],
            evidence: vec!["architecture.md".to_string()],
        };
        let ev = EngineEvent::critic_verdict(&v);
        let EngineEvent::CriticVerdict {
            seat,
            accepts,
            blocking,
            advisory,
        } = ev
        else {
            panic!("wrong variant");
        };
        assert_eq!(seat, "architect");
        assert!(!accepts);
        assert_eq!(blocking, vec!["missing API table".to_string()]);
        assert_eq!(advisory.len(), 1);
    }
}
