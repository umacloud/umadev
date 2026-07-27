use super::{
    diagnosed_blockers_for_prompt, director, plan_state, quality_evidence, Arc, BaseSession,
    EngineEvent, EventSink, RoutePlan, RunOptions, StepOutcome,
};

/// Retry one exact review boundary without reopening the writer.
///
/// An operational `/continue` authorizes the missing read-only verdict, not a
/// fresh repair/re-review loop. Pass settles the step, another outage re-parks it,
/// and a semantic finding closes it as blocked so the user can explicitly start a
/// writable `/run`. The saved route supplies the original required roster.
pub(super) async fn retry_review_step_once(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
) -> StepOutcome {
    let review = director::review_with_seats(session, options, events, &route.team).await;
    let seats = review.seats;
    let review = quality_evidence::split_review_evidence(&review);
    if !review.operational.is_empty() {
        events.emit(EngineEvent::Note(quality_evidence::operational_stop_note(
            &review.operational,
        )));
        return StepOutcome {
            accepted: false,
            reply: String::new(),
            drove: seats > 0,
            made_progress: false,
            unavailable: true,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: review.blocking,
            operational_unavailable: review.operational,
        };
    }
    if review.blocking.is_empty() {
        let reviewed = seats > 0;
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: reviewed,
            made_progress: reviewed,
            unavailable: false,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: Vec::new(),
            operational_unavailable: Vec::new(),
        };
    }
    events.emit(EngineEvent::Note(format!(
        "team · resumed review returned {} actionable finding(s) — the parked step is \
         closed without reopening source writes; use /run to repair explicitly",
        review.blocking.len()
    )));
    StepOutcome {
        accepted: false,
        reply: String::new(),
        drove: seats > 0,
        made_progress: false,
        unavailable: false,
        base_agents: crate::bg_agents::BaseAgentObservation::default(),
        gap_evidence: review.blocking,
        operational_unavailable: Vec::new(),
    }
}

/// Drive ONE Review step: fork the cross-review team (read-only) over the current
/// blackboard. A review step is clean only when every convened seat returns pass;
/// blocking findings fold into ONE bounded fix turn on the MAIN session (the doer
/// repairs), then we re-read. Returns a [`StepOutcome`].
///
/// HIGH #1 / MEDIUM #3: an EMPTY-team review (the route convened no seats — 0 actually
/// reviewed) is a NEUTRAL SKIP, NOT real progress: `made_progress == false`, so the
/// scheduler does NOT tick it `Done` over a review that never happened. A team that
/// actually convened (`seats > 0`) and accepted is real progress.
///
/// Wall-clock ceiling (graceful): the read-only fork review ALWAYS runs (it's cheap
/// and surfaces honest findings), but the minute-level main-session FIX turn it would
/// trigger is skipped once the budget is spent — the findings are then surfaced as an
/// honest note and left for the final gate / hard-gate, never silently grinding past
/// the deadline.
pub(super) async fn drive_review_step(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
    deadline: std::time::Instant,
) -> StepOutcome {
    let _ = step;
    // Wave 2 deliverable 3: size the review team from the ROUTE's seats (the seats
    // the router already chose for this turn), not from a re-derived requirement
    // classification. An empty route team → no cross-review (the floor stands).
    let review = director::review_with_seats(session, options, events, &route.team).await;
    let seats = review.seats;
    let review = quality_evidence::split_review_evidence(&review);
    if !review.operational.is_empty() {
        events.emit(EngineEvent::Note(quality_evidence::operational_stop_note(
            &review.operational,
        )));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: seats > 0,
            made_progress: false,
            unavailable: true,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: review.blocking,
            operational_unavailable: review.operational,
        };
    }
    if review.blocking.is_empty() {
        // A team actually convened (seats > 0) ⇒ real review progress; an empty team
        // (seats == 0) is a neutral skip that must NOT advance the done count.
        let reviewed = seats > 0;
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: reviewed,
            made_progress: reviewed,
            unavailable: false,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: Vec::new(), // review accepted → no gap
            operational_unavailable: Vec::new(),
        };
    }
    // The team found blockers after the fix budget ended. Preserve those blockers
    // on the step instead of accepting an empty result and hoping a later pass finds
    // them again.
    if std::time::Instant::now() >= deadline {
        events.emit(EngineEvent::Note(
            "team · time budget reached — review findings left for the final gate \
             (raise UMADEV_RUN_BUDGET_SECS to repair them in this run)"
                .to_string(),
        ));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: seats > 0,
            made_progress: false,
            unavailable: false,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: review.blocking,
            operational_unavailable: Vec::new(),
        };
    }
    // The team found blocking issues — fold them into ONE bounded fix turn on the
    // main session, then require a clean re-review before marking the step clean.
    let mut body = String::new();
    for b in &review.blocking {
        body.push_str("- ");
        body.push_str(b);
        body.push('\n');
    }
    let directive = format!(
        "The review team flagged MUST-FIX issues in what was built so far. Fix EVERY one \
         now by editing the files directly — do not narrate, just apply the fixes and \
         re-run any build/test you already ran. Issues:\n{body}\n{}\nWhen all are fixed, end \
         your turn.",
        diagnosed_blockers_for_prompt(&review.blocking, "team review")
    );
    let rework = crate::continuous::drive_rework_turn_capturing(
        session, options, events, directive, deadline,
    )
    .await;
    let drove = rework.done;
    let base_agents = rework.base_agents;
    // Re-run the same required review after the fix. A failed review transport is
    // unavailable, and a residual semantic blocker remains a blocker; neither may
    // be rewritten into a clean pass.
    let recheck = director::review_with_seats(session, options, events, &route.team).await;
    let recheck = quality_evidence::split_review_evidence(&recheck);
    if !recheck.operational.is_empty() {
        events.emit(EngineEvent::Note(
            quality_evidence::operational_recheck_note(&recheck.operational),
        ));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove,
            made_progress: false,
            unavailable: true,
            base_agents,
            gap_evidence: recheck.blocking,
            operational_unavailable: recheck.operational,
        };
    }
    if !recheck.blocking.is_empty() {
        events.emit(EngineEvent::Note(format!(
            "team · review step still has {} must-fix finding(s) after rework — preserving them as blockers",
            recheck.blocking.len()
        )));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove,
            made_progress: false,
            unavailable: false,
            base_agents,
            gap_evidence: recheck.blocking,
            operational_unavailable: Vec::new(),
        };
    }
    // A team convened, raised findings, and a repair turn ran — real review progress
    // regardless of whether the repair turn fully settled (`drove`).
    StepOutcome {
        accepted: true,
        reply: String::new(),
        drove,
        made_progress: true,
        unavailable: false,
        base_agents,
        gap_evidence: Vec::new(),
        operational_unavailable: Vec::new(),
    }
}
