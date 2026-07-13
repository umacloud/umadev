//! Director team capabilities — the internal Rust levers UmaDev calls during its
//! own QC pass (the USB / smart-hardware model of
//! `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md`, simplified: NO marker protocol).
//!
//! UmaDev never asks the base to speak a scheduling protocol to "summon a team" from
//! the outside — there is NO base-emitted lever syntax. Instead these four functions
//! are **UmaDev's OWN internal Rust capabilities**, and [`crate::director_loop`]
//! invokes them in one of two build modes, chosen AUTOMATICALLY from the router's
//! depth signal (Wave A):
//!
//! - **Deliberate build** → UmaDev's own loop drives the plan SEAT-BY-SEAT
//!   ([`crate::director_loop::drive_plan_steps`]): it [`summon`]s each step's seat
//!   serially on the main session (single-writer) so the team visibly BUILDS its own
//!   deliverables, then [`review`]s / [`verify`]s to judge reality.
//! - **Lean / single-turn build** → the base is already a complete Agent: its body
//!   builds the goal end to end with the team living inside its own head, steered by
//!   UmaDev's injected firmware (identity + craft + knowledge), and these functions
//!   are called AFTER it builds to read reality and judge it.
//!
//! - [`summon`] — drive/fork ONE seat. On a DELIBERATE build the director's build loop
//!   ([`crate::director_loop::drive_plan_steps`]) drives EACH plan step by `summon`ing
//!   its seat in [`SummonMode::Serial`] on the main session (single-writer) — the
//!   seat-by-seat "team builds" path (Wave A). On a lean / single-turn build the base
//!   builds end to end in one turn and the lever is used only for QC composition + any
//!   caller that wants a single-seat round-trip.
//! - [`review`] — fork the cross-review team on read-only sessions and collect
//!   verdicts (UmaDev's QC, NOT the base summoning anyone).
//! - [`verify`] — read an objective reality fact (source-present / build-test /
//!   contract) deterministically; never an opinion.
//! - [`checkpoint`] — pause for the user when the decision is theirs (trust-tiered).
//!
//! Crucially these are **not advertised to the base as levers it must learn** — the
//! marker/lever PROMPT surface is retired. They are pure Rust UmaDev calls. A
//! trivial goal needs none; a real product's QC calls [`verify`] + [`review`] and
//! feeds blocking findings back as a fix directive the base's body acts on.
//!
//! Everything here is a **thin wrapper over machinery that already exists** — it
//! reuses, never reimplements:
//!
//! - [`summon`] in [`SummonMode::Serial`] drives the MAIN session through the same
//!   governed, single-writer turn pump the rework loop uses
//!   ([`continuous::drive_rework_turn`]); in [`SummonMode::Parallel`] it forks a
//!   read-only session ([`continuous::fork_with_timeout`]) and reviews/works on it
//!   ([`continuous::ForkConsult`]) — exactly `critics.rs`'s mechanism, generalised
//!   so the director can invoke it whenever it judges useful.
//! - [`review`] reuses [`continuous::run_review_team`] + [`continuous::team_for`]
//!   (the 8-seat critic roster, scaled to the task) to fork parallel reviewers and
//!   collect their [`RoleVerdict`]s.
//! - [`verify`] reuses the objective checkers — [`crate::verify::run_verify`] (real
//!   build/test/lint), [`continuous::quality_floor`] (contract + coverage drift),
//!   and [`crate::acceptance::source_files`] (real code present) — and returns a FACTUAL
//!   result, never an opinion.
//! - [`checkpoint`] reuses the trust ladder ([`crate::trust::TrustMode`]): in `auto` it
//!   auto-proceeds, in `guarded` / `plan` it genuinely pauses for the user.
//!
//! HARD INVARIANTS (mirrors `critics.rs`; never break — these keep the team SAFE):
//!
//! 1. **Fail-open.** Every tool degrades to a safe no-op on any error: a summon
//!    that can't drive returns "not done" (the director proceeds), a review that
//!    can't fork returns no blocking (accept), a verify that can't run returns
//!    "unavailable / skipped" (not a false failure), a checkpoint that can't reach
//!    the user auto-proceeds. A tool can NEVER block the director on a bug.
//! 2. **Single-writer preserved.** Only [`SummonMode::Serial`] mutates the
//!    workspace, and it does so on the MAIN session under the run-lock the caller
//!    already holds — exactly one doer writes at a time. Parallel summons + every
//!    review run on ISOLATED read-only forks that never touch the main writer.
//! 3. **Objective floor untouched.** [`verify`] is a deterministic reality check
//!    (it RUNS the build / greps the real source); critic verdicts from [`review`]
//!    stay advisory — they fold into a rework directive but never drive loop
//!    termination, which the deterministic floor + the user gate own.
//! 4. **No new endpoint.** Every tool runs over the SAME borrowed brain (the live
//!    [`BaseSession`] + its `fork()`); no extra model endpoint, no extra API key.
//!
//! ## Why no base-facing lever protocol
//!
//! An earlier design exposed these as a marker protocol the base emitted
//! (`<<<umadev:summon …>>>`) that UmaDev parsed and mediated. That is retired: the
//! base is already a whole brain that builds multi-role code internally once the
//! firmware is injected, so making it speak a scheduling protocol to summon a team
//! from the outside was over-design. These functions remain ONLY as UmaDev's own
//! internal Rust calls (used by [`crate::director_loop`] to drive a deliberate build
//! seat-by-seat and to run its QC pass); the base never learns or emits a lever syntax.

use std::sync::Arc;

use umadev_runtime::BaseSession;

use crate::continuous::{self, ReviewKind};
use crate::critics::RoleVerdict;
use crate::events::{EngineEvent, EventSink};
use crate::router::{RouteClass, RoutePlan};
use crate::runner::RunOptions;

/// How a [`summon`]ed seat works the goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummonMode {
    /// The seat DOES work that mutates the workspace — driven on the MAIN session
    /// under the single-writer run-lock the caller holds. Exactly one serial doer
    /// writes at a time (the single-writer invariant). Use for a build/fix slice.
    Serial,
    /// The seat works on an ISOLATED read-only `fork()` — never touches the main
    /// writer. Use for a parallel reviewer / analyst whose output is an OPINION,
    /// not a file write. Falls back to no-op (a fail-open empty result) if the
    /// base can't fork.
    Parallel,
}

impl SummonMode {
    /// Stable lowercase id for events / logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Serial => "serial",
            Self::Parallel => "parallel",
        }
    }
}

/// What a [`summon`] produced. Fail-open by construction: a degraded summon still
/// returns a well-formed result (`done == false`, an explanatory note), never an
/// error — so the director can read it and decide, never get blocked.
#[derive(Debug, Clone, Default)]
pub struct SummonResult {
    /// The seat that was summoned (e.g. `frontend-engineer`).
    pub role: String,
    /// Whether the seat's turn completed (Serial: the doer turn finished;
    /// Parallel: the forked seat returned a verdict). `false` = degraded /
    /// session ended — the director proceeds, it does not block.
    pub done: bool,
    /// Serial only — a DEFINITE no-turn: the doer directive could not even be
    /// SENT (the base process already exited / the pipe is closed), so no turn
    /// ever ran for this summon. Distinguishes a dead session from a turn that
    /// ran but hung/died mid-way after doing real work; the step scheduler marks
    /// such a step Blocked instead of verifying workspace-global evidence an
    /// earlier step left behind. Always `false` for a Parallel summon (a failed
    /// fork is already the fail-open `done == false` accept).
    pub send_failed: bool,
    /// For a Parallel summon: the seat's structured verdict over the artifacts
    /// (None for a Serial doer, which mutates files rather than judging).
    pub verdict: Option<RoleVerdict>,
    /// For a Serial summon: the doer's accumulated assistant text (empty for a
    /// Parallel reviewer). The director reads it for the "claimed a build" gate.
    pub text: String,
    /// For a Serial summon: the failed-tool summaries this doer turn produced —
    /// the pitfall feed the director distils into the lessons KB on the default
    /// loop (Wave 2). Empty for a Parallel reviewer (it writes nothing).
    pub pitfalls: Vec<String>,
}

/// What a [`review`] produced — the deduped, seat-tagged union of every reviewing
/// seat's `blocking` findings, plus whether the team was convened at all. Empty
/// `blocking` = all seats accept (or fail-open) → the director proceeds. The
/// findings are ADVISORY: the director MAY fold them into a rework summon, but
/// they never force loop termination (invariant 3).
#[derive(Debug, Clone, Default)]
pub struct ReviewResult {
    /// How many seats reviewed (0 = lean / no-UI / docs-only path got no team).
    pub seats: usize,
    /// The union of must-fix findings, each tagged `[seat] finding`. Empty =
    /// nothing blocking.
    pub blocking: Vec<String>,
}

