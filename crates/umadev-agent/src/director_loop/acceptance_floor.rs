//! The REQUIRED acceptance floor a deliberate build's QC folds in, kept out of the
//! director loop itself.

use super::{has_reproduction_test, runtime_proof_blocking, RoutePlan, RunOptions};
use crate::acceptance::EndpointAcceptance;

/// What the REQUIRED acceptance floor found: the blocking findings a fix turn is
/// asked to repair, and a note for every check that genuinely could not run. Such a
/// check is a neutral skip the user sees, never a blocking finding the base cannot
/// repair.
#[derive(Debug, Default)]
pub(super) struct AcceptanceFloor {
    pub(super) blocking: Vec<String>,
    pub(super) notes: Vec<String>,
}

/// The REQUIRED acceptance floor for a deliberate build (Wave 4, §L4 / task 2) —
/// the spec→tasks + spec→code verification, promoted to a blocking signal on the
/// default deliberate path. Folds in coverage gaps, interface-acceptance gaps,
/// frontend↔contract drift, an unverified runtime-proof, and (for a Bugfix) a
/// missing reproduction test. Each contributor is fail-open: a missing artifact /
/// unparseable doc yields no gap (a neutral skip), so a check that genuinely
/// cannot run never fabricates a failure. Returns the blocking lines (empty =
/// the floor is clean OR nothing could be checked).
#[cfg(test)]
pub(super) fn acceptance_floor_blocking(
    options: &RunOptions,
    route: Option<&RoutePlan>,
) -> Vec<String> {
    acceptance_floor(options, route, None).blocking
}

