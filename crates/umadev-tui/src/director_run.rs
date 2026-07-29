use std::sync::Arc;

use umadev_agent::{
    task_lifecycle::EntryTaskTracker, ChannelSink, EngineEvent, EventSink,
    PostBuildOperationalPause, RoutePlan, RunOptions,
};
use umadev_runtime::{BaseSession, Message};

use crate::interaction_bridge::{
    await_host_input, await_user_approval, ApprovalHolder, ApprovalReply, HostInputHolder,
};
use crate::session_slot::{PermissionedSession, SessionHolder, SessionIdentity};
use crate::{
    block_abort_note, cold_judge_surface, detach_session_close, director_directive_with_history,
    director_source_hardgate, open_director_session, resolve_goal_mode, start_failed_note,
    RouteDecision, ABORT_SENTINEL,
};

/// Settle ownership of a Director base after one drive.
///
/// A Guarded operational-review pause is resumable. Keep that live process only
/// when its complete immutable launch identity is known; `/continue` then consumes
/// it through the same exact-match check used by the legacy continuous path.
/// Auto mode settles an exhausted reviewer retry as `Failed`, so it follows the
/// terminal path and closes the process instead of advertising a dead resume.
pub(super) async fn settle_director_session(
    mut session: Box<dyn umadev_runtime::BaseSession>,
    holder: &SessionHolder,
    identity: Option<SessionIdentity>,
    operational_pause: bool,
) {
    if operational_pause {
        if let Some(identity) = identity {
            let displaced = holder
                .lock()
                .await
                .replace(PermissionedSession::new(session, identity));
            if let Some(displaced) = displaced {
                detach_session_close(displaced.into_inner());
            }
            return;
        }
    }
    let _ = session.end().await;
}

pub(super) fn fail_entry_task(
    task: &mut Option<EntryTaskTracker>,
    summary: &str,
    blocker: impl Into<String>,
) {
    if let Some(task) = task.as_mut() {
        let _ = task.fail(summary, vec![blocker.into()]);
    }
}

pub(super) fn cancel_entry_task(task: &mut Option<EntryTaskTracker>, detail: &str) {
    if let Some(task) = task.as_mut() {
        let _ = task.cancel(detail);
    }
}

/// Close an Auto-mode resident review outage as a terminal failure. Guarded
/// pauses return the untouched session so the caller can park it for `/continue`.
pub(super) async fn settle_terminal_post_build_review(
    pause: &PostBuildOperationalPause,
    entry_task: &mut Option<EntryTaskTracker>,
    sink: &dyn EventSink,
    session: Box<dyn BaseSession>,
    holder: &SessionHolder,
    identity: Option<SessionIdentity>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
) -> Result<Box<dyn BaseSession>, ()> {
    if !pause.terminal {
        return Ok(session);
    }
    let reason = format!(
        "{}; automatic mode exhausted its bounded reviewer retry and stopped this run incomplete. Start a new /run when the reviewer service is available",
        pause.reason
    );
    fail_entry_task(entry_task, "resident review unavailable", reason.clone());
    sink.emit(EngineEvent::Note(format!(
        "team · {reason} — no source repair was started"
    )));
    settle_director_session(session, holder, identity, false).await;
    let _ = route_tx.send(RouteDecision::Failed(reason));
    Err(())
}