impl ReviewResult {
    /// Whether any seat raised a blocking finding.
    #[must_use]
    pub fn has_blocking(&self) -> bool {
        !self.blocking.is_empty()
    }
}

/// A single objective check the director can [`verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyKind {
    /// Run the project's REAL build / test / lint sequence (via
    /// [`crate::verify::run_verify`]) and report pass/fail per step.
    BuildTest,
    /// Cross-check the frontend↔backend API contract + requirement coverage
    /// (via [`continuous::quality_floor`]) — drift = a factual gap.
    Contract,
    /// Confirm real source files actually exist on disk (via
    /// [`crate::acceptance::source_files`]) — the "did anything get built" floor.
    SourcePresent,
    /// Confirm the designer's **design-tokens** deliverable
    /// (`design-tokens.{json,css}`) is a REAL file on the blackboard (via
    /// [`crate::acceptance::design_tokens_files`]) — the designer seat's
    /// anti-theatre floor. This is the deterministic half of the design system;
    /// the existing governance (UD-CODE-001 emoji-as-icon / UD-CODE-002 hardcoded
    /// colors) is the qualitative half — together they make "the team has a design
    /// system" a checkable fact, not a narrated claim.
    DesignTokensPresent,
}

impl VerifyKind {
    /// Stable lowercase id for events / logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuildTest => "build-test",
            Self::Contract => "contract",
            Self::SourcePresent => "source-present",
            Self::DesignTokensPresent => "design-tokens-present",
        }
    }
}

/// The FACTUAL outcome of a [`verify`] — never an opinion. `available == false`
/// means the check genuinely couldn't run (no project manifest / no architecture
/// doc) and was SKIPPED, which is NOT a failure — the director must not treat an
/// unavailable check as a red signal (fail-open invariant 1).
#[derive(Debug, Clone, Default)]
pub struct VerifyResult {
    /// Whether the check could run at all. `false` = skipped (e.g. nothing to
    /// build / no contract to compare) — neutral, not a failure.
    pub available: bool,
    /// Whether the check PASSED. Only meaningful when `available`. A skipped
    /// check reports `passed == true` so an absent check never blocks (it found
    /// no problem because there was nothing to check).
    pub passed: bool,
    /// Concrete evidence lines (failed step names, drift findings, a file count)
    /// — what the director reads to decide its next move.
    pub evidence: Vec<String>,
}

/// Whether a [`checkpoint`] paused for the user or auto-proceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointDecision {
    /// The trust tier let the director proceed without asking (e.g. `auto`).
    AutoProceed,
    /// The director genuinely paused for the user (guarded / plan) — the caller
    /// must surface the question and wait for the user's reply before continuing.
    AskUser,
}

// ===================================================================
// summon — delegate a slice of work to a named seat
// ===================================================================

/// **Summon a team member** — the director's unit of delegation. Inject the
/// `role`'s identity + craft + relevant knowledge as a directive, then either
/// drive it on the MAIN session (`Serial` doer, single-writer) or on a read-only
/// `fork()` (`Parallel` reviewer).
///
/// - `Serial`: reuses [`continuous::drive_rework_turn`] — the SAME governed,
///   audited, approval-mediated turn pump the rework loop uses, so a summoned
///   doer is governed exactly like any phase turn and writes under the run-lock
///   the caller holds (single-writer invariant 2). Returns `done = true` when the
///   turn completed.
/// - `Parallel`: forks a read-only session ([`continuous::fork_with_timeout`]) and
///   runs the seat's review over it ([`continuous::ForkConsult`]) — never touches
///   the main writer. Returns the seat's [`RoleVerdict`]. A base that can't fork
///   fails open to an accepting empty verdict.
///
/// Fail-open: any error (send failure, dead session, no fork) yields a degraded
/// [`SummonResult`] (`done = false` / an empty accepting verdict) — never blocks.
pub async fn summon(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    role: &str,
    instruction: &str,
    mode: SummonMode,
    // The run's wall-clock ceiling, threaded into the `Serial` doer's turn pump so an
    // ACTIVE base can't run ONE summon turn unbounded past the run budget (the mid-turn
    // graceful settle in `continuous::drive_rework_turn_with_idle`). Ignored by a
    // `Parallel` (read-only fork) seat — a fork review is bounded by its own timeout.
    deadline: std::time::Instant,
) -> SummonResult {
    events.emit(EngineEvent::Note(format!(
        "team · summon {role} ({})",
        mode.as_str()
    )));

    match mode {
        SummonMode::Serial => {
            // Build the role directive: persona/identity + the team's relevant
            // knowledge + the concrete instruction. Then drive it on the MAIN
            // session through the governed rework-turn pump (single-writer). The
            // capturing variant threads the doer's text + failed-tool pitfalls back
            // so the director can run the "claimed a build" gate and feed the
            // lessons KB on the default loop — every tool call is still governed +
            // audited inside the pump (UD-EVID-002), exactly as a phase turn.
            let directive = summon_directive(options, role, instruction);
            let turn = continuous::drive_rework_turn_capturing(
                session, options, events, directive, deadline,
            )
            .await;
            SummonResult {
                role: role.to_string(),
                done: turn.done,
                send_failed: turn.send_failed,
                verdict: None,
                text: turn.text,
                pitfalls: turn.pitfalls,
            }
        }
        SummonMode::Parallel => {
            // A parallel seat is an OPINION over the on-disk artifacts — run it on
            // a read-only fork so it can never collide with the main writer. We
            // reuse the critic roster's seat for `role` when one exists (so the
            // parallel seat carries the same craft as the review team); if `role`
            // isn't a known critic seat, the seat falls back to a fail-open accept
            // (an unknown parallel seat never blocks).
            let verdict = summon_parallel_seat(session, options, role, instruction).await;
            let done = verdict.is_some();
            let verdict = verdict.unwrap_or_else(|| RoleVerdict::empty(role));
            SummonResult {
                role: role.to_string(),
                done,
                send_failed: false,
                verdict: Some(verdict),
                text: String::new(),
                pitfalls: Vec::new(),
            }
        }
    }
}

/// Build the directive a [`SummonMode::Serial`] doer is driven with: the team's
/// always-on director identity + the craft block + a small requirement-scoped
/// knowledge digest, then a clear ROLE line and the concrete instruction. Reuses
/// the existing prompt policy in `experts` / the knowledge digest in `phases` —
/// the wording lives in one place.
fn summon_directive(options: &RunOptions, role: &str, instruction: &str) -> String {
    // Relevant accumulated experience for this slice, SCOPED TO THE SEAT (not just
    // the step-instruction text): the seat blends its own domain vocabulary into
    // the query and filters the corpus to its discipline's subdirs, so a frontend
    // step draws frontend/design knowledge and a security step draws security KB.
    // Fail-open empty (or the plain instruction-keyed digest for an unknown seat).
    // `record_feedback = true`: this seat step's directive precedes a real doer
    // whose PASS/FAIL is consumed by `self_evolve::{reward_on_pass,penalise_on_fail}`
    // — so the surfaced chunks are snapshotted here for that outcome to attribute.
    let knowledge = crate::phases::seat_scoped_knowledge_digest(
        &options.project_root,
        role,
        instruction,
        4,
        true,
    );
    let mut directive = String::new();
    directive.push_str(crate::experts::agentic_engineering_rules());
    directive.push_str("\n\n");
    // Wave 3 (§3): inject the SEAT'S persona (its craft + remit) by role id — the
    // persona is a role capability the director injects per move, not a phase-bound
    // prompt. Fail-open: an unknown role yields "" → just the generic seat line.
    let persona = crate::experts::persona_for_role(role);
    if !persona.is_empty() {
        directive.push_str(persona);
        directive.push_str("\n\n");
    }
    // The seat's concrete WORKING METHOD — the specialist checklist / evaluation
    // criteria that makes the seat a differentiated discipline, not a renamed
    // persona line (frontend: contract-align fetch URLs + a11y + tokens; security:
    // authz / IDOR / injection / secrets; QA: test independence + real assertions).
    // Fail-open: an unknown seat yields "" → just the persona + task, as before.
    let method = crate::experts::seat_method(role);
    if !method.is_empty() {
        directive.push_str(method);
        directive.push_str("\n\n");
    }
    directive.push_str(&format!(
        "You are now wearing the {role} seat on this team. Implement ONLY the task \
         below — it is ONE scheduled step of a larger build, not the whole project. \
         Do it directly, with real files on disk: edit/create the files, run any \
         build/test you need, and report only what actually landed. Do NOT implement \
         other parts of the project in this turn — the rest is scheduled separately \
         and will fail its own acceptance if you build it now. Do not ask me and do \
         not just narrate; apply ONLY this step's work and STOP — end your turn as \
         soon as this step is done.\n\n## Your task\n{instruction}\n"
    ));
    let kd = knowledge.trim();
    if !kd.is_empty() {
        directive.push_str("\n## Relevant team experience\n");
        directive.push_str(kd);
        directive.push('\n');
    }
    // Change 1: this is a MID-RUN doer turn — defer the premature project-level wrap-up
    // (the base's "## Next steps" conclusion that streams the instant a step finishes,
    // before review + rework). The base still narrates its actions; the integrated final
    // report comes only after the whole build converges. See `wrapup_suppression_note`.
    directive.push_str(crate::director_loop::wrapup_suppression_note());
    directive
}

