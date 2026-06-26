//! Owned, visible plan (Wave 1, L2) — UmaDev's "planning" primitive.
//!
//! Today the plan lives invisibly in the base's head and the phase bar sits frozen.
//! This module gives UmaDev a [`Plan`] data structure it OWNS: a dependency DAG of
//! [`PlanStep`]s, each with a machine-checkable [`AcceptanceSpec`] (so "done" is a
//! deterministic fact, not vibes), persisted to `.umadev/plan.json` and surfaced as
//! live events. The plan is SYNTHESISED by borrowing the base's brain for one forked
//! strict-JSON turn (cloned from the proven intake / critic consult pattern) — UmaDev
//! owns no model and performs no cognition itself.
//!
//! ## Wave 1 scope
//!
//! This wave **synthesises, persists, and displays** the plan; it does NOT yet drive
//! the build step-by-step off it (the existing director build loop still executes,
//! emitting progress events). Driving the plan via `summon` is Wave 2. Keeping the
//! scope here narrow is deliberate.
//!
//! ## Invariants (mirror `router.rs` / `critics.rs`)
//!
//! 1. **Fail-open.** [`synthesize_plan`] returns `None` on any failure (offline /
//!    no fork / timeout / unparseable) — the caller falls back to today's
//!    single-turn behaviour. Persistence is best-effort; a failed write is logged-
//!    nowhere and ignored, never an error that blocks the host.
//! 2. **No new endpoint.** The planning consult runs over the SAME borrowed brain +
//!    its `fork()`; no extra model, no API key.
//! 3. **Read-only synthesis.** The planning turn runs on an isolated read-only fork;
//!    it never touches the main writer session.
//! 4. **UmaDev owns the artifact.** The parsed [`Plan`] is UmaDev's typed data — the
//!    base produced JSON, UmaDev validated + normalised + owns it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use umadev_runtime::BaseSession;

use crate::critics::Seat;
use crate::router::RoutePlan;
use crate::runner::RunOptions;

/// What kind of work a step is — a doing step mutates the workspace (driven serially
/// on the main session under the run-lock); a review step is read-only judgement
/// (runs on a fork). The director uses this to decide HOW to drive the step (Wave 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    /// The step builds / changes real files (a doer drives the main session).
    Build,
    /// The step reviews the artifacts (a critic runs on a read-only fork).
    Review,
}

impl StepKind {
    /// Tolerant parse of a brain-supplied kind string; defaults to [`Self::Build`].
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "review" | "verify" | "check" | "qa" => Self::Review,
            _ => Self::Build,
        }
    }
}

/// The lifecycle state of one plan step. The plan is steerable + resumable, so the
/// status is persisted with the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    /// Not started; its dependencies may or may not be satisfied yet.
    Pending,
    /// Currently being worked.
    Active,
    /// Finished and accepted (its [`AcceptanceSpec`] is satisfied).
    Done,
    /// Cannot proceed (a dependency failed / an acceptance check can't be met).
    Blocked,
}

impl StepStatus {
    /// Stable lowercase id for events / logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

/// The mechanical "done" criterion for a step — what UmaDev deterministically checks
/// to flip the step to [`StepStatus::Done`], rather than trusting a narrated claim.
/// Maps to the existing objective verify kinds ([`crate::director::VerifyKind`]) so
/// the director reuses real checkers in Wave 2, never a new gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcceptanceSpec {
    /// Real source files for this step actually exist on disk (the honesty floor —
    /// `VerifyKind::SourcePresent`).
    SourcePresent,
    /// The project's real build/test/lint passes (`VerifyKind::BuildTest`).
    BuildTest,
    /// The frontend↔backend API contract + requirement coverage holds
    /// (`VerifyKind::Contract`).
    Contract,
    /// A review step is accepted by its reviewing seat (no blocking verdict).
    ReviewClean,
    /// No machine criterion — accepted when its work turn settles. The weakest
    /// criterion; used when the brain names nothing checkable. (Still bounded by the
    /// surrounding loop; never a free pass to ship.)
    TurnSettled,
}

