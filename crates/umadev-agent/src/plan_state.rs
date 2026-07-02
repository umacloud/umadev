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
    /// The designer's **design-tokens** deliverable (`design-tokens.{json,css}`)
    /// is a REAL file on the blackboard (`VerifyKind::DesignTokensPresent`) — the
    /// designer seat's anti-theatre floor: a design system the team can IMPORT, not
    /// just a narrated claim. Composes with the always-on governance that blocks
    /// emoji-as-icon + hardcoded colors (existence here, quality there).
    DesignTokensPresent,
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
            "design-tokens-present" | "design-tokens" | "design-system" | "tokens" => {
                Self::DesignTokensPresent
            }
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

    /// Whether this acceptance criterion is a STRONGER bar than the bare
    /// source-present / turn-settled floor — i.e. it names a real, checkable outcome
    /// (a green build/test, a matching FE↔BE contract, the design-tokens deliverable,
    /// a clean review) rather than merely "some source exists". The per-seat evidence
    /// floor reads this to tell an already-contracted step (leave it) from an
    /// under-specified one (augment it).
    fn is_strong(&self) -> bool {
        !matches!(self, Self::SourcePresent | Self::TurnSettled)
    }
}

/// A TYPED EVIDENCE CONTRACT for one plan step — an explicit, machine-checkable
/// declaration of *what concrete evidence proves the step is done*, so "done" is
/// **falsifiable per step** instead of trusted from the base's "looks done"
/// narration. This is the deterministic-subset companion to [`AcceptanceSpec`]:
/// where `AcceptanceSpec` selects a coarse WORKSPACE-level check (does ANY source
/// exist / is the WHOLE build green), an `EvidenceContract` names the SPECIFIC
/// artifact a step must produce — *this* file, containing *this* string, *this*
/// named test passing, *this* route answering. Every variant maps to a check
/// UmaDev can run on its existing deterministic floor (no new probing infra):
///
/// - [`Self::FileExists`] / [`Self::FileContains`] — a named path on disk (and,
///   for `FileContains`, a substring it must hold). The most direct falsifiable
///   proof a concrete deliverable landed.
/// - [`Self::SourcePresent`] — real source files exist (the honesty floor;
///   reuses [`crate::acceptance::source_files`] via `VerifyKind::SourcePresent`).
/// - [`Self::BuildClean`] — the project's real build/test/lint is green (reuses
///   `VerifyKind::BuildTest`).
/// - [`Self::TestPasses`] — a NAMED test is present in the codebase AND the test
///   floor is green (reuses the build/test floor + a bounded source scan). A
///   `None` name degrades to "the suite passes".
/// - [`Self::RouteResponds`] — an HTTP route answers with the expected status
///   (reuses [`crate::runtime_proof`] — boot the app + probe the route).
/// - [`Self::ContractMatches`] — the frontend↔backend API contract holds (reuses
///   `umadev-contract` via `VerifyKind::Contract`).
///
/// **UmaDev parses + OWNS the contract; the base never self-grades.** The brain
/// PROPOSES per-step evidence in the plan JSON; UmaDev validates it into this typed
/// form (dropping anything unparseable), then the verifier checks it on the floor
/// before a step ticks [`StepStatus::Done`]. An unsatisfied contract leaves the step
/// not-done and folds the typed gap into the rework directive. Where the brain
/// supplies none, the step falls back to its [`AcceptanceSpec`] (fail-open — a
/// missing/uncheckable contract never blocks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EvidenceContract {
    /// Real source files exist on disk (the honesty floor).
    SourcePresent,
    /// The project's real build/test/lint passes.
    BuildClean,
    /// The frontend↔backend API contract + requirement coverage holds.
    ContractMatches,
    /// A named file exists on disk (relative to the project root).
    FileExists {
        /// Repo-relative path that must exist.
        path: String,
    },
    /// A named file exists AND contains a substring needle (e.g. a route literal,
    /// an exported symbol) — proof the file holds the specific thing it should.
    FileContains {
        /// Repo-relative path that must exist.
        path: String,
        /// Substring the file's contents must contain.
        needle: String,
    },
    /// A named test is present in the codebase and the test floor is green. A
    /// `None` name means "the test suite passes" (no specific test named).
    TestPasses {
        /// Substring of the test's name/path that must appear in source; `None`
        /// degrades the check to "the suite passes".
        #[serde(default)]
        name: Option<String>,
    },
    /// An HTTP route answers with the expected status (probed via the runtime
    /// proof — boot the app + `curl` the path). `status == None` means "any non-error
    /// (`< 400`) response"; `Some(code)` requires that EXACT status — so a required
    /// error status (e.g. `401` for an unauthenticated probe) is expressible, which
    /// the old `u16` `0`-sentinel conflated with "unspecified". `method` is recorded
    /// for the declaration/gap text; the reused probe transport is path+status based.
    RouteResponds {
        /// HTTP method (recorded for the contract/gap text), e.g. `GET`.
        method: String,
        /// Route path relative to the base URL, e.g. `/api/login`.
        path: String,
        /// Expected HTTP status; `None` = any non-error (`< 400`) response, `Some(c)`
        /// = require exactly status `c` (including a required error code like `401`).
        #[serde(default)]
        status: Option<u16>,
    },
    /// A brain-declared evidence entry that named a KNOWN contract kind but was
    /// UNDER-SPECIFIED (e.g. `{"kind":"file-exists","path":""}` — a missing/empty
    /// required field). M6: such an entry is NOT silently dropped (which would let the
    /// step fall back to the coarse `AcceptanceSpec` default — "accepted because ANY
    /// source exists" — defeating the point of naming a specific file). It is retained
    /// as an explicit GAP the verifier always reports unsatisfied, so the step is held
    /// to a falsifiable bar. (A genuinely-unrecognised kind is still dropped.)
    Malformed {
        /// Why the declared evidence could not be formed (which kind, which field).
        detail: String,
    },
}

impl EvidenceContract {
    /// A short human label of this contract, for the rework directive so the doer
    /// knows the exact mechanical bar this step must clear.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::SourcePresent => "real source files exist on disk".to_string(),
            Self::BuildClean => "the build/test is clean".to_string(),
            Self::ContractMatches => "the frontend↔backend API contract holds".to_string(),
            Self::FileExists { path } => format!("`{path}` exists"),
            Self::FileContains { path, needle } => format!("`{path}` contains \"{needle}\""),
            Self::TestPasses { name } => match name {
                Some(n) => format!("test \"{n}\" is present and passing"),
                None => "the test suite passes".to_string(),
            },
            Self::RouteResponds {
                method,
                path,
                status,
            } => match status {
                None => format!("{method} {path} responds OK"),
                Some(s) => format!("{method} {path} responds {s}"),
            },
            Self::Malformed { detail } => {
                format!("a required evidence contract is under-specified ({detail})")
            }
        }
    }

    /// Tolerantly build ONE contract from a brain-supplied JSON value — either a
    /// bareword string (`"build-clean"`, `"source-present"`, `"contract-matches"`)
    /// or an object `{"kind": "...", ...}` with the variant's fields. Returns
    /// `None` for anything unrecognised or under-specified (an empty path, a
    /// `file-contains` with no needle) so the entry is dropped rather than poisoning
    /// the whole plan parse (fail-open). Field reads are tolerant: a missing field
    /// is empty, a numeric-or-string status is accepted, the method is upper-cased.
    fn parse_value(v: &serde_json::Value) -> Option<Self> {
        // Bareword form: a plain string names a no-argument contract.
        if let Some(s) = v.as_str() {
            return Self::parse_kind_only(s);
        }
        let obj = v.as_object()?;
        let kind = obj
            .get("kind")
            .or_else(|| obj.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '_'], "-");
        let str_field = |k: &str| -> String {
            obj.get(k)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        match kind.as_str() {
            "source-present" | "source" | "files-exist" => Some(Self::SourcePresent),
            "build-clean" | "build-test" | "build" | "test" | "tests" | "lint" => {
                Some(Self::BuildClean)
            }
            "contract-matches" | "contract" | "api-contract" | "api" => Some(Self::ContractMatches),
            "file-exists" | "file" => {
                let path = str_field("path");
                // M6: a recognised kind with a missing required field is a GAP, not a
                // silent drop (which would degrade the step to the coarse default).
                if path.is_empty() {
                    Some(Self::malformed("file-exists", "missing a non-empty `path`"))
                } else {
                    Some(Self::FileExists { path })
                }
            }
            "file-contains" | "contains" => {
                let path = str_field("path");
                let needle = {
                    let n = str_field("needle");
                    if n.is_empty() {
                        str_field("contains")
                    } else {
                        n
                    }
                };
                if path.is_empty() || needle.is_empty() {
                    Some(Self::malformed(
                        "file-contains",
                        "requires a non-empty `path` and `needle`",
                    ))
                } else {
                    Some(Self::FileContains { path, needle })
                }
            }
            "test-passes" | "test-present" | "named-test" => {
                let n = str_field("name");
                Some(Self::TestPasses {
                    name: (!n.is_empty()).then_some(n),
                })
            }
            "route-responds" | "route" | "endpoint" | "http" => {
                let path = str_field("path");
                if path.is_empty() {
                    return Some(Self::malformed(
                        "route-responds",
                        "missing a non-empty `path`",
                    ));
                }
                let method = {
                    let m = str_field("method").to_ascii_uppercase();
                    if m.is_empty() {
                        "GET".to_string()
                    } else {
                        m
                    }
                };
                Some(Self::RouteResponds {
                    method,
                    path,
                    status: value_to_status(obj.get("status")),
                })
            }
            _ => None,
        }
    }

    /// Build a [`Self::Malformed`] gap from a recognised-but-under-specified entry.
    fn malformed(kind: &str, why: &str) -> Self {
        Self::Malformed {
            detail: format!("{kind}: {why}"),
        }
    }

    /// Map a bareword kind string to a no-argument contract; `None` for a kind that
    /// needs fields (a `file-exists` cannot be formed from a bare word).
    fn parse_kind_only(s: &str) -> Option<Self> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '_'], "-")
            .as_str()
        {
            "source-present" | "source" | "files-exist" => Some(Self::SourcePresent),
            "build-clean" | "build-test" | "build" | "test" | "tests" | "lint" => {
                Some(Self::BuildClean)
            }
            "contract-matches" | "contract" | "api-contract" | "api" => Some(Self::ContractMatches),
            "test-passes" => Some(Self::TestPasses { name: None }),
            _ => None,
        }
    }

    /// Whether this contract is a STRONGER falsifiable bar than the bare
    /// source-present honesty floor — i.e. it names a specific checkable fact (a named
    /// file/route/test, a green build, a matching contract, or a retained
    /// [`Self::Malformed`] held-gap) beyond "some source exists". Only
    /// [`Self::SourcePresent`] is non-strong. The per-seat evidence floor uses this to
    /// decide whether a step already carries a real per-step contract.
    fn is_strong(&self) -> bool {
        !matches!(self, Self::SourcePresent)
    }
}