/// Run a single parallel seat over a read-only fork and return its verdict.
/// Reuses the exact fork → [`ForkConsult`] → critic mechanism `run_review_team`
/// uses, but for ONE director-chosen seat. The seat reviews the current on-disk
/// blackboard (quality surface) with `instruction` appended as the focus. A base
/// that can't fork / an unknown role → `None` (the caller fail-opens to accept).
/// An ADVERSARIAL seat (QA / security) under a host-scoped cold surface reviews
/// on a FRESH stateless one-shot instead — no doer transcript — with the fork as
/// its fail-open backup (see [`crate::critics::RoleCritic::cold`]).
async fn summon_parallel_seat(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    role: &str,
    instruction: &str,
) -> Option<RoleVerdict> {
    // Resolve the seat from the critic roster (the full 8-seat union) by id; an
    // unknown role has no craft to lend → bail to a fail-open accept.
    let critic = critic_for_role(role)?;
    // Open a read-only fork (bounded by a timeout so a wedged handshake can't
    // hang the director). A failed fork → ForkConsult fail-opens to accept.
    let fork = continuous::fork_with_timeout(session).await;
    // Read the quality-surface blackboard so the parallel seat reviews the real
    // delivered code + the deterministic floor (same surface the review team sees).
    let bb = continuous::Blackboard::read(options, ReviewKind::Quality);
    // Fold the director's focus instruction onto the requirement so the seat
    // reviews with that lens.
    let focused = if instruction.trim().is_empty() {
        options.requirement.clone()
    } else {
        format!("{}\n\n[director focus] {instruction}", options.requirement)
    };
    let arts = bb.artifacts(&focused);
    // COLD-context seat (B2#1): an ADVERSARIAL seat (`critic.cold()` — QA /
    // security) under a host-scoped fresh judge surface reviews on that stateless
    // one-shot (no doer transcript), with the fork above kept as its fail-open
    // backup. Every other seat — and every unscoped path — keeps the fork
    // byte-for-byte (today's behaviour).
    if let Some(surface) = crate::critics::cold_surface().filter(|_| critic.cold()) {
        let consult = continuous::ColdConsult::new(surface, continuous::ForkConsult::new(fork));
        let verdict = critic.review(&consult, arts).await;
        consult.end().await;
        return Some(verdict);
    }
    let consult = continuous::ForkConsult::new(fork);
    let verdict = critic.review(&consult, arts).await;
    consult.end().await;
    Some(verdict)
}

/// Resolve a critic seat for a role id from the full roster (the union of the
/// docs / preview / quality teams). Returns `None` for an unknown role so a
/// parallel summon of an unrecognised seat fail-opens to accept.
fn critic_for_role(role: &str) -> Option<Box<dyn crate::critics::RoleCritic>> {
    use crate::critics::{
        ArchitectureCritic, BackendCritic, DevOpsCritic, FrontendCritic, PmCritic, QaCritic,
        SecurityCritic, UiuxCritic,
    };
    let r = role.trim().to_ascii_lowercase();
    let seat: Box<dyn crate::critics::RoleCritic> = match r.as_str() {
        "product-manager" | "pm" | "product" => Box::new(PmCritic),
        "architect" | "architecture" | "tech-lead" => Box::new(ArchitectureCritic),
        "uiux-designer" | "uiux" | "designer" | "ui" | "ux" => Box::new(UiuxCritic),
        "frontend-engineer" | "frontend" | "fe" => Box::new(FrontendCritic),
        "backend-engineer" | "backend" | "be" => Box::new(BackendCritic),
        "qa-engineer" | "qa" => Box::new(QaCritic),
        "security-engineer" | "security" => Box::new(SecurityCritic),
        "devops-engineer" | "devops" | "sre" | "release" => Box::new(DevOpsCritic),
        _ => return None,
    };
    Some(seat)
}

// ===================================================================
// review — convene a parallel cross-review team
// ===================================================================

/// **Convene a cross-review team** — fork parallel reviewers and collect their
/// verdicts. The director calls this when it judges a slice warrants a second
/// pair of eyes; the team is scaled to the task ([`continuous::team_for`] →
/// the planner's complexity tiering), so a lean goal convenes no team and this
/// returns immediately (seats = 0, no blocking).
///
/// Reuses [`continuous::run_review_team`] verbatim: one read-only `fork()` per
/// seat, reviews run concurrently, verdicts recorded to the team ledger, the
/// deduped seat-tagged union of blocking findings returned. The findings are
/// ADVISORY — the director MAY fold them into a rework summon, but they never
/// drive loop termination (invariant 3).
///
/// Fail-open: a base that can't fork / an offline brain / a parse failure yields
/// accepting empty verdicts → no blocking → the director proceeds.
pub async fn review(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    kind: ReviewKind,
) -> ReviewResult {
    let team = continuous::team_for(kind, &options.requirement, &options.project_root);
    review_with_team(session, options, events, kind, team).await
}

/// **Convene a cross-review team sized from the ROUTE** — the team-by-route entry
/// (Wave 2 deliverable 3). Instead of re-deriving the team from the requirement
/// text, this seats the QUALITY-stage critics the router already chose for this turn
/// (`RoutePlan.team`), so team sizing is uniform on EVERY path, not just `/run`. An
/// empty route team (a lean/fast route) → no cross-review here (the floor stands).
///
/// Fail-open identically to [`review`]: a base that can't fork / an offline brain /
/// a parse failure yields accepting empty verdicts → no blocking → proceed.
///
/// User-defined seats (`.umadev/agents/*.md`) that apply to the QUALITY node join
/// here too, so a custom reviewer rides the route-sized path uniformly — appended
/// only when the route actually convened a built-in quality team (an empty route
/// team stays empty, so a lean/fast route convenes no custom seats either).
pub async fn review_with_seats(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    seats: &[crate::critics::Seat],
) -> ReviewResult {
    let mut team = crate::critics::quality_team_for_seats(seats);
    if !team.is_empty() {
        team.extend(crate::agents::custom_team_for(
            &options.project_root,
            ReviewKind::Quality,
        ));
    }
    review_with_team(session, options, events, ReviewKind::Quality, team).await
}

/// Shared body for [`review`] / [`review_with_seats`]: run ONE cross-review pass
/// over the given (already-sized) team and return its seat-tagged blocking union.
async fn review_with_team(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    kind: ReviewKind,
    team: Vec<Box<dyn crate::critics::RoleCritic>>,
) -> ReviewResult {
    let seats = team.len();
    if team.is_empty() {
        // Lean / no-UI / docs-only path: no cross-review here; the floor stands.
        return ReviewResult {
            seats: 0,
            blocking: Vec::new(),
        };
    }
    // Round 0 — a single review pass. The director decides whether to act on the
    // findings (e.g. summon a rework); this tool returns the facts, it does not
    // itself loop. (The bounded auto-rework loop stays in the pipeline's
    // `review_and_rework`; the director composes summon + review itself.)
    let blocking = continuous::run_review_team(session, options, events, kind, &team, 0).await;
    ReviewResult { seats, blocking }
}

// ===================================================================
// verify — run an objective reality check
// ===================================================================

/// **Run an objective check** — a deterministic reality probe, never an opinion.
/// The director calls this to confirm "is it actually done / correct" before it
/// reports a goal complete. Returns a FACTUAL [`VerifyResult`].
///
/// - [`VerifyKind::BuildTest`] runs the project's real build/test/lint via
///   [`crate::verify::run_verify`] and reports pass/fail per step.
/// - [`VerifyKind::Contract`] runs [`continuous::quality_floor`] — frontend↔
///   backend contract drift + requirement coverage gaps.
/// - [`VerifyKind::SourcePresent`] greps the workspace for real source files via
///   [`crate::acceptance::source_files`] — the "did anything get built" floor.
///
/// Fail-open: a check that genuinely can't run (no manifest / no architecture
/// doc) returns `available = false` (SKIPPED, neutral) — NOT a false failure.
pub async fn verify(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    kind: VerifyKind,
) -> VerifyResult {
    events.emit(EngineEvent::Note(format!(
        "team · verify {}",
        kind.as_str()
    )));
    match kind {
        VerifyKind::BuildTest => verify_build_test(options).await,
        VerifyKind::Contract => verify_contract(options),
        VerifyKind::SourcePresent => verify_source_present(options),
        VerifyKind::DesignTokensPresent => verify_design_tokens(options),
    }
}

