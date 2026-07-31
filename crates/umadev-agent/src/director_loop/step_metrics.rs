use super::{plan_state, Arc, EngineEvent, EventSink, RoutePlan, RunOptions};

/// Record this build step's FIRST-PASS acceptance outcome into UmaDev's measured
/// engineering-doctrine signal ([`crate::first_pass`]) and surface the running
/// rate as a visible advisory [`EngineEvent::Note`].
///
/// `first_pass` is `true` iff the step's deterministic acceptance passed on the
/// FIRST attempt (round 0, ZERO rework rounds). The outcome is recorded under BOTH
/// the doer-seat kind (the step-kind dimension) AND the route-class kind (the
/// route-class dimension), so both accumulate from one call. ADVISORY + FAIL-OPEN:
/// recording never changes the step's pass/fail outcome, the loop, or any gate —
/// it only feeds the visible metric + later nudges.
pub(super) fn record_step_first_pass(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
    first_pass: bool,
) {
    let kinds = [
        crate::first_pass::seat_kind(step.seat.role_id()),
        crate::first_pass::class_kind(route.class.as_str()),
    ];
    crate::first_pass::record_many(
        &options.project_root,
        &[
            (kinds[0].as_str(), first_pass),
            (kinds[1].as_str(), first_pass),
        ],
    );
    for kind in kinds {
        // Surface the running rate so the signal is visible (only once a kind has
        // crossed the trusted min-sample threshold). Pure observation.
        if let Some(rate) = crate::first_pass::first_pass_rate(&options.project_root, &kind) {
            events.emit(EngineEvent::Note(format!(
                "signal · first-pass acceptance {kind}: {:.0}% (advisory; the floor still governs)",
                rate * 100.0
            )));
        }
    }
}

/// Record this run's SIZING-calibration outcome (the single-turn loop's entry point):
/// the router's PREDICTED size for `route` vs. the `actual` size the run settled at,
/// keyed by route-class ([`crate::sizing_calibration`]). A `None` route (the
/// backward-compatible no-route entry) records nothing.
///
/// ADVISORY + FAIL-OPEN: recording never changes the run's route, plan, the
/// deterministic floor, loop termination, or any gate — by the time the actual size is
/// known the route is long-decided. It only feeds the per-class calibration that informs
/// a FUTURE default (see [`crate::sizing_calibration::calibrated_default`]).
pub(super) fn record_run_sizing(
    options: &RunOptions,
    route: Option<&RoutePlan>,
    actual: crate::sizing_calibration::SizeRank,
) {
    if let Some(r) = route {
        crate::sizing_calibration::record_route(&options.project_root, r, actual);
    }
}