/// The director build loop body — the non-spawning core of
/// [`crate::spawn_director_loop`].
///
/// Split out so the brain-routed chat dispatcher ([`crate::run_routed_turn`]) can drive
/// the director build INLINE from inside its OWN already-spawned classification
/// task (a chat message classified `Build` must reuse this exact path — run-lock,
/// branch isolation, firmware, the routed plan/step/finalize loop, source hard-gate
/// — not a second copy). The `/run` entry + the queued-chat drain keep calling the
/// spawning wrapper. The body is byte-for-byte the original; only the outer
/// `tokio::spawn(async move { … })` moved up into the wrapper.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_director_loop(
    options: RunOptions,
    sink: Arc<ChannelSink>,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    session_holder: SessionHolder,
    permissions: umadev_runtime::BasePermissionProfile,
    conversation: Vec<Message>,
    route_override: Option<RoutePlan>,
    goal_mode: bool,
    resume: bool,
    // A2#3/#4: the hosting UI's live hooks — the shared mid-run steering intake
    // and the y/n approval pause holder. Scoped into the agent's task-local
    // `RunInteraction` around the drive below, so the director loop can pause at
    // the spec-MUST gates, ask the live user to approve an escalated action, and
    // fold queued steering into the next step — all fail-open (a CLI drive that
    // never scopes them keeps headless behaviour byte-for-byte).
    steer: umadev_agent::SteerIntake,
    approval: ApprovalHolder,
    host_input: HostInputHolder,
    // A natural-language turn is classified only after its resident writer has
    // been acquired. Reuse that already-open, correctly permissioned session for
    // the director drive; explicit `/run` passes `None` and opens/resumes normally.
    resident_session: Option<Box<dyn umadev_runtime::BaseSession>>,
) {
    // A requirement recovered from a prior run is context, not fresh authority
    // to perform another Git commit. Normal fresh commit text is consumed by
    // the host transaction before a Director action exists; reaching this
    // boundary therefore means a programmatic or durable-state replay. Refuse
    // before lock/isolation/state/session/team work.
    if umadev_agent::request_has_git_commit_operation(&options.requirement) {
        let note = umadev_i18n::tl("intent.git_commit_host_boundary").to_string();
        sink.emit(EngineEvent::Note(note.clone()));
        let _ = route_tx.send(RouteDecision::Failed(note));
        return;
    }
    // Defensive no-write ceiling. Normal explicit entries reject Plan mode on
    // the UI thread, but this boundary also protects programmatic/direct callers
    // before they acquire a run lock, create a branch, persist workflow state, or
    // open a writable host session.
    if !options.mode.executes() {
        sink.emit(EngineEvent::Note(
            umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        ));
        sink.emit(EngineEvent::Note(
            umadev_i18n::tl("mode.plan.gate").to_string(),
        ));
        let _ = route_tx.send(RouteDecision::RunNotExecuted);
        return;
    }

    {
        let backend = options.backend.clone();
        let model = options.model.clone();
        let root = options.project_root.clone();

        // Single-writer run-lock for the whole director loop — the SAME guard the
        // CLI `drive_director_run` + the legacy pipeline hold, so a director build
        // serializes with any other workspace-mutating run. A lock held by a
        // DIFFERENT live run is an honest terminal abort; any other lock IO fails
        // open inside `acquire_for_run` to an un-owned guard (a lock bug never
        // blocks a legitimate build). The guard lives for the task's scope.
        let _run_lock = match umadev_agent::run_lock::RunLock::acquire_for_run(&root) {
            Ok(g) => g,
            Err(e) => {
                sink.emit(EngineEvent::Note(format!(
                    "{ABORT_SENTINEL}{}",
                    block_abort_note(&e, &backend)
                )));
                let _ = route_tx.send(RouteDecision::Failed(start_failed_note(&e)));
                return;
            }
        };

        // A persisted terminal review circuit is a caller-level failure receipt,
        // not a resumable session. Settle it before isolation, workflow writes,
        // firmware work, or any base login/ACP handshake; the inner resume guard
        // remains as defense against stale direct callers and fallback.
        if resume {
            if let Some(reason) = umadev_agent::terminal_review_circuit_reason(&root) {
                sink.emit(EngineEvent::Note(format!("{ABORT_SENTINEL}{reason}")));
                let _ = route_tx.send(RouteDecision::Failed(reason));
                return;
            }
        }

        // A fresh run supersedes an operational-review pause, but settlement
        // must use the OLD workflow identity. Do this before writing the new
        // requirement/backend baseline; otherwise the old plan is reopened
        // under a fabricated new ledger and its real Waiting task is orphaned.
        if !resume && options.mode.executes() {
            if let Err(error) = umadev_agent::cancel_operational_review_pause(
                &root,
                "superseded by an explicitly fresh run",
            ) {
                let reason = format!("parked operational review could not be superseded: {error}");
                sink.emit(EngineEvent::Note(format!("{ABORT_SENTINEL}{reason}")));
                let _ = route_tx.send(RouteDecision::Failed(reason));
                return;
            }
        }

        // Isolate the build and snapshot its baseline before writes. Fail open when
        // isolation is unavailable; never auto-merge or push the resulting branch.
        let isolation = if resume {
            umadev_agent::setup_run_isolation(&root, &options.effective_slug())
        } else {
            umadev_agent::setup_new_run_isolation(&root, &options.effective_slug())
        };
        if let Some((branch, from)) = isolation {
            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.branch_isolated",
                &[&branch, &from],
            )));
        }

        // MEDIUM #7: write the WorkflowState baseline, exactly like the CLI's
        // `AgentRunner::start` does (`.umadev/workflow-state.json`, phase `research`,
        // slug + requirement + backend). Without this a TUI-originated director build
        // left no state on disk, so `umadev status` / `umadev continue` against a
        // build STARTED in the chat TUI read `Missing` and bailed — the run was
        // invisible to the CLI surfaces. Written here (after the run-lock + isolation,
        // before the base writes anything) so the baseline reflects this run. Fail-open
        // by contract: a disk/permission error is swallowed (`let _ =`) — a state-write
        // bug must NEVER block an otherwise-healthy build.
        // P0 (full-context resume): a vendor session id is owned by the exact base
        // that persisted it. A `/continue` may carry it only when that owner matches
        // the currently selected base byte-for-byte. Retired/unknown workflows and
        // explicit formal→formal switches keep the requirement, plan, and artifacts,
        // but start a fresh vendor session; an id must never cross that boundary.
        let persisted_state = resume
            .then(|| umadev_agent::read_workflow_state(&root))
            .flatten();
        let resume_identity = resolve_workflow_resume_identity(
            resume,
            persisted_state.as_ref(),
            backend.as_str(),
            &root,
            permissions,
        );
        let prior_base_session_id = resume_identity.base_session_id.clone();
        if let Some(previous_backend) = resume_identity.handoff_from {
            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                "backend.workflow_handoff",
                &[&previous_backend, &backend],
            )));
        }
        let mut baseline = {
            // `WorkflowState::new` fills `last_transition_at` (now) + `spec_version`;
            // override the run-specific carry-through fields the CLI's `start` sets.
            let mut s = umadev_agent::WorkflowState::new(umadev_spec::Phase::Research);
            s.slug = options.effective_slug();
            s.requirement.clone_from(&options.requirement);
            s.backend.clone_from(&backend);
            s.note = format!("Started director build (TUI) with {backend}");
            // Preserve the prior resume pointer across the baseline write so the
            // resume id survives (the LIVE id is re-persisted right after the session
            // opens; a failed owned resume aborts instead of changing conversations).
            s.base_session_id = prior_base_session_id.clone();
            s.base_resume_identity = resume_identity.base_resume_identity.clone();
            s.permission_profile = Some(options.mode.base_permissions());
            s
        };
        let _ = umadev_agent::write_workflow_state(&root, &baseline);

        // Wave 2 (firmware): compose UmaDev's identity + craft + JIT knowledge +
        // pitfall memory once (the `/run` route is deterministic, no session needed)
        // so claude can take it NATIVELY as a system prompt via `session_for`'s
        // `--append-system-prompt`. Fail-open: an empty firmware just leaves the base
        // un-primed beyond the directive, exactly as before.
        //
        // Route source: an explicit `/run` passes `None` → `for_run` FORCES a Build
        // (a bare goal still builds). A natural-language build passes the healthy
        // model verdict already produced on the read-only intent child, so Director
        // drives the exact class/kind/depth/team the selected brain chose. The
        // deterministic availability fallback never reaches this entry.
        let route =
            route_override.unwrap_or_else(|| umadev_agent::router::for_run(&options.requirement));
        let firmware = umadev_agent::compose_firmware(&root, &route, &options.requirement).await;
        let firmware = (!firmware.trim().is_empty()).then_some(firmware);

        // Reuse the resident writer for a model-routed natural-language build. It
        // already carries the selected base/model, permission profile and native
        // dialogue, avoiding a third process after the read-only intent fork. An
        // explicit `/run` or `/continue` has no resident writer here and opens or
        // resumes through the normal director path.
        let reused_resident = resident_session.is_some();
        let mut session = if let Some(session) = resident_session {
            session
        } else {
            let requested_identity = SessionIdentity::for_launch(&backend, &root, permissions);
            let parked = session_holder.lock().await.take();
            let parked = match (resume, requested_identity.as_ref(), parked) {
                (true, Some(requested), Some(parked)) => match parked.into_matching(requested) {
                    Ok(session) => Some(session),
                    Err(stale) => {
                        detach_session_close(stale);
                        None
                    }
                },
                (_, _, Some(stale)) => {
                    // A fresh run, a moved workspace, a backend switch, or a
                    // permission change may never inherit the prior paused brain.
                    detach_session_close(stale.into_inner());
                    None
                }
                _ => None,
            };
            if let Some(session) = parked {
                session
            } else {
                match open_director_session(
                    &backend,
                    &root,
                    &model,
                    permissions,
                    firmware.as_deref(),
                    prior_base_session_id.as_deref(),
                )
                .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        sink.emit(EngineEvent::Note(format!(
                            "{ABORT_SENTINEL}{}",
                            umadev_i18n::tlf(
                                "continuous.tui_session_unavailable",
                                &[&e.to_string()],
                            )
                        )));
                        let _ = route_tx.send(RouteDecision::Failed(umadev_i18n::tlf(
                            "continuous.tui_session_unavailable",
                            &[&e.to_string()],
                        )));
                        return;
                    }
                }
            }
        };

        // P0 (full-context resume): persist the LIVE base session id so a later
        // `/continue` can resume THIS conversation. On a successful claude/codex
        // resume the id is unchanged (idempotent); when no eligible prior id exists,
        // a genuinely fresh open captures the NEW conversation's id. A same-base
        // resume failure never reaches this point: it is surfaced instead of silently
        // changing brains. Fail-open: a base with no resumable id or a write error
        // just leaves the baseline as-is.
        if let Some(id) = session.session_id() {
            let id = id.to_string();
            if !id.trim().is_empty() {
                baseline.base_session_id = Some(id);
                baseline.base_resume_identity = session.resume_identity().cloned().or_else(|| {
                    crate::session_slot::requested_resume_identity(&backend, &root, permissions)
                });
                let _ = umadev_agent::write_workflow_state(&root, &baseline);
            }
        }

        // Frame the goal for the director (the firmware framing), then drive the
        // build loop: the base builds end to end, UmaDev runs its honesty/QC read.
        // A newly-opened Claude director already took the firmware natively as its
        // system prompt. A reused resident was pre-warmed with identity only, so it
        // receives the full route-sized firmware in-band like every non-Claude base.
        // Fail-open: no firmware leaves the goal unchanged.
        let goal = umadev_agent::experts::director_build_directive(&options.requirement);
        // Chat-originated build (Blocker #2): front-load UmaDev's OWN bounded
        // conversation transcript so the director's brain inherits the prior dialogue
        // — the SAME Wave 5 / G11 memory `drive_agentic_stream` threads for a light
        // chat turn, so a build promoted out of a conversation keeps that context
        // instead of starting cold. Empty for an explicit `/run` (no prior chat) →
        // the directive is unchanged. See `director_directive_with_history`.
        let goal = director_directive_with_history(&conversation, &options.requirement, goal);
        let directive = match firmware.as_deref() {
            Some(fw) if backend != "claude-code" || reused_resident => {
                format!("{fw}\n\n---\n\n{goal}")
            }
            _ => goal,
        };
        // GOAL MODE (mirrors the legacy pipeline's `with_goal_mode`): front-load a
        // persistent-`/goal` framing so the base keeps working until the objective is
        // met instead of stopping early. `goal_mode` is set by the `/goal` command
        // (and defaulted on for every director build — Claude Code's native persistent
        // mode is strictly stronger than a plain prompt loop). The ENCODING follows the
        // borrowed brain's CAPABILITY: a native-`/goal` base gets a real `/goal`
        // command; every base without that capability gets the same intent as a
        // prompt fallback
        // (the director loop drives them to completion regardless). It MUST be the very
        // first thing the base reads, so it prepends ahead of the firmware block too.
        // Fail-open: `UMADEV_NO_GOAL_MODE=1`, or a backend whose capabilities can't be
        // read, leaves the directive exactly as before.
        let directive = match resolve_goal_mode(&backend, goal_mode) {
            Some(persistent_goal) => format!(
                "{}{directive}",
                umadev_agent::experts::goal_mode_prefix(&options.requirement, persistent_goal)
            ),
            None => directive,
        };
        let sink_dyn: Arc<dyn EventSink> = sink.clone();
        // Drive the loop ROUTED (Blocker #1 fix): pass the route computed at the top
        // of this task so the director loop emits the visible intent card, synthesises
        // + posts the owned plan (`PlanPosted`), drives the plan step-by-step
        // (`PlanStepStatus`), runs per-step acceptance on the deterministic floor, and
        // finalizes — exactly as the CLI `umadev run` path does. The unrouted entry
        // (route=None) skipped `synthesize_and_post_plan` and step scheduling, which is
        // why the flagship plan/schedule/finalize/acceptance machinery was DEAD on the
        // TUI `/run`. Fail-open: an unparseable/empty plan inside the routed entry just
        // degrades to the single-turn loop, so this never loses a build.
        // Cross-session RESUME (`/continue` on a fresh session): try to re-attach to
        // the persisted plan and drive ONLY the remaining steps. `drive_director_loop_resume`
        // returns `None` when there is nothing resumable (absent / corrupt / fully-done
        // plan) OR the first remaining step can't drive on this fresh session — in both
        // cases we fail open to a fresh routed run, so a resume never loses the build.
        // A non-resume `/run` / `/goal` skips straight to the fresh routed run.
        // A2#3/#4 + A1-GAP1: scope the hosting UI's interaction hooks around the
        // WHOLE drive (a tokio task-local — everything awaited inside inherits it):
        //  - `confirm_gates: true` revives the two spec-MUST gates on this default
        //    path (a guarded run pauses at docs_confirm / preview_confirm);
        //  - `approval` backs an escalated base action with the SAME y/n
        //    `await_user_approval` pause the chat drain uses (bounded, fail-open
        //    deny) instead of a silent headless auto-deny;
        //  - `steer` lets the loop fold queued user steering into the next step.
        // The CLI drive never scopes this, so headless behaviour is unchanged.
        let interaction = {
            let holder = approval.clone();
            let cb_sink = sink.clone();
            let approval_cb: umadev_agent::ApprovalFn =
                Arc::new(move |action: String, target: String| {
                    let holder = holder.clone();
                    let cb_sink = cb_sink.clone();
                    Box::pin(async move {
                        matches!(
                            await_user_approval(&holder, &cb_sink, &action, &target).await,
                            ApprovalReply::Allow
                        )
                    }) as umadev_agent::ApprovalFuture
                });
            let input_holder = host_input.clone();
            let input_sink = sink.clone();
            let host_request_cb: umadev_agent::HostRequestFn = Arc::new(
                move |_req_id: String, request: umadev_runtime::HostRequest| {
                    let input_holder = input_holder.clone();
                    let input_sink = input_sink.clone();
                    Box::pin(async move {
                        Some(await_host_input(&input_holder, &input_sink, &request).await)
                    }) as umadev_agent::HostRequestFuture
                },
            );
            umadev_agent::RunInteraction {
                steer: Some(steer),
                approval: Some(approval_cb),
                host_request: Some(host_request_cb),
                confirm_gates: true,
            }
        };
        // COLD-context critics (B2#1): scope a fresh stateless one-shot judge
        // surface over the whole drive so the adversarial seats (QA + security)
        // review with NO doer context. Fail-open: a surface that can't serve makes
        // those seats fall back to their read-only fork, exactly today's path.
        let cold_surface = cold_judge_surface(&backend, &model, &root);
        // Box::pin the (large) drive future: the task-local scope wrapper would
        // otherwise hold it inline and trip `clippy::large_futures`.
        let outcome = umadev_agent::critics::with_cold_surface(
            cold_surface,
            Box::pin(umadev_agent::hosted_interaction(interaction, async {
                let resumed = if resume {
                    umadev_agent::drive_director_loop_resume(
                        session.as_mut(),
                        &options,
                        &sink_dyn,
                        &route,
                    )
                    .await
                } else {
                    None
                };
                if let Some(o) = resumed {
                    o
                } else {
                    // An explicitly requested resume found nothing resumable
                    // (plan gone, or already terminal). Falling open to a
                    // fresh run is the safe behaviour — but it must SAY so:
                    // the user just read a "resuming your last task" banner,
                    // and silently restarting from scratch under it was the
                    // reported trap. Blocked-settled plans no longer land
                    // here (they reopen as a repair); this is the residue.
                    if resume {
                        sink_dyn.emit(umadev_agent::EngineEvent::Note(
                            umadev_i18n::tl("continue.fresh_fallback").to_string(),
                        ));
                    }
                    umadev_agent::drive_director_loop_routed(
                        session.as_mut(),
                        &options,
                        &sink_dyn,
                        directive,
                        Some(&route),
                    )
                    .await
                }
            })),
        )
        .await;
        // Capture the director's native conversation id before settling ownership
        // of its live process. A clean hand-back must resume THIS build conversation on the
        // next ordinary chat turn; relying on `--continue`/"most recent" is racy
        // when another base session exists in the same workspace. Bases without a
        // resumable id remain fail-open on UmaDev's bounded transcript replay.
        let settled_base_session_id = session.session_id().map(str::to_string);
        let settled_base_resume_identity = settled_base_session_id.as_ref().and_then(|_| {
            session.resume_identity().cloned().or_else(|| {
                crate::session_slot::requested_resume_identity(&backend, &root, permissions)
            })
        });
        let operational_pause = matches!(
            &outcome,
            umadev_agent::DirectorLoopOutcome::PausedAtOperational { .. }
        );
        settle_director_session(
            session,
            &session_holder,
            SessionIdentity::for_launch(&backend, &root, permissions),
            operational_pause,
        )
        .await;

        match outcome {
            umadev_agent::DirectorLoopOutcome::Planned { .. } => {
                // Defensive only: the mode ceiling above normally makes this
                // unreachable. Preserve the typed non-build meaning if another
                // caller reaches the shared loop without executing anything.
                let _ = route_tx.send(RouteDecision::RunNotExecuted);
            }
            umadev_agent::DirectorLoopOutcome::Done { reply } => {
                // Objective source-present hard-gate (the deterministic reality
                // floor) — the SAME check the free-text agentic path + the CLI run
                // apply. A `/run` that CLAIMED a build but produced zero real source
                // is reported honestly (an `ABORT_SENTINEL` note), never celebrated.
                let source_obligation = route.uses_director_workflow()
                    && route.kind != umadev_agent::TaskKind::DocsOnly;
                if let Some(note) = director_source_hardgate(&root, &reply, source_obligation) {
                    // This is an objective terminal rejection, not an advisory.
                    // Emitting AgenticDone after the abort note would let the event
                    // loop mark the same task Failed and then overwrite it to Done,
                    // hand back a failed session, and show a completion card. Keep
                    // the sentinel event for the aborted UI state, then settle the
                    // route honestly as Failed.
                    sink.emit(EngineEvent::Note(note.clone()));
                    let reason = note
                        .strip_prefix(ABORT_SENTINEL)
                        .unwrap_or(note.as_str())
                        .to_string();
                    let _ = route_tx.send(RouteDecision::Failed(reason));
                    return;
                }
                // The body already streamed live; hand the assembled text to the
                // event loop to record as the assistant turn + clear `thinking`. A
                // director loop is ALWAYS a Build → the hand-back fires.
                let _ = route_tx.send(RouteDecision::AgenticDone {
                    reply,
                    director_build: true,
                    // Pin the hand-back to the director's exact native session.
                    // `record_agentic_done` stores it on App, and the resident
                    // pre-loader resumes that id before the next chat turn.
                    base_session_id: settled_base_session_id,
                    base_resume_identity: settled_base_resume_identity,
                });
            }
            umadev_agent::DirectorLoopOutcome::Failed(reason) => {
                // An honest terminal abort (session died / a turn failed). Flag the
                // terminal state (so the bar shows a real aborted state) + clear
                // `thinking` via the terminal Failed decision.
                sink.emit(EngineEvent::Note(format!("{ABORT_SENTINEL}{reason}")));
                // Discoverability: on a TRANSIENT abort with a plan still resumable
                // on disk, point the user at `/continue`. A plain Note (no sentinel),
                // so it lands AFTER the abort note above. Fail-open (no plan → skip).
                if let Some(hint) = umadev_agent::transient_resume_hint(&reason, &root) {
                    sink.emit(EngineEvent::Note(hint));
                }
                let _ = route_tx.send(RouteDecision::Failed(reason));
            }
            umadev_agent::DirectorLoopOutcome::PausedAtGate { gate } => {
                // Spec-MUST gate pause (A1-GAP1): the loop already persisted the
                // plan + open door and emitted `GateOpened` (the gate card renders
                // through the engine stream). NO source hard-gate here — the build
                // is parked mid-flight, not settled. The session was ended above;
                // the resume re-attaches via the persisted base session id +
                // plan.json. This terminal decision clears `thinking` and arms the
                // app's director-pause marker so approval / a revision resume the
                // director loop instead of the legacy gate blocks.
                let _ = route_tx.send(RouteDecision::RunPausedAtGate { gate });
            }
            umadev_agent::DirectorLoopOutcome::PausedAtBudget { done, total } => {
                // RESUMABLE budget pause (Stage 1/2): the run stopped only because its
                // wall-clock budget was exhausted while resumable steps remained. The
                // loop already checkpointed the plan; present this as a PAUSE, never an
                // abort — NO ABORT_SENTINEL, no gate card. The terminal decision keeps
                // the plan panel (frozen), pushes the resume hint carrying done/total,
                // and arms the budget-pause marker so `/continue` finishes the rest.
                let _ = route_tx.send(RouteDecision::RunPausedAtBudget { done, total });
            }
            umadev_agent::DirectorLoopOutcome::PausedAtOperational {
                reason,
                done,
                total,
            } => {
                let _ = route_tx.send(RouteDecision::RunPausedAtOperational {
                    reason,
                    done,
                    total,
                });
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowResumeIdentity {
    pub(super) base_session_id: Option<String>,
    pub(super) base_resume_identity: Option<umadev_runtime::BaseResumeIdentity>,
    pub(super) handoff_from: Option<String>,
}

/// Resolve the vendor-session pointer for a TUI workflow resume.
///
/// Session ids are meaningful only inside the exact base that minted them. The
/// workflow's requirement, plan, and artifacts may cross a base handoff, but its
/// opaque vendor id may not. Keeping this decision pure makes the ownership rule
/// directly regression-testable without launching any vendor CLI.
pub(super) fn resolve_workflow_resume_identity(
    resume: bool,
    persisted: Option<&umadev_agent::WorkflowState>,
    current_backend: &str,
    current_workspace: &std::path::Path,
    current_permissions: umadev_runtime::BasePermissionProfile,
) -> WorkflowResumeIdentity {
    if !resume {
        return WorkflowResumeIdentity {
            base_session_id: None,
            base_resume_identity: None,
            handoff_from: None,
        };
    }
    let Some(state) = persisted else {
        return WorkflowResumeIdentity {
            base_session_id: None,
            base_resume_identity: None,
            handoff_from: None,
        };
    };
    if state.backend == current_backend {
        let id = state
            .base_session_id
            .clone()
            .filter(|id| !id.trim().is_empty());
        let requested = crate::session_slot::requested_resume_identity(
            current_backend,
            current_workspace,
            current_permissions,
        );
        let identity_matches = match (state.base_resume_identity.as_ref(), requested.as_ref()) {
            (Some(saved), Some(requested)) => saved.permits_resume_as(requested, false),
            // Legacy identity-free ids remain compatible on the three native
            // transports only when their stored permission profile also matches.
            // Grok ACP load is too late to enforce its immutable process sandbox,
            // so missing identity/preflight always opens a fresh process.
            (None, Some(_)) => {
                current_backend != "grok-build"
                    && state.resolved_permission_profile() == current_permissions
            }
            _ => false,
        };
        return WorkflowResumeIdentity {
            base_session_id: identity_matches.then_some(id).flatten(),
            base_resume_identity: identity_matches
                .then(|| state.base_resume_identity.clone())
                .flatten(),
            handoff_from: None,
        };
    }
    WorkflowResumeIdentity {
        base_session_id: None,
        base_resume_identity: None,
        handoff_from: Some(if state.backend.is_empty() {
            "offline".to_string()
        } else {
            state.backend.clone()
        }),
    }
}