/// Run the project's real build/test/lint and fold the per-step outcomes into a
/// factual result. An empty step list (no project manifest) → unavailable /
/// skipped (neutral). A failed, non-skipped step → `passed = false` with the
/// step name as evidence.
async fn verify_build_test(options: &RunOptions) -> VerifyResult {
    verify_build_test_raw(options).await.0
}

/// Cap on the RAW failing-log tail threaded into a rework directive: last lines…
const RAW_LOG_TAIL_LINES: usize = 60;
/// …and a character ceiling so the excerpt can never blow the directive budget
/// (the per-step capture in `crate::verify` is already ≤ 8 KiB per stream).
const RAW_LOG_TAIL_CHARS: usize = 4096;

/// [`verify_build_test`] plus a BOUNDED verbatim tail of the FIRST failing step's
/// raw build/test output (B1#2: a rework directive that carries the raw failure
/// evidence — not only UmaDev's one-line distillation — lets the brain adapt from
/// the compiler/test output itself). The floor already captured this output to
/// produce the diagnosis ([`crate::verify::VerifyOutcome::stdout`]/`stderr`); this
/// just threads a bounded slice (last [`RAW_LOG_TAIL_LINES`] lines, ≤
/// [`RAW_LOG_TAIL_CHARS`] chars) through instead of dropping it. `None` when
/// everything passed / was skipped / the failing step produced no output — the
/// caller skips the excerpt cleanly then.
pub(crate) async fn verify_build_test_raw(options: &RunOptions) -> (VerifyResult, Option<String>) {
    let outcomes = crate::verify::run_verify(&options.project_root).await;
    if outcomes.is_empty() {
        // No recognised project manifest → nothing to build. Neutral, not a fail.
        return (
            VerifyResult {
                available: false,
                passed: true,
                evidence: vec!["no project manifest — build/test skipped".to_string()],
            },
            None,
        );
    }
    let mut evidence = Vec::new();
    let mut passed = true;
    let mut raw: Option<String> = None;
    for o in &outcomes {
        if o.skipped {
            continue; // a step whose binary is absent is neutral
        }
        if o.passed {
            evidence.push(format!("{}: ok", o.step));
        } else {
            passed = false;
            evidence.push(format!("{}: FAILED (exit {})", o.step, o.exit_code));
            // Keep the FIRST failing step's raw tail (the root failure; later steps
            // often cascade from it). Bounded; empty output → no excerpt.
            if raw.is_none() {
                let combined = format!("{}\n{}", o.stdout.trim_end(), o.stderr.trim_end());
                let tail = bounded_log_tail(&combined);
                if !tail.trim().is_empty() {
                    raw = Some(format!(
                        "$ {}   (step `{}`, exit {})\n{tail}",
                        o.command, o.step, o.exit_code
                    ));
                }
            }
        }
    }
    (
        VerifyResult {
            available: true,
            passed,
            evidence,
        },
        raw,
    )
}

/// The last [`RAW_LOG_TAIL_LINES`] lines of `s`, additionally capped to the LAST
/// [`RAW_LOG_TAIL_CHARS`] characters (the end of a build/test log carries the
/// error). Pure + bounded; an empty input yields an empty string.
pub(crate) fn bounded_log_tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(RAW_LOG_TAIL_LINES);
    let tail = lines[start..].join("\n");
    let n = tail.chars().count();
    if n <= RAW_LOG_TAIL_CHARS {
        return tail;
    }
    tail.chars().skip(n - RAW_LOG_TAIL_CHARS).collect()
}

/// Run the contract + coverage floor and report drift as factual evidence. An
/// empty floor (no architecture doc / no gaps) → available + passed with no
/// evidence; any drift → `passed = false` with each finding as evidence.
fn verify_contract(options: &RunOptions) -> VerifyResult {
    let (qa_floor, _security_floor) = continuous::quality_floor(options);
    let lines: Vec<String> = qa_floor
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect();
    if lines.is_empty() {
        return VerifyResult {
            available: true,
            passed: true,
            evidence: Vec::new(),
        };
    }
    VerifyResult {
        available: true,
        passed: false,
        evidence: lines,
    }
}

/// Confirm real source files exist on disk — the cheapest objective reality
/// floor. Zero source = a build that claimed done produced nothing (`passed =
/// false`); any source = passed, with the file count as evidence.
///
/// **Document-aware** (the token-burn fix): a document task (a PRD / spec / design
/// doc / report — [`crate::planner::is_document_task`]) is EXPECTED to produce zero
/// source — its deliverable is the document, not code (this mirrors `phases.rs`'s
/// `expects_code`). So for a document task, zero source is a NEUTRAL pass (nothing to
/// verify), never a "no real source → create the code" blocker — which would otherwise
/// fabricate a pointless rework loop for every doc. A non-document task is unchanged:
/// zero source still fails. Fail-open: reading disk cannot error here.
fn verify_source_present(options: &RunOptions) -> VerifyResult {
    let files = crate::acceptance::source_files(&options.project_root);
    let n = files.len();
    if n == 0 && crate::planner::is_document_task(&options.requirement) {
        return VerifyResult {
            available: true,
            passed: true,
            evidence: vec![
                "document task — no source code expected (the deliverable is the document)"
                    .to_string(),
            ],
        };
    }
    VerifyResult {
        available: true,
        passed: n > 0,
        evidence: vec![format!("{n} source file(s) on disk")],
    }
}

/// Confirm the designer's `design-tokens.{json,css}` deliverable is a REAL file on
/// the blackboard — the designer seat's anti-theatre floor (a seat is only "done"
/// when it produced its artifact, never a narration). `available` is always `true`
/// (the check can always run — it just reads disk); `passed` iff at least one tokens
/// file exists. Absent ⇒ a factual reject the director folds into a rework directive
/// ("write the design system as real tokens"). Composes WITH, never replaces, the
/// always-on governance that blocks emoji-as-icon (UD-CODE-001) + hardcoded colors
/// (UD-CODE-002): tokens-present is existence, governance is quality. Fail-open:
/// reading disk cannot error here (a missing tree simply yields no files → reject).
fn verify_design_tokens(options: &RunOptions) -> VerifyResult {
    let files = crate::acceptance::design_tokens_files(&options.project_root);
    if files.is_empty() {
        return VerifyResult {
            available: true,
            passed: false,
            evidence: vec![
                "no design-tokens.{json,css} on the blackboard — the designer must write the \
                 design system as real token files (a type scale + color palette + spacing + \
                 the component list), not just describe it"
                    .to_string(),
            ],
        };
    }
    let names: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&options.project_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        })
        .collect();
    VerifyResult {
        available: true,
        passed: true,
        evidence: vec![format!(
            "design tokens on the blackboard: {}",
            names.join(", ")
        )],
    }
}

// ===================================================================
// checkpoint — stop and ask the user when the decision is theirs
// ===================================================================

/// **Pause for the user** when the director judges a decision is the user's to
/// make — bounded by the trust ladder. In [`crate::trust::TrustMode::Auto`] the
/// gate auto-proceeds (the user granted full autonomy); in
/// [`crate::trust::TrustMode::Guarded`] / [`crate::trust::TrustMode::Plan`] it
/// genuinely pauses, surfacing `question` and returning
/// [`CheckpointDecision::AskUser`] so the caller waits for the user's reply.
///
/// This is the SAME gate-pause policy the confirm gates use
/// ([`crate::trust::TrustMode::gates_auto_approve`]) — generalised so the director decides
/// *when* a checkpoint matters, while the tier still decides whether it actually
/// pauses. Records the pass to the trust ledger so the collaborative-trust
/// suggestion machinery keeps working.
///
/// Fail-open: this is a pure policy decision over the mode — it can't error.
#[must_use]
pub fn checkpoint(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    question: &str,
) -> CheckpointDecision {
    if options.mode.gates_auto_approve() {
        // Auto tier: the user pre-granted autonomy — record the auto-pass to the
        // trust ledger (so the suggestion machinery keeps tracking) and proceed.
        let mut ledger = crate::trust::TrustLedger::load(&options.project_root);
        let _ = ledger.record_pass("director_checkpoint");
        ledger.save(&options.project_root);
        return CheckpointDecision::AutoProceed;
    }
    // Guarded / plan: genuinely pause. Surface the question; the caller must wait
    // for the user before continuing (a revision resets the trust streak).
    events.emit(EngineEvent::Note(format!("team · checkpoint — {question}")));
    let mut ledger = crate::trust::TrustLedger::load(&options.project_root);
    ledger.record_revision("director_checkpoint");
    ledger.save(&options.project_root);
    CheckpointDecision::AskUser
}

// ===================================================================
// finalize — restore the shareable delivery on the DEFAULT path (Wave 4)
// ===================================================================