impl AcceptanceSpec {
    /// Tolerant parse of a brain-supplied acceptance string; defaults to the
    /// honesty floor ([`Self::SourcePresent`]) for a build step — the safest
    /// non-trivial criterion when the brain is vague.
    fn parse(s: &str, kind: StepKind) -> Self {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '_'], "-")
            .as_str()
        {
            "source-present" | "source" | "files-exist" | "files" => Self::SourcePresent,
            "build-test" | "build" | "test" | "tests" | "lint" => Self::BuildTest,
            "contract" | "api-contract" | "api" => Self::Contract,
            "review-clean" | "review" | "accepted" => Self::ReviewClean,
            "turn-settled" | "none" | "" => {
                if kind == StepKind::Review {
                    Self::ReviewClean
                } else {
                    Self::SourcePresent
                }
            }
            _ => {
                if kind == StepKind::Review {
                    Self::ReviewClean
                } else {
                    Self::SourcePresent
                }
            }
        }
    }
}

/// One node in the plan DAG. Owns its dependencies (`depends_on`) so independent
/// nodes are parallelisable and the director can schedule by readiness, not a flat
/// list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Stable id within the plan (e.g. `scaffold`, `auth-route`). Referenced by
    /// other steps' `depends_on`.
    pub id: String,
    /// Human-readable title shown in the checklist.
    pub title: String,
    /// The seat responsible for this step (a doer for Build, a reviewer for Review).
    pub seat: Seat,
    /// Whether this step builds or reviews.
    pub kind: StepKind,
    /// Ids of steps that must be [`StepStatus::Done`] before this one is ready.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// The mechanical criterion that flips this step to done.
    pub acceptance: AcceptanceSpec,
    /// Lifecycle status (persisted, so the plan resumes).
    pub status: StepStatus,
}

/// UmaDev's owned plan for a build — a DAG of steps plus the brain's surfaced risks
/// and open questions. Serialised to `.umadev/plan.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The ordered step nodes (order is the suggested drive order; `depends_on`
    /// is the authoritative readiness constraint).
    pub steps: Vec<PlanStep>,
    /// Risks the brain flagged for this build (advisory; surfaced to the user).
    #[serde(default)]
    pub risks: Vec<String>,
    /// Open questions the brain wants resolved (advisory).
    #[serde(default)]
    pub open_questions: Vec<String>,
}