/// The acceptance-floor core, optionally reusing scope drift the caller already
/// computed.
///
/// `unclaimed_changes` is not cheap: it stages the whole work-tree into the shadow index
/// (`git add -A --force`), reads a full run diff, and runs a repo-wide backend-route
/// extraction. A QC pass reuses that one snapshot so every scope finding is judged
/// against the same workspace state.
///
/// `scope: None` keeps the standalone behaviour (compute it here) for callers that have
/// no precomputed set.
pub(super) fn acceptance_floor(
    options: &RunOptions,
    route: Option<&RoutePlan>,
    scope: Option<&[crate::scope_creep::ScopeFinding]>,
) -> AcceptanceFloor {
    let slug = options.effective_slug();
    let root = &options.project_root;
    let mut out: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // spec→tasks: a declared FR-NNN no task covers (a requirement at risk of being
    // silently dropped). Fail-open: no PRD / no FR ids → empty.
    for r in crate::coverage::uncovered_requirements(root, &slug) {
        out.push(format!(
            "coverage gap: requirement {r} is declared in the PRD but no task implements it — \
             build it, or remove it from scope honestly"
        ));
    }
    // spec→code: a planned API endpoint with no implementation evidence on disk.
    // Fail-open: no architecture doc / no endpoints → empty. A scan that could not
    // read the whole source tree cannot say an endpoint is missing: that is a note,
    // not a gap the base is asked to "implement".
    match crate::acceptance::endpoint_acceptance(root, &slug) {
        EndpointAcceptance::Checked(gaps) => out.extend(
            gaps.into_iter()
                .map(|g| format!("acceptance gap: planned endpoint not implemented — {g}")),
        ),
        EndpointAcceptance::Unavailable(reason) => {
            notes.push(umadev_i18n::tlf("qc.acceptance_unavailable", &[&reason]));
        }
    }
    // frontend↔backend contract drift: a fetch URL with no matching backend route —
    // the same check the quality floor hands the critics.
    for v in crate::continuous::frontend_contract_drift(options, &slug) {
        out.push(format!("contract drift: {v}"));
    }

    // runtime-proof: when a `runtime-proof.json` was written (by `verify --runtime`)
    // and it did NOT verify, that is a real, recorded failure (the app didn't boot /
    // a route didn't answer). Absent file → neutral skip (the runtime check simply
    // wasn't run this loop; we never fabricate a "didn't boot" from a missing file).
    if let Some(line) = runtime_proof_blocking(root) {
        out.push(line);
    }

    // ARCHITECTURE FITNESS (UD-CODE-006, spec §3.6): the REPO-GLOBAL
    // half of the anti-spaghetti floor — the architecture doc's declared
    // layer-dependency rules (`## Layering` order / `LAYER-RULE: a !-> b`),
    // verified against the repo-map's resolved import edges. The touched-file
    // rules (god-file / added-code clones / comment hygiene) run at the STEP level in
    // `drive_build_step`, where the changed-file set is known from the pre-step
    // baseline; here the empty touched set makes them a silent no-op by
    // construction. Fail-open: no doc / no declaration / no resolved edges →
    // empty, never a fabricated failure.
    for f in crate::arch_fitness::arch_fitness_findings(root, &slug, &[]) {
        if f.blocking {
            out.push(f.message);
        }
    }

    // SCOPE CREEP — the DUAL of the coverage check above. Coverage asks "which declared
    // requirement has no step?" (UNDER-building). This asks the opposite: "which CHANGE
    // belongs to no step?" (OVER-building) — an unplanned dependency, an unplanned
    // source file, an unplanned public route: work nobody sized, nobody asked for, and
    // nobody reviewed. New surfaces and edits to existing files both violate the
    // execution contract. A missing run baseline or unreadable diff remains fail-open;
    // malformed/missing step file declarations are rejected by plan preflight.
    // See [`crate::scope_creep`]. Reuse the caller's set when it already paid for one.
    match scope {
        Some(findings) => out.extend(
            findings
                .iter()
                .filter(|f| f.blocking)
                .map(|f| f.message.clone()),
        ),
        None => {
            if let Some(plan) = crate::plan_state::load(root) {
                for f in crate::scope_creep::unclaimed_changes(root, &plan) {
                    if f.blocking {
                        out.push(f.message);
                    }
                }
            }
        }
    }

    // BUGFIX: require a reproduction test (red→green). A fix that lands no test
    // asserting the bug can silently regress. Fail-open: only fires when the route
    // is classified Bugfix AND we can read the source tree.
    if route
        .map(|r| r.kind == crate::planner::TaskKind::Bugfix)
        .unwrap_or(false)
        && !has_reproduction_test(root)
    {
        out.push(
            "bugfix without a reproduction test: add a test that FAILS on the bug before the fix \
             and PASSES after (red→green), and keep the rest of the suite green — a fix with no \
             test asserting the bug can silently regress"
                .to_string(),
        );
    }

    // DESIGN-SYSTEM CONFORMANCE (UD-CODE-007, spec §3.7): the deterministic half
    // of the design moat. The firmware PREACHES token discipline, paired
    // foregrounds, measured contrast, and one committed hue — but a prompt is not
    // a floor. This is the floor:
    //
    //   - `007a` schema     — a real system (>= 6 color roles each with a paired
    //                         `on-` foreground, a >= 4-step type scale at ratio
    //                         >= 1.125, a 4pt spacing scale, a radius scale,
    //                         >= 2 durations + >= 1 easing), not `:root{--bg:#000}`.
    //   - `007b` contrast   — every DECLARED (surface, on-surface) pair MEASURED
    //                         with the WCAG formula in pure Rust (no browser, no
    //                         deps): 4.5:1 body, 3:1 large/UI.
    //   - `007c` drift      — the UI actually DRAWS from the token set (a literal
    //                         color / font / radius / size off the scale is drift).
    //   - `007d` hue        — no AI indigo/violet primary/accent unless the
    //                         requirement asked for purple.
    //   - `007e` lints      — the register-scoped design-lint registry; only its
    //                         small P0 tier blocks, the advisory tier is a Note.
    //   - `007f` direction  — the designer decided a DIRECTION before any token.
    //
    // Fail-open at EVERY edge: no `design-tokens.{json,css}` → the report is
    // `unavailable` and contributes nothing (a project that never asked for a
    // design system is completely unaffected); no UIUX doc → no direction finding.
    // Only a project that SHIPPED a design system is held to the contract it
    // implicitly claimed.
    let register = crate::design_system::register_for_project(root, &slug);
    let report = crate::design_system::verify_design_system(root, &options.requirement, register);
    for f in report.blocking() {
        out.push(f.message.clone());
    }
    // `007f` is gated on the ROUTE, not on a file: an `output/*-uiux.md` left behind by
    // an earlier UI run (or already present in a brownfield repo) is not a reason to
    // hold a backend-only task to a design contract it never entered. No route → no UI
    // claim → nothing (fail-open).
    let needs_ui = route.is_some_and(RoutePlan::needs_ui);
    for f in crate::design_system::visual_direction_findings(root, &slug, needs_ui) {
        if f.blocking {
            out.push(f.message);
        }
    }

    AcceptanceFloor {
        blocking: out,
        notes,
    }
}