/// What [`finalize`] produced — the depth-gated delivery surface. Built fail-open:
/// every field is whatever actually landed; a degraded finalize still returns a
/// well-formed (possibly empty) result, never an error.
#[derive(Debug, Clone, Default)]
pub struct FinalizeResult {
    /// Whether a full delivery (proof-pack + scorecard + review report + security
    /// scan) was produced — `true` only on the deliberate path. A lean build gets
    /// just the core docs, so this stays `false`.
    pub proof_pack: bool,
    /// Workspace-relative names of the delivery artifacts finalize produced (the
    /// proof-pack, scorecard, review report, security scan, compliance mapping on
    /// the deliberate path). Empty when nothing was produced. Finalize NO LONGER
    /// fabricates core docs, so this never contains a retrospective template stub.
    pub artifacts: Vec<String>,
    /// Workspace-relative names of the core narrative docs (PRD / architecture /
    /// UI-UX) a DELIBERATE build did NOT produce — reported HONESTLY instead of
    /// backfilling a TODO-template stub that masquerades as a deliverable (and that
    /// fed the FR-coverage check fabricated `FR-` ids). Empty when every core doc
    /// was produced (or on a non-deliberate / no-op finalize).
    pub missing_docs: Vec<String>,
}

impl FinalizeResult {
    /// Whether finalize produced anything at all (any doc / proof-pack).
    #[must_use]
    pub fn produced_anything(&self) -> bool {
        !self.artifacts.is_empty()
    }
}