impl Plan {
    /// A compact one-line-per-step summary for the [`crate::events::EngineEvent::PlanPosted`]
    /// card — `id · title (seat)`.
    #[must_use]
    pub fn step_summaries(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|s| format!("{} · {} ({})", s.id, s.title, s.seat.role_id()))
            .collect()
    }

    /// The steps whose dependencies are ALL [`StepStatus::Done`] and which are not
    /// themselves finished/blocked — the set the director may drive next. A step
    /// with an unknown dependency id is treated as not-ready (conservative).
    #[must_use]
    pub fn ready_steps(&self) -> Vec<&PlanStep> {
        let done: HashSet<&str> = self
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Done)
            .map(|s| s.id.as_str())
            .collect();
        self.steps
            .iter()
            .filter(|s| matches!(s.status, StepStatus::Pending))
            .filter(|s| s.depends_on.iter().all(|d| done.contains(d.as_str())))
            .collect()
    }

    /// Set a step's status by id, returning `true` if the id was found. No-op +
    /// `false` for an unknown id (fail-open).
    pub fn mark(&mut self, id: &str, status: StepStatus) -> bool {
        for s in &mut self.steps {
            if s.id == id {
                s.status = status;
                return true;
            }
        }
        false
    }

    /// `done / total` progress for the checklist header.
    #[must_use]
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Done)
            .count();
        (done, self.steps.len())
    }

    /// Normalise a freshly-parsed plan: drop empty-id steps, dedupe ids, drop
    /// `depends_on` entries that reference a non-existent step (so the DAG is
    /// self-consistent and `ready_steps` can't deadlock on a dangling dep), then
    /// break any dependency CYCLES (so the DAG is acyclic and `ready_steps` can
    /// always make progress — a cyclic `a → b → a` would otherwise leave both steps
    /// permanently un-ready, a silent deadlock). Returns `None` if nothing usable
    /// survives (the caller then fail-opens to no plan).
    fn normalized(mut self) -> Option<Self> {
        let mut seen: HashSet<String> = HashSet::new();
        self.steps.retain(|s| {
            let id = s.id.trim();
            !id.is_empty() && seen.insert(id.to_string())
        });
        if self.steps.is_empty() {
            return None;
        }
        let ids: HashSet<String> = self.steps.iter().map(|s| s.id.clone()).collect();
        for s in &mut self.steps {
            s.id = s.id.trim().to_string();
            s.title = s.title.trim().to_string();
            // Trim each dep and drop self-edges + dangling refs to non-existent steps.
            s.depends_on = s
                .depends_on
                .iter()
                .map(|d| d.trim().to_string())
                .filter(|d| d != &s.id && ids.contains(d))
                .collect();
            // A fresh plan starts every step Pending regardless of what the brain
            // emitted — the director drives status from reality, not the brain's
            // optimistic claim.
            s.status = StepStatus::Pending;
        }
        self.break_dependency_cycles();
        self.risks.retain(|r| !r.trim().is_empty());
        self.open_questions.retain(|q| !q.trim().is_empty());
        Some(self)
    }

    /// Detect and break dependency cycles via DFS. A back-edge (a dep that points to
    /// a step currently on the DFS stack) closes a cycle; that single `depends_on`
    /// entry is dropped so the step still runs — it just loses the cyclic ordering
    /// constraint that could never be satisfied. Each broken edge is logged
    /// (`tracing::warn!`), never silently swallowed. Assumes self-edges and dangling
    /// deps have already been stripped (see [`Self::normalized`]).
    fn break_dependency_cycles(&mut self) {
        // Index steps by id → position so we can look up neighbours quickly.
        let index: std::collections::HashMap<String, usize> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), i))
            .collect();

        // Iterative DFS with explicit colours: 0 = white (unseen), 1 = grey (on the
        // current stack), 2 = black (fully explored). A grey target = a back-edge.
        let n = self.steps.len();
        let mut colour = vec![0u8; n];
        // Edges to drop, collected as (step_index, dep_id) — applied after the walk so
        // we don't mutate `depends_on` while iterating it.
        let mut to_drop: Vec<(usize, String)> = Vec::new();

        for root in 0..n {
            if colour[root] != 0 {
                continue;
            }
            // Stack frame: (node, next-dep-cursor).
            let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
            colour[root] = 1;
            while let Some(&(node, cursor)) = stack.last() {
                if cursor >= self.steps[node].depends_on.len() {
                    colour[node] = 2;
                    stack.pop();
                    continue;
                }
                // Advance the cursor for this frame before recursing.
                stack.last_mut().unwrap().1 += 1;
                let dep_id = self.steps[node].depends_on[cursor].clone();
                let Some(&target) = index.get(&dep_id) else {
                    continue; // dangling deps were already stripped; defensive.
                };
                match colour[target] {
                    1 => {
                        // Back-edge → this dep closes a cycle. Drop it.
                        tracing::warn!(
                            step = %self.steps[node].id,
                            depends_on = %dep_id,
                            "plan: dropping cyclic dependency edge to keep the DAG acyclic"
                        );
                        to_drop.push((node, dep_id));
                    }
                    0 => {
                        colour[target] = 1;
                        stack.push((target, 0));
                    }
                    _ => {} // black: fully explored, no cycle through it.
                }
            }
        }

        for (i, dep) in to_drop {
            self.steps[i].depends_on.retain(|d| d != &dep);
        }
    }
}

/// The relative path of the persisted plan under the project root.
#[must_use]
pub fn plan_rel_path() -> PathBuf {
    PathBuf::from(".umadev").join("plan.json")
}