/// Read a JSON status value as an `Option<u16>`, accepting both a number (`200`) and
/// a string (`"200"`); anything else / absent / a non-status `0` → `None` (interpreted
/// as "any non-error response"). Clamps an out-of-range number to `u16::MAX`.
/// Fail-open, never panics. L2: an `Option` makes "unspecified" and a required status
/// type-distinct, so a required error code like `401` is no longer conflated with the
/// old `0` "any" sentinel.
fn value_to_status(v: Option<&serde_json::Value>) -> Option<u16> {
    let raw = match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|x| x.min(u64::from(u16::MAX)) as u16),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u16>().ok(),
        _ => None,
    };
    // `0` is not a real HTTP status — treat it as "unspecified" (any non-error).
    raw.filter(|c| *c != 0)
}

/// Parse + normalise a brain-supplied list of evidence values into owned typed
/// contracts: drop a genuinely-unrecognised entry, but RETAIN a recognised-kind entry
/// that was under-specified as an explicit [`EvidenceContract::Malformed`] GAP (M6 —
/// so it cannot silently degrade the step to the coarse [`AcceptanceSpec`] default),
/// then dedupe (preserving first-seen order). An empty result means the step carries
/// NO typed contract and will fall back to its [`AcceptanceSpec`] at verify time
/// (fail-open).
fn parse_brain_evidence(values: &[serde_json::Value]) -> Vec<EvidenceContract> {
    let mut out: Vec<EvidenceContract> = Vec::new();
    for v in values {
        if let Some(c) = EvidenceContract::parse_value(v) {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// Push `c` onto a step's evidence list only when an EQUAL contract is not already
/// present (dedupe, preserving first-seen order) — so a per-seat / falsifiability
/// augmentation never duplicates a contract the brain volunteered.
fn push_unique(evidence: &mut Vec<EvidenceContract>, c: EvidenceContract) {
    if !evidence.contains(&c) {
        evidence.push(c);
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
    /// The step's TYPED EVIDENCE CONTRACT(s) — explicit, machine-checkable proof
    /// the step is done (a file exists/contains X, a named test passes, a route
    /// responds, the contract matches). When non-empty, the verifier checks THESE
    /// specific facts on the deterministic floor before the step ticks
    /// [`StepStatus::Done`]; an unsatisfied contract leaves the step not-done and
    /// folds the typed gap into rework. Empty (the brain named nothing checkable,
    /// or a persisted plan predates this field) ⇒ fall back to `acceptance`
    /// (fail-open). `#[serde(default)]` so older `plan.json` files still load.
    #[serde(default)]
    pub evidence: Vec<EvidenceContract>,
    /// Lifecycle status (persisted, so the plan resumes).
    pub status: StepStatus,
}

impl PlanStep {
    /// Whether this step already carries a falsifiable contract stronger than the bare
    /// source-present floor — via a strong typed [`EvidenceContract`] OR a non-trivial
    /// [`AcceptanceSpec`]. A BUILD step that does NOT is "under-specified":
    /// [`Plan::enforce_seat_evidence_floor`] augments it with a seat-appropriate default
    /// (it never touches a step that already has one, so a brain-volunteered contract is
    /// never removed or downgraded).
    fn has_strong_contract(&self) -> bool {
        self.acceptance.is_strong() || self.evidence.iter().any(EvidenceContract::is_strong)
    }
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

    /// Each step's CURRENT status id, index-aligned with [`Self::step_summaries`]
    /// — carried on the [`crate::events::EngineEvent::PlanPosted`] card so a
    /// cross-session RESUME re-post renders the persisted truth (already-`done`
    /// steps checked) instead of resetting the checklist to all-pending.
    #[must_use]
    pub fn step_statuses(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|s| s.status.as_str().to_string())
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

    /// The **blast radius** of a step: the number of OTHER steps that transitively
    /// depend on it — the size of its downstream cone in the dependency DAG. An
    /// UPSTREAM node (a schema, an API contract, the scaffold) that many later steps
    /// build on has a LARGE blast radius: if it is wrong, everything downstream must be
    /// unwound, so it is the most expensive place to be wrong. A leaf nobody depends on
    /// has radius 0. The director reads this to verify the high-blast-radius work
    /// FIRST / most rigorously and to prioritise reworking the highest-blast-radius
    /// blocking step (its fix may obviate the downstream rework).
    ///
    /// Computed as reverse-reachability over `depends_on` (which points UPSTREAM, a
    /// step → its prerequisites): the edges are inverted to a downstream adjacency, then
    /// walked from `step_id`. Cycle-safe — a `visited` set bounds the walk even if a
    /// residual cycle survived (the DAG is normally acyclic, see [`Self::normalized`]);
    /// the start node itself is excluded. Returns 0 for an unknown id (fail-open). Pure.
    #[must_use]
    pub fn blast_radius(&self, step_id: &str) -> usize {
        downstream_count(&self.downstream_adjacency(), step_id)
    }

    /// Precompute [`Self::blast_radius`] for EVERY step in one pass (one shared
    /// downstream adjacency, a reachability walk per node) → `id → downstream-dependent
    /// count`. The scheduler reads this to order ready peers (and a rework round to
    /// order its blocking steps) by descending blast radius without rebuilding the
    /// adjacency each time. Pure.
    #[must_use]
    pub fn blast_radius_map(&self) -> std::collections::HashMap<String, usize> {
        let adj = self.downstream_adjacency();
        self.steps
            .iter()
            .map(|s| (s.id.clone(), downstream_count(&adj, s.id.as_str())))
            .collect()
    }

    /// [`Self::ready_steps`] ordered for the scheduler: highest **blast radius** FIRST,
    /// so among the currently-ready PEERS the most expensive-to-unwind upstream step is
    /// driven (and verified / reworked) before its lower-impact siblings. This never
    /// breaks the DAG order — `ready_steps` already guarantees every returned step's
    /// dependencies are `Done`, so a step still never runs before a prerequisite; this
    /// only orders the independent peers among themselves. Equal-radius peers keep the
    /// plan's original order (the sort is stable → deterministic). Pure.
    #[must_use]
    pub fn ready_steps_prioritized(&self) -> Vec<&PlanStep> {
        let radius = self.blast_radius_map();
        let mut ready = self.ready_steps();
        // Stable sort by DESCENDING blast radius; ties keep original (plan) order.
        ready.sort_by_key(|s| std::cmp::Reverse(radius.get(s.id.as_str()).copied().unwrap_or(0)));
        ready
    }

    /// Build the **downstream** adjacency of the plan DAG: maps each step id to the ids
    /// of the steps that DIRECTLY depend on it. `depends_on` points UPSTREAM (a step →
    /// its prerequisites), so this inverts every edge — a downstream walk then becomes a
    /// plain forward reachability. Only real step ids are keys/values (any dangling dep
    /// that survived normalisation is ignored). Borrows from `self`; pure.
    fn downstream_adjacency(&self) -> std::collections::HashMap<&str, Vec<&str>> {
        let ids: HashSet<&str> = self.steps.iter().map(|s| s.id.as_str()).collect();
        let mut adj: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::with_capacity(self.steps.len());
        // Seed every real step so a leaf (nothing depends on it) still has an entry.
        for s in &self.steps {
            adj.entry(s.id.as_str()).or_default();
        }
        for s in &self.steps {
            for d in &s.depends_on {
                if ids.contains(d.as_str()) {
                    // `d` is upstream of `s` ⇒ `s` is downstream of `d`.
                    adj.entry(d.as_str()).or_default().push(s.id.as_str());
                }
            }
        }
        adj
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

    /// The ids of the steps that transitively depend on `id` AND are still
    /// [`StepStatus::Pending`] — the subtree that would be **stranded** (never become
    /// ready) if `id` ends [`StepStatus::Blocked`]. `ready_steps` requires EVERY
    /// dependency `Done`, and a Blocked step never flips to Done, so every Pending step
    /// with a (direct or transitive) `depends_on` path to `id` is unreachable. Excludes
    /// `id` itself; a `Done` / `Active` / `Blocked` dependent is not "stranded" (it
    /// already ran or is terminal). Returns empty for an unknown id or a leaf with no
    /// pending dependents — the signal a BOUNDED RE-PLAN reads to decide whether a block
    /// is worth repairing (a leaf block strands nothing, so today's honest strand is
    /// already correct). Computed by a downstream reachability walk over the inverted
    /// `depends_on` edges; cycle-safe (bounded by a `visited` set). Pure.
    #[must_use]
    pub fn stranded_dependents(&self, id: &str) -> Vec<String> {
        let adj = self.downstream_adjacency();
        let Some((&start, _)) = adj.get_key_value(id) else {
            return Vec::new();
        };
        let mut visited: HashSet<&str> = HashSet::new();
        visited.insert(start);
        let mut downstream: HashSet<&str> = HashSet::new();
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            if let Some(neighbours) = adj.get(node) {
                for &nb in neighbours {
                    if visited.insert(nb) {
                        downstream.insert(nb);
                        stack.push(nb);
                    }
                }
            }
        }
        self.steps
            .iter()
            .filter(|s| s.status == StepStatus::Pending && downstream.contains(s.id.as_str()))
            .map(|s| s.id.clone())
            .collect()
    }

    /// **BOUNDED RE-PLAN merge** — replace a blocked subtree with a validated
    /// replacement sub-DAG. `replaced` is the set of step ids being retired (the blocked
    /// step + its stranded pending dependents, from [`Self::stranded_dependents`]);
    /// `new_steps` is the brain's proposed replacement (parsed via [`parse_brain_steps`]).
    ///
    /// The whole merged plan (the SURVIVING steps + the new steps) is re-validated
    /// through the EXISTING [`Self::normalized`] machinery — dedup by id, dangling-dep
    /// strip, cycle-break, and the per-seat / falsifiability evidence floors — so a
    /// re-planned subtree faces the identical structural discipline (and later the
    /// identical acceptance floor) as a freshly-synthesised plan. `normalized` resets
    /// every status to `Pending`, so afterward the SURVIVING steps' persisted statuses
    /// are restored (an already-`Done` step is NOT re-driven); the NEW steps keep
    /// `Pending` (fresh work the scheduler will pick up by readiness).
    ///
    /// Returns `true` ONLY when the merge actually CHANGES the plan — the replacement
    /// must introduce at least one genuinely-new step id (a real route around the
    /// blocker). An empty / unparseable sub-DAG, a reply that re-emits only existing
    /// ids, or a normalisation that survives nothing new leaves `self` **unchanged** and
    /// returns `false` (fail-open → the caller keeps today's honest stranded-Blocked
    /// report). Never panics.
    pub fn merge_replan(&mut self, replaced: &HashSet<String>, new_steps: Vec<PlanStep>) -> bool {
        if new_steps.is_empty() {
            return false;
        }
        let old_ids: HashSet<String> = self.steps.iter().map(|s| s.id.trim().to_string()).collect();
        // The replacement must add at least one NEW id — otherwise nothing routes
        // around the blocker and the blocked situation is unchanged.
        let introduces_new = new_steps.iter().any(|s| {
            let id = s.id.trim();
            !id.is_empty() && !old_ids.contains(id)
        });
        if !introduces_new {
            return false;
        }
        // Snapshot the survivors' statuses so `normalized`'s Pending-reset can't wipe
        // already-completed work (the surviving Done steps must stay Done).
        let survivor_status: std::collections::HashMap<String, StepStatus> = self
            .steps
            .iter()
            .filter(|s| !replaced.contains(&s.id))
            .map(|s| (s.id.trim().to_string(), s.status))
            .collect();
        // Build the merged node list: SURVIVORS first (so `normalized`'s first-seen
        // dedup keeps them over any colliding new id), then the new sub-DAG.
        let mut merged: Vec<PlanStep> = self
            .steps
            .iter()
            .filter(|s| !replaced.contains(&s.id))
            .cloned()
            .collect();
        merged.extend(new_steps);
        let candidate = Plan {
            steps: merged,
            risks: self.risks.clone(),
            open_questions: self.open_questions.clone(),
        };
        let Some(mut normalized) = candidate.normalized() else {
            return false; // nothing usable survived normalisation → fail-open
        };
        // Restore the survivors' statuses; NEW steps keep `normalized`'s fresh Pending.
        for s in &mut normalized.steps {
            if let Some(&st) = survivor_status.get(&s.id) {
                s.status = st;
            }
        }
        // Final guard: the normalised merge must still carry a genuinely-new step id
        // (one absent from the OLD plan) — else the sub-DAG collapsed to nothing new and
        // the blocked situation is unchanged (fail-open → honest strand).
        let recovered = normalized.steps.iter().any(|s| !old_ids.contains(&s.id));
        if !recovered {
            return false;
        }
        *self = normalized;
        true
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
        // M5: build the id set from the TRIMMED id — the deps below are trimmed
        // before the `ids.contains(d)` membership test, so an un-trimmed id here
        // (a brain id with surrounding whitespace) would not match its dependents'
        // trimmed refs, dropping a real edge as "dangling" and letting the dependent
        // run BEFORE its prerequisite. Trim on both sides so the DAG stays intact.
        let ids: HashSet<String> = self.steps.iter().map(|s| s.id.trim().to_string()).collect();
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
        // Wave B (team deliverables): enforce two structural ordering rules the brain
        // is asked for but UmaDev guarantees deterministically, then break any cycle
        // they introduce so the DAG always stays schedulable (fail-open).
        //   1. CONTRACT-FIRST — the architect's API contract is a hard prerequisite of
        //      every frontend/backend build step (lock the interface before building
        //      against it). See [`Self::enforce_contract_first`].
        //   2. TEST-AUTHOR ≠ CODE-AUTHOR — a QA test-authoring build step never depends
        //      on the code it will check (de-biasing); it writes tests from the
        //      contract/spec, schedulable before/independent of the code seat. See
        //      [`Self::enforce_test_authoring_independence`].
        self.enforce_contract_first();
        self.enforce_test_authoring_independence();
        self.break_dependency_cycles();
        // PER-SEAT DETERMINISTIC FLOOR (verification-level seat differentiation) —
        // orthogonal to the DAG above: augment an UNDER-SPECIFIED build step with a
        // seat-appropriate default evidence contract (backend → FE↔BE contract, QA →
        // tests pass, frontend/security → build+lint clean), then a falsifiability
        // backstop when the brain under-specified the plan wholesale. Augment-only +
        // fail-open: a step already carrying a strong contract, or a seat with no floor,
        // is left exactly as today. See [`Self::enforce_seat_evidence_floor`] /
        // [`Self::enforce_falsifiability_floor`].
        self.enforce_seat_evidence_floor();
        self.enforce_falsifiability_floor();
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

    /// **Contract-first DAG ordering** (Wave B deliverable 2) — make the architect's
    /// API contract a hard `depends_on` PREREQUISITE of every frontend/backend BUILD
    /// step, so the interface is LOCKED before the engineers build against it (the
    /// contract is the handoff — `.umadev/contracts/openapi.*` + the architecture API
    /// table). A "contract step" is any Build step seated by the architect OR whose
    /// acceptance IS [`AcceptanceSpec::Contract`]; a "consumer" is any Build step
    /// seated by the frontend/backend engineer. Each consumer gains an edge to each
    /// contract step (deduped, never a self-edge). Any cycle this introduces (e.g. an
    /// architect step the brain made depend on an engineer) is broken afterward by
    /// [`Self::break_dependency_cycles`], so the DAG always stays schedulable.
    /// No contract step ⇒ a deterministic no-op (fail-open). Pure over `self`.
    fn enforce_contract_first(&mut self) {
        use crate::critics::Seat;
        let contract_ids: Vec<String> = self
            .steps
            .iter()
            .filter(|s| {
                s.kind == StepKind::Build
                    && (s.seat == Seat::Architect || s.acceptance == AcceptanceSpec::Contract)
            })
            .map(|s| s.id.clone())
            .collect();
        if contract_ids.is_empty() {
            return; // no architect contract in this plan → nothing to order
        }
        for s in &mut self.steps {
            if s.kind != StepKind::Build {
                continue; // only a BUILD step consumes the contract by building on it
            }
            if !matches!(s.seat, Seat::FrontendEngineer | Seat::BackendEngineer) {
                continue;
            }
            for cid in &contract_ids {
                if cid != &s.id && !s.depends_on.contains(cid) {
                    s.depends_on.push(cid.clone());
                }
            }
        }
    }

    /// **Test-author ≠ code-author** (Wave B deliverable 3) — strip any dependency a
    /// QA test-authoring BUILD step has on a frontend/backend code step, so the QA
    /// seat writes tests INDEPENDENT of (and schedulable before) the code it checks.
    /// A test written AGAINST the delivered code inherits that code's blind spots; a
    /// SEPARATE author working from the contract/spec catches what the code-author
    /// assumed away (the de-biasing principle). Only QA BUILD steps are affected — a
    /// QA *review* step legitimately depends on the code (it reads it), and a QA
    /// dependency on the architect's contract is KEPT (tests should bind the locked
    /// interface). No QA build step / no code step ⇒ a no-op (fail-open). Pure.
    fn enforce_test_authoring_independence(&mut self) {
        use crate::critics::Seat;
        let code_ids: HashSet<String> = self
            .steps
            .iter()
            .filter(|s| {
                s.kind == StepKind::Build
                    && matches!(s.seat, Seat::FrontendEngineer | Seat::BackendEngineer)
            })
            .map(|s| s.id.clone())
            .collect();
        if code_ids.is_empty() {
            return;
        }
        for s in &mut self.steps {
            // A QA test-authoring step is a QA-seated step that BUILDS (writes tests).
            if s.kind == StepKind::Build && s.seat == Seat::QaEngineer {
                s.depends_on.retain(|d| !code_ids.contains(d));
            }
        }
    }

    /// **Per-seat deterministic acceptance floor** — auto-attach a seat-appropriate
    /// default [`EvidenceContract`] to a BUILD step the brain left UNDER-SPECIFIED, so
    /// seats are mechanically differentiated at VERIFICATION time (a backend step is
    /// judged by its FE↔BE contract / a real route registration, a QA step by tests
    /// actually passing, a frontend/security step by a clean build+lint) rather than
    /// only narratively. This AUGMENTS: it fires ONLY for a step carrying no contract
    /// stronger than source-present ([`PlanStep::has_strong_contract`]), so a
    /// brain-volunteered contract is never removed or downgraded. Every default is a
    /// variant the existing deterministic floor already verifies — no new gate, no new
    /// [`EvidenceContract`] variant. A REVIEW step, a seat with no floor, or an
    /// already-contracted step is left exactly as today (fail-open). Bounded (one pass);
    /// pure over `self`.
    fn enforce_seat_evidence_floor(&mut self) {
        for s in &mut self.steps {
            // Only a BUILD step is judged by an evidence contract; a REVIEW step is
            // judged by its reviewing seat's verdict (ReviewClean) and is left alone.
            // An already-contracted step is never touched (augment, never downgrade).
            if s.kind != StepKind::Build || s.has_strong_contract() {
                continue;
            }
            let default = match s.seat {
                // Backend: the FE↔BE contract must hold — the same backend-route
                // registration check the deterministic floor runs, so the endpoint has
                // something to verify (not just "some source exists").
                Seat::BackendEngineer => EvidenceContract::ContractMatches,
                // QA: a test-authoring step is meaningless unless tests actually pass.
                Seat::QaEngineer => EvidenceContract::TestPasses { name: None },
                // Frontend / Security: the real build/test/lint must pass — where the
                // frontend craft-governance (banned patterns / a11y lint) runs and a
                // security change's regressions surface.
                Seat::FrontendEngineer | Seat::SecurityEngineer => EvidenceContract::BuildClean,
                // Every other seat keeps today's behaviour (fail-open); the
                // falsifiability backstop below still covers a wholesale-sloppy plan.
                _ => continue,
            };
            push_unique(&mut s.evidence, default);
        }
    }

    /// **Falsifiability backstop** — if, AFTER the per-seat floor, strictly MORE THAN
    /// HALF the plan's BUILD steps STILL carry no contract stronger than source-present,
    /// the brain under-specified the plan wholesale (falsifiability is brain-optional, so
    /// a lazy plan ships build steps proven by nothing but "a file exists"). Synthesize a
    /// sane deterministic default — [`EvidenceContract::BuildClean`], the project's real
    /// build/test/lint — on each STILL-bare build step, so "done" stays falsifiable even
    /// for a sloppy plan. (`PlanStep` carries no declared-files field to mint a
    /// `FileExists` from, and there is no re-ask channel back to the coordinator that
    /// stays inside this module, so a deterministic default is the bounded, fail-open
    /// choice.) A step already carrying a strong contract is never touched; no build step
    /// ⇒ a no-op. Pure over `self`.
    fn enforce_falsifiability_floor(&mut self) {
        let build_total = self
            .steps
            .iter()
            .filter(|s| s.kind == StepKind::Build)
            .count();
        let bare = self
            .steps
            .iter()
            .filter(|s| s.kind == StepKind::Build && !s.has_strong_contract())
            .count();
        // Trip only on STRICTLY more than half the build steps still bare.
        if build_total == 0 || bare * 2 <= build_total {
            return;
        }
        for s in &mut self.steps {
            if s.kind == StepKind::Build && !s.has_strong_contract() {
                push_unique(&mut s.evidence, EvidenceContract::BuildClean);
            }
        }
    }
}

/// Count the steps transitively reachable downstream from `start` over a
/// [`Plan::downstream_adjacency`] map — i.e. how many steps (directly or transitively)
/// depend on `start`. The walk is bounded by a `visited` set, so it terminates even if a
/// residual cycle survived the DAG normalisation (cycle-safe); the start node itself is
/// excluded from the count. An id absent from the map returns 0 (fail-open). Pure.
fn downstream_count(adj: &std::collections::HashMap<&str, Vec<&str>>, start: &str) -> usize {
    // Resolve the canonical key reference so `visited`/`stack` share the map's lifetime.
    let Some((&start_key, _)) = adj.get_key_value(start) else {
        return 0;
    };
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(start_key);
    let mut stack = vec![start_key];
    while let Some(node) = stack.pop() {
        if let Some(neighbours) = adj.get(node) {
            for &nb in neighbours {
                if visited.insert(nb) {
                    stack.push(nb);
                }
            }
        }
    }
    // `visited` includes the start node; subtract it to count only the dependents.
    visited.len() - 1
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
    /// The brain's PROPOSED per-step evidence — a tolerant array of values (each a
    /// bareword like `"build-clean"` or an object `{"kind":"file-exists","path":…}`).
    /// Parsed as raw [`serde_json::Value`]s so one malformed entry can never fail the
    /// whole plan parse; [`parse_brain_evidence`] then validates them into owned
    /// [`EvidenceContract`]s (dropping the unparseable). Absent ⇒ empty ⇒ the step
    /// falls back to its [`AcceptanceSpec`].
    #[serde(default)]
    evidence: Vec<serde_json::Value>,
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
    // `options.project_root` feeds the success-recipe fingerprint recall below;
    // model/trust already live on the session.
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
         `acceptance`: source-present | build-test | contract | design-tokens | review-clean. \
         `evidence` (OPTIONAL but STRONGLY preferred): an array declaring the SPECIFIC, \
         machine-checkable proof this step is done — UmaDev verifies it deterministically \
         before marking the step done, so make `done` falsifiable. Each entry is an object \
         {{\"kind\":…, …}}; kinds: \
         {{\"kind\":\"file-exists\",\"path\":\"src/foo.tsx\"}}, \
         {{\"kind\":\"file-contains\",\"path\":\"src/api.ts\",\"needle\":\"/api/login\"}}, \
         {{\"kind\":\"test-passes\",\"name\":\"login\"}}, {{\"kind\":\"build-clean\"}}, \
         {{\"kind\":\"route-responds\",\"method\":\"POST\",\"path\":\"/api/login\",\"status\":200}}, \
         {{\"kind\":\"contract-matches\"}}, {{\"kind\":\"source-present\"}}. Prefer the most \
         specific evidence the step actually produces (a concrete file/route over a generic \
         build-clean). \
         Team-deliverable rules (UmaDev ALSO enforces these structurally, so honour them): \
         (a) when there is a UI surface, the uiux-designer has a BUILD step that writes the \
         design system as real `design-tokens.{{json,css}}` files (acceptance=design-tokens) \
         and every frontend step depends_on it; \
         (b) the architect's API contract is a depends_on PREREQUISITE of every \
         frontend/backend step (lock the interface first); \
         (c) QA AUTHORS tests as its OWN build step (seat=qa-engineer, kind=build, \
         acceptance=build-test) that does NOT depend on the frontend/backend code steps — \
         the test-author must not be the code-author (de-biasing); a QA review step is \
         separate. \
         JSON shape: {{\"steps\":[{{\"id\":\"scaffold\",\"title\":\"…\",\"seat\":\"…\",\
         \"kind\":\"build\",\"depends_on\":[],\"acceptance\":\"source-present\",\
         \"evidence\":[{{\"kind\":\"file-exists\",\"path\":\"src/App.tsx\"}}]}}],\
         \"risks\":[\"…\"],\"open_questions\":[\"…\"]}}",
        class = route.class.as_str(),
        kind = route.kind.id(),
        depth = route.depth.as_str(),
    );
    let user = format!("Requirement:\n{requirement}");

    // SUCCESS-RECIPE RECALL (a PRIOR, never a template): look up the closest past
    // CLEAN build of a similar stack/kind/feature in the cross-project recipe store
    // and inject its proven plan SHAPE as an adaptable hint the brain may reuse or
    // ignore. Fail-open at every step — no home dir, no match, or a read error yields
    // an empty prior, and the plan is synthesised exactly as before. Bounded to one
    // best recipe within [`crate::recipes::RECIPE_PRIOR_BUDGET`].
    let recipe_prior = crate::recipes::recipes_dir()
        .and_then(|dir| {
            let fp = crate::recipes::fingerprint_for(&options.project_root, route, requirement);
            crate::recipes::recall_prior_block(&dir, &fp, crate::recipes::RECIPE_PRIOR_BUDGET)
        })
        .map(|block| format!("{block}\n\n"))
        .unwrap_or_default();

    // Run the planning turn on the MAIN session — NOT a fork. claude cannot
    // `--resume` a session that has not had its first turn yet, so a pre-build
    // planning FORK fails silently and the user never sees a plan. Running it here
    // makes planning the session's FIRST turn: reliable, it establishes the session
    // so later QC forks work, and the base keeps the plan in its own context when it
    // then builds. JSON-only, tools forbidden this turn.
    let directive = format!(
        "{system}\n\n{recipe_prior}Return EXACTLY ONE JSON object and nothing else — no markdown, \
         no code fence, no prose. Do NOT write any files or run any commands in this \
         turn; this is the PLAN only.\n\n{user}"
    );
    let text = drain_plan_turn(session, directive, deadline).await?;
    let json = crate::continuous::extract_json_object(&text)?;
    let raw: BrainPlan = serde_json::from_str(&json).ok()?;
    let plan = Plan {
        steps: raw.steps.into_iter().map(brain_step_to_plan_step).collect(),
        risks: raw.risks,
        open_questions: raw.open_questions,
    };
    plan.normalized()
}

/// Map ONE tolerant [`BrainStep`] into an owned [`PlanStep`] — the shared node parse
/// used by both [`synthesize_plan`] (the initial DAG) and [`parse_brain_steps`] (a
/// re-plan sub-DAG). An unknown / missing seat fails open to a sensible default by step
/// kind (build → frontend doer, review → QA) so a vague brain reply still yields an
/// assignable step; the brain's proposed per-step evidence is validated + OWNED
/// (unparseable entries dropped, an under-specified one retained as a held gap); status
/// always starts [`StepStatus::Pending`] — the director drives it from reality, never
/// the brain's optimistic claim.
fn brain_step_to_plan_step(b: BrainStep) -> PlanStep {
    let kind = StepKind::parse(&b.kind);
    PlanStep {
        id: b.id,
        title: b.title,
        seat: Seat::from_alias(&b.seat).unwrap_or(match kind {
            StepKind::Review => Seat::QaEngineer,
            StepKind::Build => Seat::FrontendEngineer,
        }),
        kind,
        depends_on: b.depends_on,
        acceptance: AcceptanceSpec::parse(&b.acceptance, kind),
        evidence: parse_brain_evidence(&b.evidence),
        status: StepStatus::Pending,
    }
}

/// Parse a brain-supplied plan JSON reply into owned [`PlanStep`]s WITHOUT normalising
/// or wrapping in a full [`Plan`] — the raw replacement nodes a BOUNDED RE-PLAN merges
/// into an existing plan (the merge, [`Plan::merge_replan`], re-normalises the whole
/// thing so dedup / dangling-dep strip / cycle-break / seat floors run over the spliced
/// result). Accepts either a bare JSON object or a reply with surrounding prose (it
/// re-extracts the object). Fail-open by contract: unparseable JSON / no `steps` yields
/// an EMPTY vec, so the caller falls back to today's honest stranded-Blocked report and
/// nothing is merged. Reuses the tolerant [`BrainStep`] parse, so one malformed field
/// never sinks the batch.
pub(crate) fn parse_brain_steps(json_text: &str) -> Vec<PlanStep> {
    let raw: Option<BrainPlan> = serde_json::from_str(json_text).ok().or_else(|| {
        crate::continuous::extract_json_object(json_text)
            .and_then(|j| serde_json::from_str(&j).ok())
    });
    match raw {
        Some(p) => p.steps.into_iter().map(brain_step_to_plan_step).collect(),
        None => Vec::new(),
    }
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
            evidence: Vec::new(),
            status: StepStatus::Pending,
        }
    }

    /// A step with an explicit seat / kind / acceptance (for the Wave-B ordering
    /// tests) — `step()` above always uses a frontend build step.
    fn step_seat(
        id: &str,
        deps: &[&str],
        seat: Seat,
        kind: StepKind,
        acceptance: AcceptanceSpec,
    ) -> PlanStep {
        PlanStep {
            id: id.to_string(),
            title: format!("step {id}"),
            seat,
            kind,
            depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
            acceptance,
            evidence: Vec::new(),
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

    /// Find a step's `depends_on` by id (test helper).
    fn deps_of<'a>(p: &'a Plan, id: &str) -> &'a [String] {
        &p.steps.iter().find(|s| s.id == id).unwrap().depends_on
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
    fn normalize_keeps_a_dep_edge_when_the_prereq_id_has_whitespace() {
        // M5 regression: a brain step id with surrounding whitespace (" auth ") must
        // still satisfy a dependent's (trimmed) ref to it. The id-set is built from the
        // TRIMMED ids, so the edge is NOT dropped as dangling — otherwise the dependent
        // would run BEFORE its prerequisite.
        let p = plan(vec![step(" auth ", &[]), step("ui", &["auth"])])
            .normalized()
            .expect("a usable plan survives");
        // Both ids are trimmed; the dependency edge survives.
        assert_eq!(deps_of(&p, "ui"), &["auth".to_string()]);
        // Only `auth` (no deps) is ready first; `ui` waits on it (edge intact).
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(
            ready,
            vec!["auth"],
            "ui must NOT be ready before its prereq"
        );
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
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let p = plan(vec![step("a", &[]), step("b", &["a"])]);
        let path = save(&p, dir).expect("save ok");
        assert!(path.exists());
        let loaded = load(dir).expect("load ok");
        assert_eq!(loaded, p);
    }

    #[test]
    fn load_missing_is_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(load(tmp.path()).is_none());
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
    fn blast_radius_counts_transitive_downstream() {
        // Linear chain a <- b <- c (c depends on b, b depends on a). a's downstream cone
        // is {b, c} = 2; b's is {c} = 1; the leaf c is 0.
        let p = plan(vec![step("a", &[]), step("b", &["a"]), step("c", &["b"])]);
        assert_eq!(p.blast_radius("a"), 2);
        assert_eq!(p.blast_radius("b"), 1);
        assert_eq!(p.blast_radius("c"), 0);
        // An unknown id fail-opens to 0.
        assert_eq!(p.blast_radius("ghost"), 0);
        // The precomputed map agrees with the per-id function.
        let m = p.blast_radius_map();
        assert_eq!(m.get("a").copied(), Some(2));
        assert_eq!(m.get("b").copied(), Some(1));
        assert_eq!(m.get("c").copied(), Some(0));
    }

    #[test]
    fn blast_radius_handles_a_diamond_without_double_counting() {
        // Diamond: a <- b, a <- c, then d depends on BOTH b and c. a's downstream cone
        // is {b, c, d} = 3 — d is reachable via two paths but counted ONCE (set-based).
        let p = plan(vec![
            step("a", &[]),
            step("b", &["a"]),
            step("c", &["a"]),
            step("d", &["b", "c"]),
        ]);
        assert_eq!(
            p.blast_radius("a"),
            3,
            "diamond apex counts d once, not twice"
        );
        assert_eq!(p.blast_radius("b"), 1);
        assert_eq!(p.blast_radius("c"), 1);
        assert_eq!(p.blast_radius("d"), 0);
    }

    #[test]
    fn blast_radius_is_cycle_safe() {
        // A residual cycle (constructed DIRECTLY, bypassing `normalized`'s cycle-break)
        // must not hang the reverse-reachability walk: the `visited` set bounds it.
        // a -> b -> c -> a is a 3-cycle; each node reaches the other two → radius 2.
        let p = plan(vec![
            step("a", &["c"]),
            step("b", &["a"]),
            step("c", &["b"]),
        ]);
        assert_eq!(p.blast_radius("a"), 2);
        assert_eq!(p.blast_radius("b"), 2);
        assert_eq!(p.blast_radius("c"), 2);
        // A 2-cycle a <-> b → each depends on the other → radius 1 (not an infinite loop).
        let p2 = plan(vec![step("a", &["b"]), step("b", &["a"])]);
        assert_eq!(p2.blast_radius("a"), 1);
        assert_eq!(p2.blast_radius("b"), 1);
    }

    #[test]
    fn ready_steps_prioritized_orders_ready_peers_by_blast_radius() {
        // Two independent ready peers with DIFFERENT blast radii, plus two steps that
        // depend on the high-radius one. `config` is listed FIRST in plan order but has
        // radius 0; `schema` has radius 2 (api + ui depend on it). The prioritised
        // schedule must surface `schema` BEFORE `config` even though plan order is the
        // reverse — the upstream, expensive-to-unwind work is driven first.
        let mut p = plan(vec![
            step("config", &[]),      // radius 0, first in plan order
            step("schema", &[]),      // radius 2 (api, ui)
            step("api", &["schema"]), // not ready until schema is Done
            step("ui", &["schema"]),
        ]);
        // Plain readiness is plan order: config, schema (api/ui gated by schema).
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["config", "schema"]);
        // Prioritised readiness puts the higher-blast-radius peer first.
        let prio: Vec<_> = p
            .ready_steps_prioritized()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(prio, vec!["schema", "config"]);
        // DAG correctness: a dependent never appears before its prerequisite is Done —
        // api/ui are absent until schema completes.
        assert!(!prio.contains(&"api".to_string()));
        assert!(!prio.contains(&"ui".to_string()));
        // After schema completes, api + ui join the ready set (now downstream is legal),
        // and every prioritised step still has all deps Done (DAG invariant holds).
        assert!(p.mark("schema", StepStatus::Done));
        let prio2: Vec<_> = p
            .ready_steps_prioritized()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        // config, api, ui are all radius 0 now → stable plan order is preserved.
        assert_eq!(prio2, vec!["config", "api", "ui"]);
        let done: HashSet<&str> = p
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Done)
            .map(|s| s.id.as_str())
            .collect();
        for s in p.ready_steps_prioritized() {
            assert!(
                s.depends_on.iter().all(|d| done.contains(d.as_str())),
                "a prioritised step never precedes an unfinished prerequisite: {}",
                s.id
            );
        }
    }

    #[test]
    fn blast_radius_map_prioritizes_rework_of_the_highest_impact_blocking_step() {
        // The rework-priority primitive: given a set of steps that each carry a blocking
        // finding, the director reworks the highest-blast-radius one FIRST. `schema`
        // (radius 2) outranks the independent `docs` (radius 0), so sorting the blocking
        // ids by the blast-radius map yields schema before docs.
        let p = plan(vec![
            step("schema", &[]),
            step("api", &["schema"]),
            step("ui", &["schema"]),
            step("docs", &[]),
        ]);
        let radius = p.blast_radius_map();
        let mut blocking = vec!["docs".to_string(), "schema".to_string()];
        blocking.sort_by_key(|id| std::cmp::Reverse(radius.get(id.as_str()).copied().unwrap_or(0)));
        assert_eq!(
            blocking,
            vec!["schema".to_string(), "docs".to_string()],
            "the highest-blast-radius blocking step is reworked first"
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
        // Wave B deliverable 1: the designer's design-tokens acceptance + its aliases.
        for s in ["design-tokens", "design_tokens", "design-system", "tokens"] {
            assert_eq!(
                AcceptanceSpec::parse(s, StepKind::Build),
                AcceptanceSpec::DesignTokensPresent,
                "{s} → DesignTokensPresent"
            );
        }
    }

    // ── Wave B deliverable 2: contract-first DAG ordering ────────────────────

    #[test]
    fn contract_first_makes_engineers_depend_on_the_architect() {
        // An architect contract step + a frontend + a backend build step that the
        // brain left WITHOUT a contract dependency. Normalisation must insert the
        // edge so neither engineer is ready until the architect's contract is Done.
        let p = plan(vec![
            step_seat(
                "contract",
                &[],
                Seat::Architect,
                StepKind::Build,
                AcceptanceSpec::Contract,
            ),
            step_seat(
                "fe",
                &[],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "be",
                &[],
                Seat::BackendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized()
        .expect("usable");
        assert!(deps_of(&p, "fe").contains(&"contract".to_string()));
        assert!(deps_of(&p, "be").contains(&"contract".to_string()));
        // Only the architect's contract is ready first — the interface is locked
        // BEFORE the engineers build against it.
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["contract"]);
    }

    #[test]
    fn contract_first_recognizes_a_contract_acceptance_step_and_is_idempotent() {
        // The "contract step" can be identified by acceptance=Contract even when not
        // architect-seated, and an engineer that ALREADY depends on it gains no
        // duplicate edge.
        let p = plan(vec![
            step_seat(
                "api-spec",
                &[],
                Seat::BackendEngineer,
                StepKind::Build,
                AcceptanceSpec::Contract,
            ),
            step_seat(
                "fe",
                &["api-spec"],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized()
        .expect("usable");
        // Exactly one edge, not a duplicate.
        assert_eq!(deps_of(&p, "fe"), &["api-spec".to_string()]);
    }

    #[test]
    fn contract_first_is_a_noop_without_an_architect_step() {
        // No architect / contract step → nothing to order; the FE step is ready
        // immediately (fail-open — we never fabricate a phantom prerequisite).
        let p = plan(vec![step_seat(
            "fe",
            &[],
            Seat::FrontendEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized()
        .expect("usable");
        assert!(deps_of(&p, "fe").is_empty());
    }

    #[test]
    fn contract_first_stays_acyclic_if_architect_depends_on_an_engineer() {
        // A pathological plan where the architect step ALSO depends on the frontend
        // step. Contract-first adds fe→contract; combined with the brain's
        // contract→fe that is a cycle — the cycle-breaker must keep the DAG
        // schedulable (at least one step ready, total deps bounded).
        let p = plan(vec![
            step_seat(
                "contract",
                &["fe"],
                Seat::Architect,
                StepKind::Build,
                AcceptanceSpec::Contract,
            ),
            step_seat(
                "fe",
                &[],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized()
        .expect("usable");
        assert!(
            !p.ready_steps().is_empty(),
            "the cycle was broken → at least one step is ready: {:?}",
            p.steps
        );
    }

    // ── Wave B deliverable 3: QA test-author ≠ code-author ───────────────────

    #[test]
    fn qa_test_authoring_build_step_is_independent_of_the_code_steps() {
        // A QA test-authoring BUILD step the brain (wrongly) made depend on the
        // frontend + backend code, plus a legit dep on the architect's contract.
        // Normalisation strips the CODE edges (de-biasing) but KEEPS the contract
        // edge (tests should bind the locked interface).
        let p = plan(vec![
            step_seat(
                "contract",
                &[],
                Seat::Architect,
                StepKind::Build,
                AcceptanceSpec::Contract,
            ),
            step_seat(
                "fe",
                &["contract"],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "be",
                &["contract"],
                Seat::BackendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "qa-tests",
                &["contract", "fe", "be"],
                Seat::QaEngineer,
                StepKind::Build,
                AcceptanceSpec::BuildTest,
            ),
        ])
        .normalized()
        .expect("usable");
        let qa_deps = deps_of(&p, "qa-tests");
        assert!(
            qa_deps.contains(&"contract".to_string()),
            "the contract edge is KEPT: {qa_deps:?}"
        );
        assert!(
            !qa_deps.contains(&"fe".to_string()) && !qa_deps.contains(&"be".to_string()),
            "the code edges are stripped (test-author ≠ code-author): {qa_deps:?}"
        );
        // Sequencing: once the contract is Done, the QA test step is ready
        // ALONGSIDE the code (not gated behind it) — the test-author works in
        // parallel with / before the code-author, never downstream of it.
        let mut p = p;
        assert!(p.mark("contract", StepStatus::Done));
        let ready: Vec<String> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert!(
            ready.contains(&"qa-tests".to_string()),
            "QA tests are ready independent of the code: {ready:?}"
        );
        assert!(ready.contains(&"fe".to_string()) && ready.contains(&"be".to_string()));
    }

    #[test]
    fn qa_review_step_keeps_its_dependency_on_the_code() {
        // De-biasing applies only to a QA BUILD (test-authoring) step. A QA REVIEW
        // step legitimately depends on the code — it READS the delivered code — so
        // its edge is preserved.
        let p = plan(vec![
            step_seat(
                "fe",
                &[],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "qa-review",
                &["fe"],
                Seat::QaEngineer,
                StepKind::Review,
                AcceptanceSpec::ReviewClean,
            ),
        ])
        .normalized()
        .expect("usable");
        assert!(
            deps_of(&p, "qa-review").contains(&"fe".to_string()),
            "a QA review step keeps its code dependency (it reads the code)"
        );
    }

    // ── Evidence-contract-per-step: parsing + ownership ──────────────────────

    #[test]
    fn evidence_contract_parses_object_and_bareword_forms() {
        use serde_json::json;
        // Object forms with their fields.
        assert_eq!(
            EvidenceContract::parse_value(&json!({"kind":"file-exists","path":"src/a.tsx"})),
            Some(EvidenceContract::FileExists {
                path: "src/a.tsx".into()
            })
        );
        assert_eq!(
            EvidenceContract::parse_value(
                &json!({"kind":"file-contains","path":"x.ts","needle":"/api/login"})
            ),
            Some(EvidenceContract::FileContains {
                path: "x.ts".into(),
                needle: "/api/login".into()
            })
        );
        // Method is upper-cased; numeric status is read into Some(_).
        assert_eq!(
            EvidenceContract::parse_value(
                &json!({"kind":"route-responds","method":"post","path":"/api/login","status":200})
            ),
            Some(EvidenceContract::RouteResponds {
                method: "POST".into(),
                path: "/api/login".into(),
                status: Some(200)
            })
        );
        // A string status + a missing method (defaults to GET).
        assert_eq!(
            EvidenceContract::parse_value(&json!({"kind":"route","path":"/x","status":"201"})),
            Some(EvidenceContract::RouteResponds {
                method: "GET".into(),
                path: "/x".into(),
                status: Some(201)
            })
        );
        // L2: an absent status means "any non-error response" → None (not the old
        // `0` sentinel); a required error status like 401 is now expressible as Some(401).
        assert_eq!(
            EvidenceContract::parse_value(&json!({"kind":"route","path":"/secure"})),
            Some(EvidenceContract::RouteResponds {
                method: "GET".into(),
                path: "/secure".into(),
                status: None
            })
        );
        assert_eq!(
            EvidenceContract::parse_value(&json!({"kind":"route","path":"/secure","status":401})),
            Some(EvidenceContract::RouteResponds {
                method: "GET".into(),
                path: "/secure".into(),
                status: Some(401)
            })
        );
        assert_eq!(
            EvidenceContract::parse_value(&json!({"kind":"test-passes","name":"login"})),
            Some(EvidenceContract::TestPasses {
                name: Some("login".into())
            })
        );
        assert_eq!(
            EvidenceContract::parse_value(&json!({"kind":"test-passes"})),
            Some(EvidenceContract::TestPasses { name: None })
        );
        // Bareword (no-argument) kinds.
        assert_eq!(
            EvidenceContract::parse_value(&json!("build-clean")),
            Some(EvidenceContract::BuildClean)
        );
        assert_eq!(
            EvidenceContract::parse_value(&json!("source-present")),
            Some(EvidenceContract::SourcePresent)
        );
        assert_eq!(
            EvidenceContract::parse_value(&json!("contract-matches")),
            Some(EvidenceContract::ContractMatches)
        );
        // M6: a recognised kind that is under-specified (an empty/missing required
        // field) is RETAINED as an explicit Malformed gap — NOT silently dropped (which
        // would let the step fall back to the coarse "any source exists" default).
        assert!(matches!(
            EvidenceContract::parse_value(&json!({"kind":"file-exists"})),
            Some(EvidenceContract::Malformed { .. })
        ));
        assert!(matches!(
            EvidenceContract::parse_value(&json!({"kind":"file-contains","path":"x"})),
            Some(EvidenceContract::Malformed { .. })
        ));
        // A genuinely-unrecognised kind / a wrong JSON type / a bareword that needs
        // fields is still dropped (None) — never a panic.
        assert!(EvidenceContract::parse_value(&json!({"kind":"bogus"})).is_none());
        assert!(EvidenceContract::parse_value(&json!("file-exists")).is_none());
        assert!(EvidenceContract::parse_value(&json!(123)).is_none());
    }

    #[test]
    fn brain_step_evidence_is_parsed_owned_and_underspecified_kept_as_gap() {
        // A brain step JSON whose evidence array mixes a good object, an under-specified
        // object (M6: KEPT as a Malformed gap, not dropped), a duplicate (deduped), and a
        // bareword. UmaDev PARSES + OWNS the typed contracts — the base does not self-grade.
        let raw: BrainStep = serde_json::from_str(
            r#"{
                "id":"login","title":"login route","seat":"backend-engineer","kind":"build",
                "evidence":[
                    {"kind":"file-exists","path":"src/login.ts"},
                    {"kind":"file-exists"},
                    {"kind":"file-exists","path":"src/login.ts"},
                    "build-clean"
                ]
            }"#,
        )
        .expect("brain step parses");
        let evidence = parse_brain_evidence(&raw.evidence);
        assert_eq!(
            evidence.len(),
            3,
            "good + malformed-gap + build-clean (dup deduped)"
        );
        assert!(evidence.contains(&EvidenceContract::FileExists {
            path: "src/login.ts".into()
        }));
        assert!(evidence.contains(&EvidenceContract::BuildClean));
        // The under-specified `{"kind":"file-exists"}` is retained as an explicit gap, so
        // the step is NOT silently degraded to "any source exists".
        assert!(
            evidence
                .iter()
                .any(|c| matches!(c, EvidenceContract::Malformed { .. })),
            "an under-specified file-exists must be kept as a Malformed gap: {evidence:?}"
        );
    }

    #[test]
    fn one_malformed_evidence_entry_never_fails_the_whole_plan_parse() {
        // A wrong-typed evidence entry (a bare number) must NOT break the tolerant
        // Value-based parse: the plan still parses and the bad entry is simply dropped.
        let bp: BrainPlan = serde_json::from_str(
            r#"{"steps":[{"id":"a","title":"t","evidence":[123,{"kind":"build-clean"}]}]}"#,
        )
        .expect("plan stays parseable despite a malformed evidence entry");
        let ev = parse_brain_evidence(&bp.steps[0].evidence);
        assert_eq!(ev, vec![EvidenceContract::BuildClean]);
    }

    #[test]
    fn plan_step_with_evidence_round_trips_through_save_load() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s = step("a", &[]);
        s.evidence = vec![
            EvidenceContract::FileExists {
                path: "src/App.tsx".into(),
            },
            EvidenceContract::RouteResponds {
                method: "GET".into(),
                path: "/".into(),
                status: None,
            },
        ];
        let p = plan(vec![s]);
        save(&p, dir).expect("save ok");
        let loaded = load(dir).expect("load ok");
        assert_eq!(
            loaded, p,
            "the typed evidence contract survives persistence"
        );
    }

    // ── Per-seat deterministic acceptance floor ──────────────────────────────

    /// Find a step's evidence by id (test helper).
    fn ev_of<'a>(p: &'a Plan, id: &str) -> &'a [EvidenceContract] {
        &p.steps.iter().find(|s| s.id == id).unwrap().evidence
    }

    #[test]
    fn backend_build_step_gains_a_contract_floor_when_bare() {
        // A backend build step the brain left with only the source-present floor gains a
        // FE↔BE ContractMatches bar — so it is judged by a real route registration, the
        // seat-appropriate mechanical check, not by "some source exists".
        let p = plan(vec![step_seat(
            "api",
            &[],
            Seat::BackendEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized()
        .expect("usable");
        assert!(
            ev_of(&p, "api").contains(&EvidenceContract::ContractMatches),
            "a bare backend build step gains an endpoint/contract floor: {:?}",
            ev_of(&p, "api")
        );
    }

    #[test]
    fn qa_build_step_gains_a_test_passes_floor_when_bare() {
        // A bare QA build step is meaningless unless tests actually pass — it gains a
        // TestPasses bar. (A frontend peer keeps the plan below the wholesale backstop.)
        let p = plan(vec![
            step_seat(
                "qa",
                &[],
                Seat::QaEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "ui",
                &[],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized()
        .expect("usable");
        assert!(
            ev_of(&p, "qa")
                .iter()
                .any(|c| matches!(c, EvidenceContract::TestPasses { .. })),
            "a bare QA build step is judged by tests passing: {:?}",
            ev_of(&p, "qa")
        );
    }

    #[test]
    fn frontend_build_step_gains_a_build_governance_floor_when_bare() {
        // A bare frontend build step gains a BuildClean bar — where the frontend
        // craft-governance (banned patterns / a11y lint) actually runs.
        let p = plan(vec![step_seat(
            "ui",
            &[],
            Seat::FrontendEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized()
        .expect("usable");
        assert!(
            ev_of(&p, "ui").contains(&EvidenceContract::BuildClean),
            "a bare frontend build step is judged by the build/lint governance floor: {:?}",
            ev_of(&p, "ui")
        );
    }

    #[test]
    fn security_doing_step_gains_a_build_floor_when_bare() {
        // A (rare) security BUILD step gains a BuildClean bar — a security change must at
        // least keep the build/test green, a falsifiable bar stronger than source-present.
        let p = plan(vec![step_seat(
            "harden",
            &[],
            Seat::SecurityEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized()
        .expect("usable");
        assert!(
            ev_of(&p, "harden").contains(&EvidenceContract::BuildClean),
            "a bare security doing step references a real build bar: {:?}",
            ev_of(&p, "harden")
        );
    }

    #[test]
    fn a_strong_brain_contract_is_never_downgraded_by_the_seat_floor() {
        // A backend build step the brain ALREADY gave a specific route probe. The
        // per-seat floor must leave it EXACTLY as-is — augment only the under-specified,
        // never weaken or bolt a redundant ContractMatches onto a volunteered contract.
        let mut s = step_seat(
            "login",
            &[],
            Seat::BackendEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        );
        s.evidence = vec![EvidenceContract::RouteResponds {
            method: "POST".into(),
            path: "/api/login".into(),
            status: Some(200),
        }];
        let p = plan(vec![s]).normalized().expect("usable");
        assert_eq!(
            ev_of(&p, "login"),
            &[EvidenceContract::RouteResponds {
                method: "POST".into(),
                path: "/api/login".into(),
                status: Some(200),
            }],
            "a strong brain contract is preserved unchanged"
        );
    }

    #[test]
    fn a_strong_acceptance_step_is_left_bare_of_added_evidence() {
        // A designer build step whose acceptance IS the design-tokens deliverable already
        // carries a falsifiable contract via its acceptance — the floor adds nothing.
        let p = plan(vec![step_seat(
            "tokens",
            &[],
            Seat::UiuxDesigner,
            StepKind::Build,
            AcceptanceSpec::DesignTokensPresent,
        )])
        .normalized()
        .expect("usable");
        assert!(
            ev_of(&p, "tokens").is_empty(),
            "a strong acceptance is already falsifiable; no evidence is bolted on: {:?}",
            ev_of(&p, "tokens")
        );
    }

    #[test]
    fn unknown_seat_falls_open_to_todays_behavior() {
        // A devops build step (no per-seat floor) alongside a frontend step so the
        // wholesale backstop does NOT trip (only 1 of 2 build steps stays bare). The
        // devops step keeps today's behaviour: no fabricated seat contract.
        let p = plan(vec![
            step_seat(
                "deploy",
                &[],
                Seat::DevopsEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "ui",
                &[],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized()
        .expect("usable");
        assert!(
            ev_of(&p, "deploy").is_empty(),
            "an unknown seat gets no per-seat contract and (below the >50% threshold) no backstop: {:?}",
            ev_of(&p, "deploy")
        );
        // …while its frontend peer still got the per-seat floor.
        assert!(ev_of(&p, "ui").contains(&EvidenceContract::BuildClean));
    }

    #[test]
    fn review_step_is_never_given_an_evidence_floor() {
        // A REVIEW step is judged by its reviewing seat's verdict, not an evidence
        // contract — the floor must leave it untouched even when seated by a doer.
        let p = plan(vec![step_seat(
            "qa-review",
            &[],
            Seat::QaEngineer,
            StepKind::Review,
            AcceptanceSpec::ReviewClean,
        )])
        .normalized()
        .expect("usable");
        assert!(
            ev_of(&p, "qa-review").is_empty(),
            "a review step carries no synthesized evidence: {:?}",
            ev_of(&p, "qa-review")
        );
    }

    #[test]
    fn falsifiability_backstop_fires_when_most_build_steps_are_bare() {
        // A plan the brain left broadly under-specified: two PM build steps (no per-seat
        // floor covers PM) with only the honesty floor. MORE THAN HALF the build steps
        // are bare → the backstop synthesizes a deterministic BuildClean bar on each.
        let p = plan(vec![
            step_seat(
                "prd",
                &[],
                Seat::ProductManager,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "scope",
                &[],
                Seat::ProductManager,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized()
        .expect("usable");
        for id in ["prd", "scope"] {
            assert!(
                ev_of(&p, id).contains(&EvidenceContract::BuildClean),
                "{id} gains a falsifiability floor when the plan is broadly under-specified: {:?}",
                ev_of(&p, id)
            );
        }
    }

    #[test]
    fn falsifiability_backstop_does_not_fire_when_the_plan_is_well_specified() {
        // When at most half the build steps are bare after the per-seat floor, the
        // backstop stays quiet — one bare PM step alongside a contracted backend step is
        // exactly half, below the STRICT >50% threshold, so the PM step stays as-is.
        let p = plan(vec![
            step_seat(
                "api",
                &[],
                Seat::BackendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "prd",
                &[],
                Seat::ProductManager,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized()
        .expect("usable");
        // Backend got its per-seat contract; the lone bare PM step is exactly half →
        // no wholesale backstop, so it keeps today's behaviour (empty evidence).
        assert!(ev_of(&p, "api").contains(&EvidenceContract::ContractMatches));
        assert!(
            ev_of(&p, "prd").is_empty(),
            "a single bare step at exactly half must NOT trip the >50% backstop: {:?}",
            ev_of(&p, "prd")
        );
    }

    // ── BOUNDED RE-PLAN: stranded_dependents + merge_replan + parse_brain_steps ──

    /// Set a status helper for the re-plan tests.
    fn mark(p: &mut Plan, id: &str, s: StepStatus) {
        assert!(p.mark(id, s), "step {id} must exist");
    }

    #[test]
    fn stranded_dependents_are_the_pending_downstream_cone() {
        // a → b → c, plus an independent d. Block a: b and c are stranded (Pending
        // downstream); d is not (independent). Done/Active dependents are not stranded.
        let mut p = plan(vec![
            step("a", &[]),
            step("b", &["a"]),
            step("c", &["b"]),
            step("d", &[]),
        ]);
        let mut stranded = p.stranded_dependents("a");
        stranded.sort();
        assert_eq!(stranded, vec!["b".to_string(), "c".to_string()]);
        // A leaf (nothing depends on it) strands nothing → no re-plan is warranted.
        assert!(p.stranded_dependents("c").is_empty());
        assert!(p.stranded_dependents("d").is_empty());
        // An unknown id fails open to empty.
        assert!(p.stranded_dependents("nope").is_empty());
        // A dependent that already reached Done is NOT stranded (it ran already).
        mark(&mut p, "b", StepStatus::Done);
        assert_eq!(p.stranded_dependents("a"), vec!["c".to_string()]);
    }

    #[test]
    fn merge_replan_splices_a_subdag_through_normalized_and_preserves_survivor_status() {
        // scaffold(Done) → api(BLOCKED) → ui(Pending). Replace {api, ui} with a fresh
        // route {api2, ui2} that depends on the surviving scaffold.
        let mut p = plan(vec![
            step("scaffold", &[]),
            step("api", &["scaffold"]),
            step("ui", &["api"]),
        ]);
        mark(&mut p, "scaffold", StepStatus::Done);
        mark(&mut p, "api", StepStatus::Blocked);
        let replaced: HashSet<String> = ["api".to_string(), "ui".to_string()].into_iter().collect();
        let new_steps = vec![step("api2", &["scaffold"]), step("ui2", &["api2"])];
        assert!(p.merge_replan(&replaced, new_steps));
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        // The blocked subtree is gone; the fresh route is spliced in.
        assert!(ids.contains(&"api2") && ids.contains(&"ui2"));
        assert!(!ids.contains(&"api") && !ids.contains(&"ui"));
        // The surviving Done step KEEPS its status (normalized's Pending-reset was undone).
        assert_eq!(
            p.steps.iter().find(|s| s.id == "scaffold").unwrap().status,
            StepStatus::Done
        );
        // The new steps are Pending (fresh work), and the dangling dep on `scaffold` held
        // (it's a survivor, so the edge is NOT stripped) → api2 depends_on scaffold.
        assert_eq!(
            p.steps.iter().find(|s| s.id == "api2").unwrap().status,
            StepStatus::Pending
        );
        assert_eq!(deps_of(&p, "api2"), &["scaffold".to_string()]);
    }

    #[test]
    fn merge_replan_rejects_a_no_op_subdag_that_adds_no_new_id() {
        // The brain re-emits ONLY existing ids (no genuinely-new route) → no-op → reject.
        let mut p = plan(vec![step("a", &[]), step("b", &["a"])]);
        mark(&mut p, "a", StepStatus::Blocked);
        let before = p.clone();
        let replaced: HashSet<String> = ["a".to_string()].into_iter().collect();
        // Only "b" (an existing id) is offered — nothing new.
        assert!(!p.merge_replan(&replaced, vec![step("b", &[])]));
        assert_eq!(p, before, "a no-op sub-DAG must leave the plan UNCHANGED");
        // An empty sub-DAG is also a no-op.
        assert!(!p.merge_replan(&replaced, vec![]));
        assert_eq!(p, before);
    }

    #[test]
    fn parse_brain_steps_is_fail_open_and_tolerant() {
        // A bare JSON object parses; a re-plan reply wrapped in prose is re-extracted.
        let good = r#"{"steps":[{"id":"x","title":"do x","seat":"backend-engineer",
            "kind":"build","depends_on":["scaffold"],"acceptance":"contract"}]}"#;
        let steps = parse_brain_steps(good);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, "x");
        assert_eq!(steps[0].seat, Seat::BackendEngineer);
        assert_eq!(steps[0].depends_on, vec!["scaffold".to_string()]);
        // Prose around the object is tolerated (re-extraction).
        let wrapped = format!("here is the plan:\n{good}\nthanks");
        assert_eq!(parse_brain_steps(&wrapped).len(), 1);
        // Unparseable / no steps → EMPTY (fail-open, nothing merged).
        assert!(parse_brain_steps("not json at all").is_empty());
        assert!(parse_brain_steps(r#"{"steps":[]}"#).is_empty());
    }
}