/// **Finalize a clean build into a shareable delivery** — the Wave 4 (§L4 / G8)
/// recovery of the delivery artifacts on the DEFAULT `/run` path, run ONCE after
/// QC settles clean. Assembles the shareable proof-pack + scorecard so a default
/// build leaves verifiable delivery evidence, not just source files.
///
/// **Depth-gated, so we never over-deliver:**
/// - **Lean / Fast** (a todo page, a quick edit) → the code IS the deliverable; no
///   doc ceremony, no zipped proof-pack + scorecard (that would be ceremony nobody
///   asked for). The owned `.umadev/plan.json` records what was built.
/// - **Deliberate (`Standard` / `Deep`)** → the FULL delivery
///   ([`crate::phases::run_delivery`]): compliance mapping + the owned + tool
///   security scan + the PR-ready review report + the zipped proof-pack + the
///   shareable HTML scorecard, over the docs the base actually produced.
///
/// **HONESTY — no fabricated docs.** Finalize does NOT backfill a TODO-template
/// stub for a missing PRD/architecture/UIUX (which would masquerade as a real
/// deliverable inside the proof pack AND feed the FR-coverage check fake `FR-`
/// ids). A genuinely-missing core doc is reported truthfully via
/// [`crate::phases::missing_core_docs`] (`FinalizeResult::missing_docs` + a Note +
/// a "not produced" scorecard row); the docs are made real up front by the
/// PM/architect plan step's `FileContains` evidence contract.
///
/// **Only on a Build route with real source on disk.** A chat / explain / a build
/// that produced no code gets nothing (there is nothing to deliver). The caller
/// passes the same `route` the loop drove off; a `None` route (the legacy entry)
/// → no finalize (backward-compatible — the old callers are unchanged).
///
/// **Fail-open by contract** (mirrors every other director tool): a failed
/// scaffold / a delivery writer that errors degrades to a `Note` + the partial
/// result, never an `Err`, never a panic — finalize must NEVER turn a build that
/// already succeeded into a failure. It writes only UmaDev's own `output/` +
/// `release/` artifacts (single-writer preserved: the main build is already done;
/// this is post-build bookkeeping, not a base turn).
pub fn finalize(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: Option<&RoutePlan>,
    clean: bool,
) -> FinalizeResult {
    // Only a real build with code on disk has anything to deliver. A None route
    // (legacy entry) is a no-op so existing callers are byte-for-byte unchanged.
    let Some(route) = route else {
        return FinalizeResult::default();
    };
    if route.class != RouteClass::Build {
        return FinalizeResult::default();
    }
    // Honesty floor: nothing was built ⇒ nothing to finalize (don't scaffold docs
    // around an empty tree and call it a delivery).
    if crate::acceptance::source_files(&options.project_root).is_empty() {
        return FinalizeResult::default();
    }
    // HONESTY (MEDIUM M2): finalize emits the shareable proof-pack + the delivery
    // scorecard, which READ as "the build shipped". Producing them for an INCOMPLETE
    // build (a blocked / stranded step, QC that never cleared) would disguise an
    // incomplete build as success — the exact failure the spec forbids
    // ("never disguise an incomplete build as success"). The plan-driven caller passes
    // `clean = every step reached Done`; the single-turn caller only reaches here from
    // inside `qc.is_clean()` (passes `true`). When NOT clean, withhold the delivery —
    // no proof-pack, no scorecard, no retrospective docs — and surface an honest Note.
    // Fail-open: returns the empty result, never an `Err`.
    if !clean {
        events.emit(EngineEvent::Note(
            "team · delivery — withheld: the build did not settle clean (incomplete / blocked \
             steps); no proof-pack or scorecard for an unfinished build"
                .to_string(),
        ));
        return FinalizeResult::default();
    }

    let mut result = FinalizeResult::default();

    // PROPORTIONALITY (audit #7): a lean/Fast build's deliverable IS the code. A
    // counter / single page does NOT need a retrospective PRD + architecture + UIUX
    // + execution-plan set — that is pure ceremony that also slows the fast path
    // (the owned `.umadev/plan.json` already records what was built). The full
    // shareable docs + proof-pack are reserved for a DELIBERATE (Standard/Deep)
    // build, where a real product genuinely warrants them.
    if !route.depth.is_deliberate() {
        events.emit(EngineEvent::Note(
            "team · delivery — lean build: the code is the deliverable (no doc ceremony; \
             see .umadev/plan.json for the plan)"
                .to_string(),
        ));
        return result;
    }

    // DELIBERATE: HONESTY (proof-pack) — do NOT backfill a TODO-template stub for a
    // missing PRD/architecture/UIUX. A retrospective template masquerading as a
    // deliverable is exactly the fabrication the spec forbids (and it fed the
    // FR-coverage check fake `FR-` ids, making it vacuous). Report any missing core
    // doc TRUTHFULLY instead; the docs the base actually produced ship as-is, and the
    // shareable scorecard marks the missing ones "未产出 · not produced". The doc is
    // made REAL up front by the PM/architect plan step's `FileContains` evidence
    // contract (see `plan_state::Plan::enforce_doc_evidence_floor`), not retrofitted
    // here. Fail-open: `missing_core_docs` only reads existence and never panics.
    let missing = crate::phases::missing_core_docs(options);
    if missing.is_empty() {
        events.emit(EngineEvent::Note(
            "team · delivery — core docs present (PRD / architecture / UI-UX produced \
             during the build)"
                .to_string(),
        ));
    } else {
        events.emit(EngineEvent::Note(format!(
            "team · delivery — {} core doc(s) NOT produced ({}); reported missing, not \
             fabricated",
            missing.len(),
            missing.join(", ")
        )));
    }
    result.missing_docs = missing;

    // … plus the full, shareable proof-pack + scorecard.
    {
        match crate::phases::run_delivery(options) {
            Ok(out) => {
                let rels: Vec<String> = out
                    .artifacts
                    .iter()
                    .map(|p| {
                        p.strip_prefix(&options.project_root)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .replace(std::path::MAIN_SEPARATOR, "/")
                    })
                    .collect();
                events.emit(EngineEvent::Note(format!(
                    "team · delivery — assembled the proof-pack + scorecard ({} artifact(s))",
                    rels.len()
                )));
                result.proof_pack = true;
                for r in rels {
                    if !result.artifacts.contains(&r) {
                        result.artifacts.push(r);
                    }
                }
            }
            Err(e) => {
                // Fail-open: a delivery-writer error never fails a clean build.
                events.emit(EngineEvent::Note(format!(
                    "team · delivery — proof-pack skipped (non-fatal: {e})"
                )));
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RecordingSink;
    use crate::trust::TrustMode;
    use umadev_runtime::{SessionError, SessionEvent, TurnStatus};

    /// Minimal RunOptions for a tempdir project.
    fn opts(root: &std::path::Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "做一个登录系统".to_string(),
            slug: "demo".to_string(),
            model: String::new(),
            backend: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: TrustMode::Guarded,
            strict_coverage: false,
        }
    }

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(RecordingSink::default())
    }

    #[test]
    fn summon_directive_carries_seat_method_and_is_differentiated_per_seat() {
        // A doer step's directive must carry the SEAT'S working method (a specialist
        // checklist), so two seats on the SAME instruction are differentiated
        // specialists, not one shared persona line. Isolate the corpus so the assert
        // is about the seat method wiring, not incidental staged-corpus recall.
        let _no_corpus = crate::test_support::NoBundledCorpus::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let instr = "build the account settings screen";
        let fe = summon_directive(&o, "frontend-engineer", instr);
        let sec = summon_directive(&o, "security-engineer", instr);
        // Frontend directive carries the frontend method (contract-align fetch +
        // design tokens) and NOT the security checklist.
        let fe_l = fe.to_lowercase();
        assert!(
            fe_l.contains("fetch") && fe_l.contains("token"),
            "frontend method: {fe}"
        );
        assert!(
            !fe_l.contains("idor"),
            "frontend directive is not the security one"
        );
        // Security directive carries the security checklist (authz / IDOR / injection).
        let sec_l = sec.to_lowercase();
        assert!(
            sec_l.contains("idor") && sec_l.contains("injection"),
            "security method: {sec}"
        );
        // Same instruction, DIFFERENT seat → DIFFERENT directive (seat drives it).
        assert_ne!(
            fe, sec,
            "the seat, not just the instruction, shapes the directive"
        );
        // Both still carry the concrete task + the instruction (unchanged behaviour).
        assert!(
            fe.contains(instr) && sec.contains(instr),
            "task + instruction preserved"
        );
        // Unknown seat: no persona, no method → still a valid non-empty directive
        // (fail-open) carrying the instruction, never a panic or empty directive.
        let unknown = summon_directive(&o, "astrologer", instr);
        assert!(
            !unknown.is_empty() && unknown.contains(instr),
            "unknown seat fails open"
        );
    }

    #[test]
    fn bounded_log_tail_keeps_the_last_lines_and_caps_chars() {
        // B1#2 bounding: the raw-failure excerpt is the LAST ~60 lines (the end of a
        // build/test log carries the error), additionally char-capped — never the
        // whole log, never the head.
        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let tail = bounded_log_tail(&long);
        assert!(
            tail.starts_with("line 140"),
            "keeps the LAST 60 lines: {tail}"
        );
        assert!(
            tail.ends_with("line 199"),
            "the final line survives: {tail}"
        );
        assert_eq!(tail.lines().count(), 60, "exactly the last 60 lines");
        // Char ceiling: a single huge line is cut to the trailing chars.
        let huge = "x".repeat(20_000);
        let cut = bounded_log_tail(&huge);
        assert_eq!(cut.chars().count(), 4096, "the char cap holds");
        // Small input passes through untouched; empty stays empty.
        assert_eq!(bounded_log_tail("a\nb"), "a\nb");
        assert_eq!(bounded_log_tail(""), "");
    }

    // ---- A scriptable fake BaseSession that records the directive it was driven
    // with and emits a chosen end status. Forks into a clone whose own turns
    // return a fixed JSON verdict (so a Parallel summon / review gets a verdict). --

    #[derive(Clone)]
    struct FakeSession {
        /// The end status the main turn reports.
        status: TurnStatus,
        /// Whether `fork()` succeeds.
        can_fork: bool,
        /// Text a forked judge turn emits (a strict-JSON verdict).
        fork_reply: String,
        /// Pending events to drain for the in-flight turn.
        queue: std::collections::VecDeque<SessionEvent>,
        /// Captures the last directive sent (so a test can assert role injection).
        last_directive: Arc<std::sync::Mutex<String>>,
        /// `true` once this is a forked (read-only) session.
        is_fork: bool,
    }

    impl FakeSession {
        fn new(status: TurnStatus, can_fork: bool, fork_reply: &str) -> Self {
            Self {
                status,
                can_fork,
                fork_reply: fork_reply.to_string(),
                queue: std::collections::VecDeque::new(),
                last_directive: Arc::new(std::sync::Mutex::new(String::new())),
                is_fork: false,
            }
        }
        fn directive_handle(&self) -> Arc<std::sync::Mutex<String>> {
            Arc::clone(&self.last_directive)
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for FakeSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            if !self.can_fork {
                return Err(SessionError::ForkUnsupported("test".into()));
            }
            let mut f = self.clone();
            f.is_fork = true;
            f.queue.clear();
            Ok(Box::new(f))
        }
        async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
            *self.last_directive.lock().unwrap() = directive;
            self.queue.clear();
            if self.is_fork {
                // A forked judge turn emits its JSON verdict then ends.
                self.queue
                    .push_back(SessionEvent::TextDelta(self.fork_reply.clone()));
                self.queue.push_back(SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                });
            } else {
                self.queue.push_back(SessionEvent::TurnDone {
                    status: self.status.clone(),
                    usage: None,
                });
            }
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            self.queue.pop_front()
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: umadev_runtime::ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn summon_serial_injects_role_and_drives_turn() {
        // A Serial summon injects the role into the directive and drives the main
        // session's turn to completion (done = true), single-writer.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut sess = FakeSession::new(TurnStatus::Completed, false, "");
        let captured = sess.directive_handle();
        let o = opts(tmp.path());
        let ev = sink();
        let r = summon(
            &mut sess,
            &o,
            &ev,
            "frontend-engineer",
            "build the login form",
            SummonMode::Serial,
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        assert_eq!(r.role, "frontend-engineer");
        assert!(r.done, "a completed serial turn is done");
        assert!(r.verdict.is_none(), "a serial doer returns no verdict");
        let directive = captured.lock().unwrap().clone();
        assert!(
            directive.contains("frontend-engineer"),
            "the role is injected into the directive"
        );
        assert!(
            directive.contains("build the login form"),
            "the instruction is carried"
        );
        // Wave 3: the seat's PERSONA (its craft + remit) is injected by role id, so
        // a summoned doer carries the same craft the fixed-phase persona did.
        assert!(
            directive.to_lowercase().contains("frontend engineer"),
            "the seat's persona is injected: {directive}"
        );
    }

    #[tokio::test]
    async fn summon_serial_failed_turn_is_fail_open_not_done() {
        // A failed/dead session → done = false (the director proceeds, never blocks).
        let tmp = tempfile::TempDir::new().unwrap();
        let mut sess = FakeSession::new(TurnStatus::Failed("boom".into()), false, "");
        let o = opts(tmp.path());
        let ev = sink();
        let r = summon(
            &mut sess,
            &o,
            &ev,
            "backend-engineer",
            "wire the API",
            SummonMode::Serial,
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        assert!(!r.done, "a failed turn is not done (fail-open)");
    }

    #[tokio::test]
    async fn summon_parallel_forks_a_seat_and_returns_verdict() {
        // A Parallel summon of a known seat forks a read-only session, runs the
        // seat's review, and returns its verdict.
        let tmp = tempfile::TempDir::new().unwrap();
        let reply = r#"{"accepts": false, "blocking": ["缺鉴权"], "evidence": ["api.ts"]}"#;
        let mut sess = FakeSession::new(TurnStatus::Completed, true, reply);
        let o = opts(tmp.path());
        let ev = sink();
        let r = summon(
            &mut sess,
            &o,
            &ev,
            "security-engineer",
            "audit the auth surface",
            SummonMode::Parallel,
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        assert!(r.done, "a forked seat that returned a verdict is done");
        let v = r.verdict.expect("parallel summon yields a verdict");
        assert_eq!(v.role, "security-engineer");
        assert!(!v.accepts);
        assert_eq!(v.blocking, vec!["缺鉴权".to_string()]);
    }

    #[tokio::test]
    async fn summon_parallel_unknown_role_fails_open_accept() {
        // An unknown parallel seat has no craft to lend → fail-open accepting verdict.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut sess = FakeSession::new(TurnStatus::Completed, true, "{}");
        let o = opts(tmp.path());
        let ev = sink();
        let r = summon(
            &mut sess,
            &o,
            &ev,
            "astrologer",
            "read the stars",
            SummonMode::Parallel,
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        let v = r.verdict.expect("always a verdict");
        assert!(v.accepts, "unknown seat fail-opens to accept");
        assert!(!r.done, "an unknown seat could not be summoned");
    }

    #[tokio::test]
    async fn summon_parallel_no_fork_fails_open_accept() {
        // A base that can't fork → fail-open accepting verdict, never blocks.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut sess = FakeSession::new(TurnStatus::Completed, false, "{}");
        let o = opts(tmp.path());
        let ev = sink();
        let r = summon(
            &mut sess,
            &o,
            &ev,
            "qa-engineer",
            "review the tests",
            SummonMode::Parallel,
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        let v = r.verdict.expect("always a verdict");
        // The fork failed → the seat's consult fail-opens to an accepting verdict.
        assert!(v.accepts, "a seat with no fork fail-opens to accept");
    }

    #[tokio::test]
    async fn review_collects_blocking_from_seats() {
        // A greenfield requirement convenes the quality team; the seats' blocking
        // findings come back deduped + seat-tagged.
        let tmp = tempfile::TempDir::new().unwrap();
        let reply = r#"{"accepts": false, "blocking": ["登录失败路径无测试"]}"#;
        let mut sess = FakeSession::new(TurnStatus::Completed, true, reply);
        let o = opts(tmp.path());
        let ev = sink();
        let r = review(&mut sess, &o, &ev, ReviewKind::Quality).await;
        assert!(r.seats > 0, "a greenfield goal convenes a quality team");
        assert!(r.has_blocking(), "the seats' blocking findings come back");
        assert!(
            r.blocking.iter().any(|b| b.contains("登录失败路径无测试")),
            "the finding is carried (seat-tagged)"
        );
    }

    #[tokio::test]
    async fn review_with_seats_sizes_the_team_from_the_route() {
        // Wave 2 deliverable 3: the cross-review team is built from the ROUTE's seats
        // (here a frontend + QA pair), not a re-derived requirement classification.
        // Both seats raise the scripted blocking finding → it comes back seat-tagged.
        let tmp = tempfile::TempDir::new().unwrap();
        let reply = r#"{"accepts": false, "blocking": ["按钮缺 loading 态"]}"#;
        let mut sess = FakeSession::new(TurnStatus::Completed, true, reply);
        let o = opts(tmp.path());
        let ev = sink();
        let r = director_review_with_seats(
            &mut sess,
            &o,
            &ev,
            &[
                crate::critics::Seat::FrontendEngineer,
                crate::critics::Seat::QaEngineer,
            ],
        )
        .await;
        assert_eq!(r.seats, 2, "exactly the two route seats reviewed");
        assert!(r.has_blocking());
        assert!(r.blocking.iter().any(|b| b.contains("按钮缺 loading 态")));
    }

    #[tokio::test]
    async fn review_with_seats_empty_team_is_no_review() {
        // An empty route team (a lean/fast route) → no cross-review, no blocking.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut sess = FakeSession::new(TurnStatus::Completed, true, "{}");
        let o = opts(tmp.path());
        let ev = sink();
        let r = director_review_with_seats(&mut sess, &o, &ev, &[]).await;
        assert_eq!(r.seats, 0);
        assert!(!r.has_blocking());
    }

    /// Local alias so the test reads naturally (the public fn lives at module root).
    async fn director_review_with_seats(
        session: &mut dyn BaseSession,
        options: &RunOptions,
        events: &Arc<dyn EventSink>,
        seats: &[crate::critics::Seat],
    ) -> ReviewResult {
        super::review_with_seats(session, options, events, seats).await
    }

    #[tokio::test]
    async fn review_no_fork_is_fail_open_no_blocking() {
        // A base that can't fork → every seat fail-opens to accept → no blocking.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut sess = FakeSession::new(TurnStatus::Completed, false, "{}");
        let o = opts(tmp.path());
        let ev = sink();
        let r = review(&mut sess, &o, &ev, ReviewKind::Quality).await;
        assert!(
            !r.has_blocking(),
            "no fork → fail-open accept → no blocking"
        );
    }

    #[tokio::test]
    async fn verify_source_present_reports_real_files() {
        // SourcePresent passes only when real source exists on disk.
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let ev = sink();
        // Empty project → no source → not passed.
        let r = verify(&o, &ev, VerifyKind::SourcePresent).await;
        assert!(r.available);
        assert!(!r.passed, "an empty project has no source");
        // Write a real source file → passes.
        std::fs::write(tmp.path().join("app.ts"), "export const x = 1;").unwrap();
        let r = verify(&o, &ev, VerifyKind::SourcePresent).await;
        assert!(r.passed, "a project with source passes");
        assert!(r.evidence[0].contains("source file"));
    }

    #[tokio::test]
    async fn verify_source_present_is_document_aware() {
        // The token-burn fix: a DOCUMENT task (a PRD / spec / design doc / report) is
        // EXPECTED to produce zero source — its deliverable is the document, not code.
        // So zero source for a document task is a NEUTRAL pass, never the "no real
        // source → create the code" blocker that fabricated a rework loop for docs.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "帮我写一份产品需求文档(PRD)".to_string();
        let ev = sink();
        let r = verify(&o, &ev, VerifyKind::SourcePresent).await;
        assert!(r.available);
        assert!(
            r.passed,
            "a document task with zero source is a neutral pass, not a blocker"
        );
        assert!(r.evidence[0].contains("document task"));
        // A NON-document task with zero source still fails (a real build that produced
        // nothing is the decisive blocking finding — unchanged).
        let mut o2 = opts(tmp.path());
        o2.requirement = "做一个待办事项应用".to_string();
        let r2 = verify(&o2, &ev, VerifyKind::SourcePresent).await;
        assert!(
            !r2.passed,
            "a real product build with zero source still fails"
        );
    }

    #[tokio::test]
    async fn verify_design_tokens_present_passes_only_with_a_real_tokens_file() {
        // The designer seat's anti-theatre floor: DesignTokensPresent is available
        // (it always can read disk) and passes ONLY when a real design-tokens file
        // exists — absent ⇒ a factual reject (NOT an unavailable skip), so a designer
        // that narrated "I designed the system" but wrote no tokens is caught.
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let ev = sink();
        // No tokens file → available + NOT passed (a real reject, fail-open evidence).
        let r = verify(&o, &ev, VerifyKind::DesignTokensPresent).await;
        assert!(r.available, "the tokens check can always run (reads disk)");
        assert!(!r.passed, "no design-tokens file → reject");
        assert!(r.evidence[0].contains("design-tokens"));
        // Write a real design-tokens.css → passes, with the file named as evidence.
        std::fs::create_dir_all(tmp.path().join("src/styles")).unwrap();
        std::fs::write(
            tmp.path().join("src/styles/design-tokens.css"),
            ":root{--color-bg:#0b0b0c;--font-scale-1:0.75rem}",
        )
        .unwrap();
        let r = verify(&o, &ev, VerifyKind::DesignTokensPresent).await;
        assert!(r.passed, "a real design-tokens file passes");
        assert!(r.evidence[0].contains("design-tokens.css"));
    }

    #[tokio::test]
    async fn verify_build_test_no_manifest_is_unavailable_not_failure() {
        // No project manifest → the build/test check is SKIPPED (neutral), NOT a
        // false failure (fail-open).
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let ev = sink();
        let r = verify(&o, &ev, VerifyKind::BuildTest).await;
        assert!(!r.available, "no manifest → unavailable");
        assert!(r.passed, "an unavailable check is neutral, not a failure");
    }

    #[tokio::test]
    async fn verify_contract_no_arch_doc_passes_clean() {
        // No architecture doc / no gaps → the contract floor is empty → passes.
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let ev = sink();
        let r = verify(&o, &ev, VerifyKind::Contract).await;
        assert!(r.available);
        assert!(r.passed, "an empty contract floor passes");
        assert!(r.evidence.is_empty());
    }

    #[test]
    fn checkpoint_auto_proceeds_in_auto_pauses_in_guarded() {
        // The trust ladder governs whether a checkpoint pauses: auto proceeds,
        // guarded / plan genuinely ask the user.
        let tmp = tempfile::TempDir::new().unwrap();
        let ev = sink();
        let mut o = opts(tmp.path());

        o.mode = TrustMode::Auto;
        assert_eq!(
            checkpoint(&o, &ev, "ship to prod?"),
            CheckpointDecision::AutoProceed,
            "auto tier proceeds without asking"
        );

        o.mode = TrustMode::Guarded;
        assert_eq!(
            checkpoint(&o, &ev, "ship to prod?"),
            CheckpointDecision::AskUser,
            "guarded tier pauses for the user"
        );

        o.mode = TrustMode::Plan;
        assert_eq!(
            checkpoint(&o, &ev, "ship to prod?"),
            CheckpointDecision::AskUser,
            "plan tier pauses for the user"
        );
    }

    #[test]
    fn critic_for_role_resolves_known_seats_and_aliases() {
        assert!(critic_for_role("frontend-engineer").is_some());
        assert!(critic_for_role("frontend").is_some(), "alias resolves");
        assert!(critic_for_role("SECURITY").is_some(), "case-insensitive");
        assert!(critic_for_role("unknown-seat").is_none());
    }

    // ── Wave 4: finalize — depth-gated delivery on the default path ──────────

    /// A Build route at the given depth, with a real build team.
    fn build_route(depth: crate::router::Depth) -> RoutePlan {
        RoutePlan {
            class: RouteClass::Build,
            kind: crate::planner::TaskKind::Greenfield,
            depth,
            team: vec![crate::critics::Seat::FrontendEngineer],
            scope: vec![],
            needs_clarify: None,
            est_budget: crate::router::Budget::for_route(RouteClass::Build, depth),
            confidence: 0.7,
        }
    }

    /// Seed a real source file so the source-present honesty floor passes.
    fn seed_source(root: &std::path::Path) {
        std::fs::write(root.join("app.ts"), "export const x = 1;").unwrap();
    }

    #[test]
    fn finalize_lean_build_ships_code_only_no_doc_ceremony() {
        // PROPORTIONALITY (audit #7): a LEAN/Fast Build's deliverable IS the code —
        // NO retrospective PRD/architecture/uiux/execution-plan set, NO proof-pack.
        // A counter / single page should not produce 4 enterprise docs.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let ev = sink();
        let o = opts(tmp.path());
        let route = build_route(crate::router::Depth::Fast);
        let r = finalize(&o, &ev, Some(&route), true);
        assert!(!r.proof_pack, "a lean build earns no proof-pack");
        // NO scaffolded core docs — the code is the deliverable.
        for name in ["demo-prd.md", "demo-architecture.md", "demo-uiux.md"] {
            assert!(
                !tmp.path().join("output").join(name).is_file(),
                "{name} must NOT be scaffolded for a lean build"
            );
        }
        // And no proof-pack zip in release/.
        let release = tmp.path().join("release");
        assert!(
            !release.exists()
                || std::fs::read_dir(&release)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "a lean build produces no release/ proof-pack"
        );
    }

    #[test]
    fn finalize_deliberate_build_assembles_the_full_proof_pack() {
        // A DELIBERATE (Standard/Deep) Build leaves the full shareable delivery —
        // core docs AND the zipped proof-pack + scorecard in release/.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let ev = sink();
        let o = opts(tmp.path());
        let route = build_route(crate::router::Depth::Standard);
        let r = finalize(&o, &ev, Some(&route), true);
        assert!(r.proof_pack, "a deliberate build assembles the proof-pack");
        // A proof-pack zip landed in release/.
        let release = tmp.path().join("release");
        let has_zip = std::fs::read_dir(&release)
            .map(|d| {
                d.flatten()
                    .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("zip"))
            })
            .unwrap_or(false);
        assert!(has_zip, "the proof-pack zip was assembled");
    }

    #[test]
    fn finalize_does_not_clobber_a_real_doc_the_base_wrote() {
        // Idempotent: a doc the base already wrote (a real architecture table) is
        // left untouched — finalize only backfills the MISSING ones.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        let arch = tmp.path().join("output").join("demo-architecture.md");
        std::fs::write(&arch, "# REAL architecture written by the base").unwrap();
        let ev = sink();
        let o = opts(tmp.path());
        let route = build_route(crate::router::Depth::Fast);
        finalize(&o, &ev, Some(&route), true);
        let after = std::fs::read_to_string(&arch).unwrap();
        assert!(
            after.contains("REAL architecture written by the base"),
            "the base's real doc must not be clobbered: {after}"
        );
    }

    #[test]
    fn finalize_is_a_noop_with_no_source_or_no_route_or_a_chat_route() {
        let ev = sink();
        // No route (legacy entry) → no-op.
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        assert!(!finalize(&o, &ev, None, true).produced_anything());
        // A Build route but an EMPTY tree (nothing built) → no-op (don't scaffold
        // docs around a build that produced nothing).
        let route = build_route(crate::router::Depth::Standard);
        assert!(
            !finalize(&o, &ev, Some(&route), true).produced_anything(),
            "no source → nothing to deliver"
        );
        // A non-Build (chat/explain) route with source → no-op (nothing to ship).
        seed_source(tmp.path());
        let mut chat = build_route(crate::router::Depth::Fast);
        chat.class = RouteClass::Chat;
        assert!(
            !finalize(&o, &ev, Some(&chat), true).produced_anything(),
            "a chat route delivers nothing"
        );
    }

    #[test]
    fn finalize_withholds_delivery_for_an_incomplete_build() {
        // MEDIUM M2: a deliberate Build with real source on disk but NOT clean
        // (blocked / stranded steps → `clean == false`) must NOT emit a proof-pack or
        // a delivery scorecard — an incomplete build must never be disguised as
        // success. With `clean == false` finalize produces nothing.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let ev = sink();
        let o = opts(tmp.path());
        let route = build_route(crate::router::Depth::Standard);
        let r = finalize(&o, &ev, Some(&route), false);
        assert!(!r.proof_pack, "an incomplete build earns no proof-pack");
        assert!(
            !r.produced_anything(),
            "an unclean build delivers nothing: {r:?}"
        );
        // No proof-pack zip in release/, no scaffolded core docs.
        let release = tmp.path().join("release");
        assert!(
            !release.exists()
                || std::fs::read_dir(&release)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "an incomplete build produces no release/ proof-pack"
        );
        for name in ["demo-prd.md", "demo-architecture.md", "demo-uiux.md"] {
            assert!(
                !tmp.path().join("output").join(name).is_file(),
                "{name} must NOT be scaffolded for an incomplete build"
            );
        }
    }

    #[test]
    fn finalize_deliberate_missing_docs_are_reported_missing_not_fabricated() {
        // HONESTY: a clean DELIBERATE build whose base did NOT produce the core docs must
        // NOT get a TODO-template stub backfilled (a fake deliverable that also fed the
        // FR-coverage check fabricated FR- ids). Finalize reports them MISSING truthfully,
        // still assembles the proof pack (the build IS clean), and the scorecard marks the
        // docs "not produced".
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path()); // real code on disk, but no output/*.md docs
        let ev = sink();
        let o = opts(tmp.path());
        let route = build_route(crate::router::Depth::Standard);
        let r = finalize(&o, &ev, Some(&route), true);

        // 1. No fabricated stub on disk: none of the core docs exist (nothing was written).
        for name in ["demo-prd.md", "demo-architecture.md", "demo-uiux.md"] {
            assert!(
                !tmp.path().join("output").join(name).is_file(),
                "{name} must NOT be fabricated — a missing doc is reported, not stubbed"
            );
        }
        // 2. FR-coverage is not fed a fabricated stub: with no PRD on disk there are no
        //    fake FR- ids to (mis)report — coverage sees nothing declared.
        assert!(
            crate::coverage::uncovered_requirements(tmp.path(), "demo").is_empty(),
            "no fabricated PRD ⇒ no phantom FR- requirements for coverage to chew on"
        );
        // 3. The result reports the three core docs as missing, honestly.
        for name in [
            "output/demo-prd.md",
            "output/demo-architecture.md",
            "output/demo-uiux.md",
        ] {
            assert!(
                r.missing_docs.iter().any(|m| m == name),
                "missing_docs must name {name}: {:?}",
                r.missing_docs
            );
        }
        // 4. The build is clean, so the proof pack + scorecard still ship (not fabricated,
        //    not withheld) — and the scorecard tells the truth about the missing docs.
        assert!(
            r.proof_pack,
            "a clean deliberate build still assembles the proof-pack"
        );
        let release = tmp.path().join("release");
        let scorecard = std::fs::read_dir(&release)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                let is_scorecard = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("scorecard-"));
                let is_html = p.extension().and_then(|e| e.to_str()) == Some("html");
                is_scorecard && is_html
            })
            .expect("a scorecard was written");
        let html = std::fs::read_to_string(&scorecard).unwrap();
        assert!(
            html.contains("not produced"),
            "the scorecard reports the missing docs as 'not produced'"
        );
    }

    #[test]
    fn finalize_reports_only_the_absent_docs_and_never_fabricates_a_real_one() {
        // A DELIBERATE build where the base produced a REAL PRD but not the architecture /
        // UI-UX docs: finalize leaves the real PRD untouched (no clobber), reports ONLY the
        // two absent docs as missing, and fabricates nothing. Fail-open: a partial doc set
        // never panics and never turns a clean build into a failure.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        let prd = tmp.path().join("output").join("demo-prd.md");
        std::fs::write(&prd, "# REAL PRD\n\n| FR-001 | login | P0 |\n").unwrap();
        let ev = sink();
        let o = opts(tmp.path());
        let route = build_route(crate::router::Depth::Standard);
        let r = finalize(&o, &ev, Some(&route), true);
        // The base's real PRD is never clobbered.
        assert!(
            std::fs::read_to_string(&prd).unwrap().contains("REAL PRD"),
            "finalize must not clobber the base's real PRD"
        );
        // Only the two genuinely-absent docs are reported missing (the PRD is NOT).
        assert!(
            !r.missing_docs.iter().any(|m| m == "output/demo-prd.md"),
            "a produced PRD is not reported missing: {:?}",
            r.missing_docs
        );
        assert!(
            r.missing_docs
                .iter()
                .any(|m| m == "output/demo-architecture.md")
                && r.missing_docs.iter().any(|m| m == "output/demo-uiux.md"),
            "the two absent docs are reported missing: {:?}",
            r.missing_docs
        );
        assert!(r.proof_pack, "a clean build still delivers (not failed)");
    }
}