/// Persist a plan to `.umadev/plan.json` (atomic: write a temp sibling, then
/// rename). Best-effort + fail-open: any IO error is returned for the caller to
/// ignore — a failed persist never blocks the build. Returns `Ok(path)` on success.
pub fn save(plan: &Plan, root: &Path) -> std::io::Result<PathBuf> {
    let dir = root.join(".umadev");
    std::fs::create_dir_all(&dir)?;
    let final_path = dir.join("plan.json");
    let json = serde_json::to_string_pretty(plan)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Atomic write: temp sibling on the SAME dir (so rename is atomic), then rename.
    let tmp = dir.join(format!("plan.json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

/// Load the persisted plan from `.umadev/plan.json`, or `None` when absent /
/// unreadable / unparseable (fail-open — a corrupt plan is treated as "no plan").
#[must_use]
pub fn load(root: &Path) -> Option<Plan> {
    let path = root.join(".umadev").join("plan.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Plan>(&text).ok()
}

/// The brain's raw plan reply — tolerant so a partial / sloppy JSON still parses.
#[derive(Debug, Clone, Default, Deserialize)]
struct BrainPlan {
    #[serde(default)]
    steps: Vec<BrainStep>,
    #[serde(default)]
    risks: Vec<String>,
    #[serde(default)]
    open_questions: Vec<String>,
}

/// One raw step from the brain — every field tolerant (a missing seat / acceptance
/// is filled deterministically during normalisation).
#[derive(Debug, Clone, Default, Deserialize)]
struct BrainStep {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    seat: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    acceptance: String,
}

/// Synthesise a [`Plan`] by borrowing the base's brain for ONE forked, read-only,
/// strict-JSON planning turn — cloned from the proven intake / critic consult
/// pattern. The brain decomposes the requirement into a DAG of steps with seats +
/// machine-checkable acceptance; UmaDev parses, normalises, and OWNS the result.
///
/// `route` seeds the brain with UmaDev's already-decided class/kind/depth + team so
/// the plan matches the route (a fast quick-edit gets a tiny plan; a deep build gets
/// a real DAG).
///
/// **Fail-open by contract:** any failure — offline brain, no fork, timeout,
/// unparseable reply, or an empty plan after normalisation — returns `None`, and the
/// caller falls back to today's single-turn build behaviour. Never errors, never
/// blocks.
/// Send ONE directive on the MAIN session and collect its full text reply (for
/// JSON parsing). Bounded by a generous idle timeout per event; non-text events
/// (an unexpected tool call / result on a JSON-only turn) are ignored.
///
/// A pending [`SessionEvent::NeedApproval`] is **answered with `Deny`** (and, if the
/// `respond` itself fails, the turn is `interrupt()`ed) rather than left dangling:
/// this is a JSON-only PLAN turn that must mutate nothing, and an unanswered
/// approval would wedge the base waiting on a decision — poisoning this same shared
/// session for the later fallback build. Cleanly denying ends the turn and leaves
/// the session usable. Fail-open: a dead session / a timeout / an empty reply →
/// `None` (the caller then runs the plain build on the still-usable session).
async fn drain_plan_turn(
    session: &mut dyn BaseSession,
    directive: String,
    deadline: std::time::Instant,
) -> Option<String> {
    use umadev_runtime::{ApprovalDecision, SessionEvent};
    if session.send_turn(directive).await.is_err() {
        return None;
    }
    let mut text = String::new();
    loop {
        // LOW #5: bound each event wait by the SHARED run deadline (capped at the
        // original 180s per-event idle window). The planning turn can never run past
        // the whole-build budget, so planning time is attributed to the run clock.
        // A spent budget yields a zero wait → the turn settles immediately (fail-open:
        // a partial/empty reply degrades to `None`, i.e. the plain single-turn build).
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let wait = remaining.min(std::time::Duration::from_secs(180));
        match tokio::time::timeout(wait, session.next_event()).await {
            Ok(Some(SessionEvent::TextDelta(t))) => text.push_str(&t),
            Ok(Some(SessionEvent::TurnDone { .. })) => break,
            // The plan turn forbids tools; an approval request means the base tried to
            // act anyway. DENY it (best-effort) so the JSON-only turn ends cleanly and
            // the shared session stays usable for the fallback build. If `respond`
            // fails, interrupt the turn to un-wedge the session, then bail.
            Ok(Some(SessionEvent::NeedApproval { req_id, .. })) => {
                if session
                    .respond(&req_id, ApprovalDecision::Deny)
                    .await
                    .is_err()
                {
                    let _ = session.interrupt().await;
                    return None;
                }
            }
            // A JSON-only plan turn should emit no other tools; ignore anything else
            // and let the next-event timeout bound a misbehaving turn.
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return None,
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Ask the borrowed brain to decompose the requirement into an owned [`Plan`] DAG.
/// Runs as the session's first (JSON-only) turn; fail-open to `None` on any
/// failure so the caller falls back to the plain single-turn build.
pub async fn synthesize_plan(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    requirement: &str,
    route: &RoutePlan,
    // LOW #5: the SHARED run deadline. The planning drain is bounded by it so the
    // whole deliberate build (planning + build) shares one clock instead of the old
    // fixed 180s-per-event timeout that left planning unattributed to the budget.
    deadline: std::time::Instant,
) -> Option<Plan> {
    let _ = options; // reserved (model/trust already live on the session)
    let team: Vec<&str> = route.team.iter().map(|s| s.role_id()).collect();
    let team_line = if team.is_empty() {
        "(no standing team — keep the plan minimal)".to_string()
    } else {
        team.join(", ")
    };
    let system = format!(
        "You are a senior engineering director turning ONE requirement into a concrete, \
         buildable PLAN before any work starts. Decompose it into a SMALL dependency DAG of \
         steps (typically 3-8; fewer for a small change). Each step names the responsible \
         seat, whether it BUILDS or REVIEWS, its dependencies (by step id), and a MECHANICAL \
         acceptance criterion UmaDev can check deterministically. \
         Routing context — class={class}, kind={kind}, depth={depth}, team=[{team_line}]. \
         Keep the plan proportional to that depth. \
         `seat`: one of product-manager, architect, uiux-designer, frontend-engineer, \
         backend-engineer, qa-engineer, security-engineer, devops-engineer. \
         `kind`: build | review. \
         `acceptance`: source-present | build-test | contract | review-clean. \
         JSON shape: {{\"steps\":[{{\"id\":\"scaffold\",\"title\":\"…\",\"seat\":\"…\",\
         \"kind\":\"build\",\"depends_on\":[],\"acceptance\":\"source-present\"}}],\
         \"risks\":[\"…\"],\"open_questions\":[\"…\"]}}",
        class = route.class.as_str(),
        kind = route.kind.id(),
        depth = route.depth.as_str(),
    );
    let user = format!("Requirement:\n{requirement}");

    // Run the planning turn on the MAIN session — NOT a fork. claude cannot
    // `--resume` a session that has not had its first turn yet, so a pre-build
    // planning FORK fails silently and the user never sees a plan. Running it here
    // makes planning the session's FIRST turn: reliable, it establishes the session
    // so later QC forks work, and the base keeps the plan in its own context when it
    // then builds. JSON-only, tools forbidden this turn.
    let directive = format!(
        "{system}\n\nReturn EXACTLY ONE JSON object and nothing else — no markdown, \
         no code fence, no prose. Do NOT write any files or run any commands in this \
         turn; this is the PLAN only.\n\n{user}"
    );
    let text = drain_plan_turn(session, directive, deadline).await?;
    let json = crate::continuous::extract_json_object(&text)?;
    let raw: BrainPlan = serde_json::from_str(&json).ok()?;
    let plan = Plan {
        steps: raw
            .steps
            .into_iter()
            .map(|b| {
                let kind = StepKind::parse(&b.kind);
                PlanStep {
                    id: b.id,
                    title: b.title,
                    // An unknown / missing seat fails open to a sensible default by
                    // step kind (build→frontend doer, review→QA) so a vague brain
                    // reply still yields an assignable step.
                    seat: Seat::from_alias(&b.seat).unwrap_or(match kind {
                        StepKind::Review => Seat::QaEngineer,
                        StepKind::Build => Seat::FrontendEngineer,
                    }),
                    kind,
                    depends_on: b.depends_on,
                    acceptance: AcceptanceSpec::parse(&b.acceptance, kind),
                    status: StepStatus::Pending,
                }
            })
            .collect(),
        risks: raw.risks,
        open_questions: raw.open_questions,
    };
    plan.normalized()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, deps: &[&str]) -> PlanStep {
        PlanStep {
            id: id.to_string(),
            title: format!("step {id}"),
            seat: Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
            acceptance: AcceptanceSpec::SourcePresent,
            status: StepStatus::Pending,
        }
    }

    fn plan(steps: Vec<PlanStep>) -> Plan {
        Plan {
            steps,
            risks: vec![],
            open_questions: vec![],
        }
    }

    #[test]
    fn ready_steps_respects_the_dag() {
        let mut p = plan(vec![
            step("a", &[]),
            step("b", &["a"]),
            step("c", &["a", "b"]),
        ]);
        // Only `a` (no deps) is ready initially.
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["a"]);
        // Finishing `a` unblocks `b` only (c still waits on b).
        assert!(p.mark("a", StepStatus::Done));
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["b"]);
        // Finishing `b` unblocks `c`.
        assert!(p.mark("b", StepStatus::Done));
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["c"]);
    }

    #[test]
    fn mark_unknown_id_is_a_noop() {
        let mut p = plan(vec![step("a", &[])]);
        assert!(!p.mark("nope", StepStatus::Done));
        assert_eq!(p.steps[0].status, StepStatus::Pending);
    }

    #[test]
    fn progress_counts_done_steps() {
        let mut p = plan(vec![step("a", &[]), step("b", &[])]);
        assert_eq!(p.progress(), (0, 2));
        p.mark("a", StepStatus::Done);
        assert_eq!(p.progress(), (1, 2));
    }

    #[test]
    fn normalize_drops_dangling_deps_and_empty_ids() {
        let p = Plan {
            steps: vec![
                step("a", &["ghost"]), // ghost dep dropped
                step("", &[]),         // empty id dropped
                step("a", &[]),        // duplicate id dropped
                step("b", &["a"]),
            ],
            risks: vec![String::new(), "real risk".to_string()],
            open_questions: vec![],
        }
        .normalized()
        .expect("a usable plan survives");
        // `` and the duplicate `a` are gone → a, b.
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.steps[0].id, "a");
        // The dangling `ghost` dep was stripped, so `a` is ready immediately.
        assert!(p.steps[0].depends_on.is_empty());
        // Empty risk dropped.
        assert_eq!(p.risks, vec!["real risk".to_string()]);
        // After normalisation `a` (no real deps) is ready; the DAG is consistent.
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["a"]);
    }

    #[test]
    fn normalize_returns_none_when_nothing_usable() {
        let p = plan(vec![step("", &[])]).normalized();
        assert!(p.is_none());
    }

    #[test]
    fn normalize_breaks_a_two_node_cycle() {
        // LOW #2: a → b → a is a cycle; left intact, `ready_steps` would NEVER surface
        // either step (silent deadlock). Normalisation must break the back-edge so the
        // DAG is acyclic and at least one step becomes ready.
        let p = plan(vec![step("a", &["b"]), step("b", &["a"])])
            .normalized()
            .expect("a usable plan survives");
        assert_eq!(p.steps.len(), 2, "no step is dropped, only an edge");
        // Exactly one back-edge is dropped, so one of the two becomes ready.
        let ready = p.ready_steps();
        assert!(
            !ready.is_empty(),
            "breaking the cycle must leave at least one step ready: {:?}",
            p.steps
        );
        // The DAG is now acyclic: total surviving deps across both nodes is at most 1.
        let total_deps: usize = p.steps.iter().map(|s| s.depends_on.len()).sum();
        assert!(
            total_deps <= 1,
            "the cyclic edge was broken: {total_deps} deps left"
        );
    }

    #[test]
    fn normalize_breaks_a_three_node_cycle_but_keeps_acyclic_edges() {
        // a → b → c → a is a 3-cycle, plus a legit acyclic edge d → a. The cycle is
        // broken; the acyclic d → a edge survives.
        let p = plan(vec![
            step("a", &["c"]),
            step("b", &["a"]),
            step("c", &["b"]),
            step("d", &["a"]),
        ])
        .normalized()
        .expect("a usable plan survives");
        assert_eq!(p.steps.len(), 4);
        // The whole graph must be schedulable: repeatedly mark ready steps Done until
        // none remain pending; if a cycle survived this would loop without draining.
        let mut p = p;
        let mut guard = 0;
        loop {
            let ready: Vec<String> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
            if ready.is_empty() {
                break;
            }
            for id in ready {
                p.mark(&id, StepStatus::Done);
            }
            guard += 1;
            assert!(guard < 10, "the DAG must drain (no surviving cycle)");
        }
        let (done, total) = p.progress();
        assert_eq!(
            done, total,
            "every step became reachable → the DAG is acyclic"
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("umadev-plan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = plan(vec![step("a", &[]), step("b", &["a"])]);
        let path = save(&p, &dir).expect("save ok");
        assert!(path.exists());
        let loaded = load(&dir).expect("load ok");
        assert_eq!(loaded, p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_is_none() {
        let dir = std::env::temp_dir().join(format!("umadev-plan-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&dir).is_none());
    }

    // ── drain_plan_turn cleanly handles a mid-turn approval (MEDIUM #3) ──

    /// A minimal scripted [`BaseSession`] for `drain_plan_turn` tests: it replays a
    /// fixed event batch after `send_turn`, records approval replies + interrupts, and
    /// can be told to FAIL `respond` (to exercise the interrupt fallback).
    struct ScriptedSession {
        events: std::collections::VecDeque<umadev_runtime::SessionEvent>,
        responded:
            std::sync::Arc<std::sync::Mutex<Vec<(String, umadev_runtime::ApprovalDecision)>>>,
        interrupts: std::sync::Arc<std::sync::Mutex<usize>>,
        respond_fails: bool,
    }

    impl ScriptedSession {
        fn new(events: Vec<umadev_runtime::SessionEvent>, respond_fails: bool) -> Self {
            Self {
                events: events.into_iter().collect(),
                responded: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                interrupts: std::sync::Arc::new(std::sync::Mutex::new(0)),
                respond_fails,
            }
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for ScriptedSession {
        async fn send_turn(
            &mut self,
            _directive: String,
        ) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<umadev_runtime::SessionEvent> {
            self.events.pop_front()
        }
        async fn respond(
            &mut self,
            req_id: &str,
            decision: umadev_runtime::ApprovalDecision,
        ) -> Result<(), umadev_runtime::SessionError> {
            self.responded
                .lock()
                .unwrap()
                .push((req_id.to_string(), decision));
            if self.respond_fails {
                Err(umadev_runtime::SessionError::Send(
                    "scripted respond failure".into(),
                ))
            } else {
                Ok(())
            }
        }
        async fn interrupt(&mut self) -> Result<(), umadev_runtime::SessionError> {
            *self.interrupts.lock().unwrap() += 1;
            Ok(())
        }
        async fn end(&mut self) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn drain_plan_turn_denies_an_approval_and_finishes_without_wedging() {
        use umadev_runtime::{ApprovalDecision, SessionEvent, TurnStatus};
        // The plan turn forbids tools, but the base asks to act anyway. drain must DENY
        // the approval (not ignore it → wedge) and still drain to the JSON reply.
        let mut s = ScriptedSession::new(
            vec![
                SessionEvent::NeedApproval {
                    req_id: "req-1".into(),
                    action: "write".into(),
                    target: "app.rs".into(),
                },
                SessionEvent::TextDelta("{\"steps\":[]}".into()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ],
            false,
        );
        let responded = std::sync::Arc::clone(&s.responded);
        let out = drain_plan_turn(
            &mut s,
            "plan please".into(),
            std::time::Instant::now() + std::time::Duration::from_secs(3_600),
        )
        .await;
        assert_eq!(
            out.as_deref(),
            Some("{\"steps\":[]}"),
            "drained the JSON reply"
        );
        let replies = responded.lock().unwrap();
        assert_eq!(
            replies.len(),
            1,
            "the approval was answered, not left dangling"
        );
        assert_eq!(replies[0], ("req-1".to_string(), ApprovalDecision::Deny));
    }

    #[tokio::test]
    async fn drain_plan_turn_interrupts_when_respond_fails() {
        use umadev_runtime::SessionEvent;
        // If `respond` itself fails, drain must interrupt() to un-wedge the shared
        // session, then bail (None) so the caller runs the plain build on a live
        // session rather than one stuck waiting on an approval.
        let mut s = ScriptedSession::new(
            vec![SessionEvent::NeedApproval {
                req_id: "req-1".into(),
                action: "write".into(),
                target: "app.rs".into(),
            }],
            true, // respond fails
        );
        let interrupts = std::sync::Arc::clone(&s.interrupts);
        let out = drain_plan_turn(
            &mut s,
            "plan please".into(),
            std::time::Instant::now() + std::time::Duration::from_secs(3_600),
        )
        .await;
        assert!(out.is_none(), "a failed respond bails out fail-open");
        assert_eq!(
            *interrupts.lock().unwrap(),
            1,
            "the session was interrupted to un-wedge it for the fallback build"
        );
    }

    /// A session whose `next_event` never resolves (the base hangs holding the pipe
    /// open) — used to prove the LOW #5 deadline bound actually settles the drain.
    struct HangingPlanSession;

    #[async_trait::async_trait]
    impl BaseSession for HangingPlanSession {
        async fn send_turn(
            &mut self,
            _directive: String,
        ) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<umadev_runtime::SessionEvent> {
            std::future::pending::<()>().await;
            None
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: umadev_runtime::ApprovalDecision,
        ) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), umadev_runtime::SessionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn drain_plan_turn_is_bounded_by_the_shared_run_deadline() {
        // LOW #5: the planning drain is bounded by the SHARED run deadline (capped at
        // the per-event idle window), so planning shares the build's one clock instead
        // of its own fixed 180s timeout. An ALREADY-SPENT deadline must settle the
        // drain immediately (a near-zero wait → None), never block on a hung base for
        // the full 180s. We assert it returns promptly under a generous test ceiling.
        let mut s = HangingPlanSession;
        let spent = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap_or_else(std::time::Instant::now);
        let settled = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            drain_plan_turn(&mut s, "plan please".into(), spent),
        )
        .await
        .expect("the drain must settle on the spent deadline, not block 180s");
        assert!(
            settled.is_none(),
            "a hung base under a spent deadline settles fail-open to None (the plain single-turn build)"
        );
    }

    #[test]
    fn acceptance_and_stepkind_parse_tolerantly() {
        assert_eq!(StepKind::parse("review"), StepKind::Review);
        assert_eq!(StepKind::parse("anything"), StepKind::Build);
        assert_eq!(
            AcceptanceSpec::parse("build-test", StepKind::Build),
            AcceptanceSpec::BuildTest
        );
        // A vague acceptance on a build step falls back to the honesty floor.
        assert_eq!(
            AcceptanceSpec::parse("???", StepKind::Build),
            AcceptanceSpec::SourcePresent
        );
        // A vague acceptance on a review step falls back to review-clean.
        assert_eq!(
            AcceptanceSpec::parse("", StepKind::Review),
            AcceptanceSpec::ReviewClean
        );
    }
}
